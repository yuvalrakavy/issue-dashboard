//! Launch reservations (10.7): the transition table enforced exactly, idempotent retries,
//! orphans alongside a launch, ownership, the 15-minute rule, slots and the queue.

mod common;

use common::{ISSUE_A, ISSUE_B, ISSUE_C, launch_doc, t, temp_root};
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::launch::{self, EndState, Reservation, StartResult, State, Via};
use issue_dashboard_core::ledger::{self, Tx};
use issue_dashboard_core::{Error, Result, Root};

const D1: &str = "local_dispatcher_1";
const D2: &str = "local_dispatcher_2";

fn cap(recipe: &str) -> Result<u32> {
    match recipe {
        "lanes" => Ok(2),
        "solo" => Ok(1),
        other => Err(Error::refused(format!("no recipe {other}"))),
    }
}

fn at<T>(root: &Root, minutes: i64, f: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
    root.mutate(t(minutes), f)
}

fn request(root: &Root, id: &str, items: &[&str], recipe: &str) {
    at(root, 0, |tx| {
        launch::request(tx, &launch_doc(id, "home", items, recipe), Via::Ingest)
    })
    .unwrap();
}

fn reserve(root: &Root, id: &str, dispatcher: &str) -> Result<Reservation> {
    at(root, 1, |tx| launch::reserve(tx, id, dispatcher, &cap))
}

fn token_of(r: &Reservation) -> String {
    match r {
        Reservation::Reserved { token, .. } => token.clone(),
        other => panic!("expected a reservation, got {other:?}"),
    }
}

fn state(root: &Root, id: &str) -> State {
    root.read(|l| launch::load(l.conn(), id)).unwrap().unwrap().state
}

fn launch_events(root: &Root, id: &str) -> usize {
    root.read(|l| ledger::events_of(l.conn(), id, Kind::Launch))
        .unwrap()
        .len()
}

fn refused<T: std::fmt::Debug>(r: Result<T>) -> String {
    match r {
        Err(Error::Refused(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A launch reserved by D1 with its token.
fn reserved(root: &Root, id: &str, items: &[&str]) -> String {
    request(root, id, items, "lanes");
    token_of(&reserve(root, id, D1).unwrap())
}

/// A launch started by session S1.
fn started(root: &Root, id: &str, items: &[&str]) -> String {
    let token = reserved(root, id, items);
    at(root, 2, |tx| launch::started(tx, &token, "S1")).unwrap();
    token
}

#[test]
fn the_happy_path_requested_reserved_started_ended_released() {
    let r = temp_root();
    let token = reserved(&r.root, "L1", &[ISSUE_A]);
    assert!(token.starts_with("res-") && token.len() == 36, "{token}");
    assert_eq!(state(&r.root, "L1"), State::Reserved);
    let s = at(&r.root, 2, |tx| launch::started(tx, &token, "S1")).unwrap();
    assert_eq!(
        s,
        StartResult::Started {
            launch: "L1".into(),
            repeat: false
        }
    );
    let m = at(&r.root, 3, |tx| launch::ended(tx, &token, EndState::Landed)).unwrap();
    assert_eq!(m.state, State::Released);
    let l = r.root.read(|l| launch::load(l.conn(), "L1")).unwrap().unwrap();
    assert_eq!(l.end_state, Some(EndState::Landed));
    assert_eq!(l.session.as_deref(), Some("S1"));
    // requested, reserved, started, ended, released
    assert_eq!(launch_events(&r.root, "L1"), 5);
}

#[test]
fn a_retried_launch_started_is_a_no_op_returning_the_recorded_result() {
    let r = temp_root();
    let token = started(&r.root, "L1", &[ISSUE_A]);
    let before = launch_events(&r.root, "L1");
    let again = at(&r.root, 3, |tx| launch::started(tx, &token, "S1")).unwrap();
    assert_eq!(
        again,
        StartResult::Started {
            launch: "L1".into(),
            repeat: true
        }
    );
    assert_eq!(launch_events(&r.root, "L1"), before);
    // Even after the launch ended, the same session retrying is not an orphan.
    at(&r.root, 4, |tx| launch::ended(tx, &token, EndState::Stopped)).unwrap();
    let late = at(&r.root, 5, |tx| launch::started(tx, &token, "S1")).unwrap();
    assert_eq!(
        late,
        StartResult::Started {
            launch: "L1".into(),
            repeat: true
        }
    );
    let l = r.root.read(|l| launch::load(l.conn(), "L1")).unwrap().unwrap();
    assert!(l.orphans.is_empty());
}

#[test]
fn a_different_session_for_a_started_token_is_an_orphan_alongside_the_launch() {
    let r = temp_root();
    let token = started(&r.root, "L1", &[ISSUE_A]);
    let o = at(&r.root, 3, |tx| launch::started(tx, &token, "S2")).unwrap();
    assert_eq!(
        o,
        StartResult::Orphan {
            launch: "L1".into(),
            repeat: false
        }
    );
    let l = r.root.read(|l| launch::load(l.conn(), "L1")).unwrap().unwrap();
    assert_eq!(l.state, State::Started, "an orphan is not the launch's state");
    assert_eq!(l.session.as_deref(), Some("S1"));
    assert_eq!(l.orphans.len(), 1);
    assert_eq!(l.orphans[0].session, "S2");
    // Reported again: still one orphan.
    let o2 = at(&r.root, 4, |tx| launch::started(tx, &token, "S2")).unwrap();
    assert_eq!(
        o2,
        StartResult::Orphan {
            launch: "L1".into(),
            repeat: true
        }
    );
    let l = r.root.read(|l| launch::load(l.conn(), "L1")).unwrap().unwrap();
    assert_eq!(l.orphans.len(), 1);
}

#[test]
fn a_session_reporting_a_released_token_is_an_orphan() {
    let r = temp_root();
    let token = reserved(&r.root, "L1", &[ISSUE_A]);
    at(&r.root, 20, |tx| launch::release(tx, &token, D1)).unwrap();
    let o = at(&r.root, 21, |tx| launch::started(tx, &token, "S9")).unwrap();
    assert_eq!(
        o,
        StartResult::Orphan {
            launch: "L1".into(),
            repeat: false
        }
    );
    assert_eq!(state(&r.root, "L1"), State::Released);
}

#[test]
fn an_orphan_holds_a_slot_until_settled() {
    let r = temp_root();
    // "solo" has one slot.
    request(&r.root, "L1", &[ISSUE_A], "solo");
    let token = token_of(&reserve(&r.root, "L1", D1).unwrap());
    at(&r.root, 20, |tx| launch::release(tx, &token, D1)).unwrap();
    at(&r.root, 21, |tx| launch::started(tx, &token, "S9")).unwrap(); // orphan
    request(&r.root, "L2", &[ISSUE_B], "solo");
    let q = reserve(&r.root, "L2", D1).unwrap();
    assert_eq!(
        q,
        Reservation::Queued {
            launch: "L2".into(),
            repeat: false
        },
        "the orphan's slot was handed out"
    );
    at(&r.root, 22, |tx| launch::settle_orphan(tx, &token, "S9")).unwrap();
    let next = at(&r.root, 23, |tx| launch::reserve_next(tx, D1, &cap)).unwrap();
    assert!(
        matches!(next, Some(Reservation::Reserved { ref launch, .. }) if launch == "L2"),
        "{next:?}"
    );
}

#[test]
fn settling_an_orphan_that_does_not_exist_is_refused() {
    let r = temp_root();
    let token = started(&r.root, "L1", &[ISSUE_A]);
    refused(at(&r.root, 3, |tx| launch::settle_orphan(tx, &token, "S1")));
}

#[test]
fn unconfirmed_then_started_and_unconfirmed_then_released() {
    let r = temp_root();
    let t1 = reserved(&r.root, "L1", &[ISSUE_A]);
    let m = at(&r.root, 2, |tx| launch::unconfirm(tx, &t1, D1)).unwrap();
    assert_eq!(m.state, State::Unconfirmed);
    let again = at(&r.root, 2, |tx| launch::unconfirm(tx, &t1, D1)).unwrap();
    assert!(again.repeat);
    at(&r.root, 3, |tx| launch::started(tx, &t1, "S1")).unwrap();
    assert_eq!(state(&r.root, "L1"), State::Started);

    let t2 = reserved(&r.root, "L2", &[ISSUE_B]);
    at(&r.root, 2, |tx| launch::unconfirm(tx, &t2, D1)).unwrap();
    at(&r.root, 17, |tx| launch::release(tx, &t2, D1)).unwrap();
    assert_eq!(state(&r.root, "L2"), State::Released);
}

#[test]
fn only_the_owning_dispatcher_settles_a_reservation() {
    let r = temp_root();
    let token = reserved(&r.root, "L1", &[ISSUE_A]);
    let m = refused(at(&r.root, 30, |tx| launch::release(tx, &token, D2)));
    assert!(m.contains(D1), "the refusal should name the owner: {m}");
    refused(at(&r.root, 30, |tx| launch::unconfirm(tx, &token, D2)));
    assert_eq!(state(&r.root, "L1"), State::Reserved);
}

#[test]
fn a_reservation_is_released_only_once_older_than_fifteen_minutes() {
    let r = temp_root();
    let token = reserved(&r.root, "L1", &[ISSUE_A]); // reserved at t(1)
    let m = refused(at(&r.root, 16, |tx| launch::release(tx, &token, D1)));
    assert!(m.contains("15"), "{m}");
    assert_eq!(state(&r.root, "L1"), State::Reserved);
    at(&r.root, 17, |tx| launch::release(tx, &token, D1)).unwrap();
    assert_eq!(state(&r.root, "L1"), State::Released);
    let again = at(&r.root, 18, |tx| launch::release(tx, &token, D1)).unwrap();
    assert!(again.repeat);
}

#[test]
fn illegal_transitions_are_refused_and_record_nothing() {
    let r = temp_root();
    // started: cannot be released by a dispatcher or unconfirmed or withdrawn.
    let token = started(&r.root, "L1", &[ISSUE_A]);
    let n = launch_events(&r.root, "L1");
    refused(at(&r.root, 30, |tx| launch::release(tx, &token, D1)));
    refused(at(&r.root, 30, |tx| launch::unconfirm(tx, &token, D1)));
    refused(at(&r.root, 30, |tx| launch::withdraw(tx, "L1", "D_x")));
    // reserved: cannot end (reserved -> ended is not in the table).
    let t2 = reserved(&r.root, "L2", &[ISSUE_B]);
    refused(at(&r.root, 30, |tx| launch::ended(tx, &t2, EndState::Landed)));
    // released: cannot be reserved again, withdrawn or ended.
    at(&r.root, 31, |tx| launch::ended(tx, &token, EndState::Failed)).unwrap();
    let n_released = launch_events(&r.root, "L1");
    refused(reserve(&r.root, "L1", D1));
    refused(at(&r.root, 32, |tx| launch::withdraw(tx, "L1", "D_x")));
    // an unknown token or launch is refused.
    refused(at(&r.root, 32, |tx| {
        launch::started(tx, "res-00000000000000000000000000000000", "S1")
    }));
    refused(reserve(&r.root, "L_unknown", D1));
    assert_eq!(launch_events(&r.root, "L1"), n_released);
    assert!(n_released == n + 2, "ended + released only");
}

#[test]
fn launch_ended_retried_is_a_no_op_and_a_different_end_state_is_refused() {
    let r = temp_root();
    let token = started(&r.root, "L1", &[ISSUE_A]);
    at(&r.root, 3, |tx| launch::ended(tx, &token, EndState::Landed)).unwrap();
    let n = launch_events(&r.root, "L1");
    let again = at(&r.root, 4, |tx| launch::ended(tx, &token, EndState::Landed)).unwrap();
    assert!(again.repeat);
    refused(at(&r.root, 4, |tx| launch::ended(tx, &token, EndState::Failed)));
    assert_eq!(launch_events(&r.root, "L1"), n);
}

#[test]
fn reserve_retried_returns_the_same_token_and_another_dispatcher_is_refused() {
    let r = temp_root();
    let token = reserved(&r.root, "L1", &[ISSUE_A]);
    let again = reserve(&r.root, "L1", D1).unwrap();
    assert_eq!(
        again,
        Reservation::Reserved {
            launch: "L1".into(),
            token: token.clone(),
            repeat: true
        }
    );
    refused(reserve(&r.root, "L1", D2));
}

#[test]
fn an_issue_held_by_a_launch_in_any_project_refuses_another_reservation() {
    let r = temp_root();
    reserved(&r.root, "L1", &[ISSUE_A]);
    // Another project's batch that includes the same issue.
    at(&r.root, 0, |tx| {
        launch::request(tx, &launch_doc("L2", "work", &[ISSUE_B, ISSUE_A], "lanes"), Via::Ingest)
    })
    .unwrap();
    let m = refused(reserve(&r.root, "L2", D1));
    assert!(m.contains(ISSUE_A), "{m}");
    assert_eq!(state(&r.root, "L2"), State::Requested, "a refusal changes nothing");
}

#[test]
fn a_held_issue_is_free_again_once_its_launch_is_released() {
    let r = temp_root();
    let token = started(&r.root, "L1", &[ISSUE_A]);
    at(&r.root, 3, |tx| launch::ended(tx, &token, EndState::Landed)).unwrap();
    request(&r.root, "L2", &[ISSUE_A], "lanes");
    assert!(matches!(
        reserve(&r.root, "L2", D1).unwrap(),
        Reservation::Reserved { .. }
    ));
}

#[test]
fn a_full_pool_queues_and_the_queue_is_first_in_first_out() {
    let r = temp_root();
    reserved(&r.root, "L1", &[ISSUE_A]);
    let t2 = reserved(&r.root, "L2", &[ISSUE_B]); // "lanes" has two slots: now full
    request(&r.root, "L3", &[ISSUE_C], "lanes");
    assert_eq!(
        reserve(&r.root, "L3", D1).unwrap(),
        Reservation::Queued {
            launch: "L3".into(),
            repeat: false
        }
    );
    assert_eq!(
        reserve(&r.root, "L3", D1).unwrap(),
        Reservation::Queued {
            launch: "L3".into(),
            repeat: true
        }
    );
    request(&r.root, "L4", &["I_kwDOissueD"], "lanes");
    assert!(matches!(
        reserve(&r.root, "L4", D1).unwrap(),
        Reservation::Queued { .. }
    ));
    assert!(
        at(&r.root, 2, |tx| launch::reserve_next(tx, D1, &cap))
            .unwrap()
            .is_none(),
        "no slot is free"
    );

    // A slot frees: the later queued launch may not jump the earlier one.
    at(&r.root, 3, |tx| launch::started(tx, &t2, "S2")).unwrap();
    at(&r.root, 4, |tx| launch::ended(tx, &t2, EndState::Landed)).unwrap();
    assert!(
        matches!(reserve(&r.root, "L4", D1).unwrap(), Reservation::Queued { .. }),
        "L4 jumped the queue"
    );
    let next = at(&r.root, 5, |tx| launch::reserve_next(tx, D1, &cap)).unwrap();
    assert!(
        matches!(next, Some(Reservation::Reserved { ref launch, .. }) if launch == "L3"),
        "{next:?}"
    );
    assert_eq!(state(&r.root, "L3"), State::Reserved);
    assert_eq!(state(&r.root, "L4"), State::Queued);
}

#[test]
fn a_new_request_queues_behind_an_earlier_queued_launch_even_with_a_free_slot() {
    let r = temp_root();
    reserved(&r.root, "L1", &[ISSUE_A]);
    let t2 = reserved(&r.root, "L2", &[ISSUE_B]);
    request(&r.root, "L3", &[ISSUE_C], "lanes");
    reserve(&r.root, "L3", D1).unwrap(); // queued
    at(&r.root, 3, |tx| launch::started(tx, &t2, "S2")).unwrap();
    at(&r.root, 4, |tx| launch::ended(tx, &t2, EndState::Landed)).unwrap();
    request(&r.root, "L5", &["I_kwDOissueE"], "lanes");
    assert!(
        matches!(reserve(&r.root, "L5", D1).unwrap(), Reservation::Queued { .. }),
        "L5 took the slot L3 waits for"
    );
}

#[test]
fn withdraw_only_from_requested_or_queued() {
    let r = temp_root();
    request(&r.root, "L1", &[ISSUE_A], "lanes");
    let m = at(&r.root, 1, |tx| launch::withdraw(tx, "L1", "D_cancel")).unwrap();
    assert_eq!(m.state, State::Withdrawn);
    assert!(
        at(&r.root, 1, |tx| launch::withdraw(tx, "L1", "D_cancel"))
            .unwrap()
            .repeat
    );
    refused(reserve(&r.root, "L1", D1));

    reserved(&r.root, "L2", &[ISSUE_B]);
    refused(at(&r.root, 2, |tx| launch::withdraw(tx, "L2", "D_cancel")));
}

#[test]
fn requested_by_reserve_then_ingested_is_one_event_and_a_changed_document_is_refused() {
    let r = temp_root();
    let doc = launch_doc("L1", "home", &[ISSUE_A], "lanes");
    let first = at(&r.root, 0, |tx| launch::request(tx, &doc, Via::Reserve)).unwrap();
    assert!(!first.repeat);
    let again = at(&r.root, 1, |tx| launch::request(tx, &doc, Via::Ingest)).unwrap();
    assert!(again.repeat, "the later ingest of the same document must be a no-op");
    assert_eq!(launch_events(&r.root, "L1"), 1);
    let mut changed = doc.clone();
    changed.items = vec![ISSUE_B.into()];
    refused(at(&r.root, 2, |tx| launch::request(tx, &changed, Via::Ingest)));
    // Even after the launch moved on, its request is still the same document.
    reserve(&r.root, "L1", D1).unwrap();
    assert!(
        at(&r.root, 3, |tx| launch::request(tx, &doc, Via::Ingest))
            .unwrap()
            .repeat
    );
}

#[test]
fn every_launch_event_validates_against_the_launch_event_schema() {
    let r = temp_root();
    let token = started(&r.root, "L1", &[ISSUE_A]);
    at(&r.root, 3, |tx| launch::started(tx, &token, "S2")).unwrap();
    at(&r.root, 4, |tx| launch::settle_orphan(tx, &token, "S2")).unwrap();
    at(&r.root, 5, |tx| launch::ended(tx, &token, EndState::Landed)).unwrap();
    let t2 = reserved(&r.root, "L2", &[ISSUE_B]);
    at(&r.root, 6, |tx| launch::unconfirm(tx, &t2, D1)).unwrap();
    request(&r.root, "L3", &[ISSUE_C], "solo");
    at(&r.root, 7, |tx| launch::withdraw(tx, "L3", "D_c")).unwrap();
    let events = r.root.read(|l| ledger::events_by_kind(l.conn(), Kind::Launch)).unwrap();
    assert!(events.len() >= 11);
    let mut transitions = std::collections::BTreeSet::new();
    for e in &events {
        common::assert_valid("event-launch", &serde_json::to_value(e).unwrap());
        transitions.insert(e.data["transition"].as_str().unwrap().to_string());
    }
    assert_eq!(
        transitions.len(),
        9,
        "not every transition was exercised: {transitions:?}"
    );
}

// ---- mutation-test additions ----

#[test]
fn settling_one_orphan_leaves_the_other_orphans_of_the_launch_holding_their_slots() {
    let r = temp_root();
    request(&r.root, "L1", &[ISSUE_A], "solo");
    let token = token_of(&reserve(&r.root, "L1", D1).unwrap());
    at(&r.root, 20, |tx| launch::release(tx, &token, D1)).unwrap();
    at(&r.root, 21, |tx| launch::started(tx, &token, "S9")).unwrap(); // orphan
    at(&r.root, 21, |tx| launch::started(tx, &token, "S8")).unwrap(); // a second orphan
    request(&r.root, "L2", &[ISSUE_B], "solo");
    assert!(matches!(
        reserve(&r.root, "L2", D1).unwrap(),
        Reservation::Queued { .. }
    ));

    at(&r.root, 22, |tx| launch::settle_orphan(tx, &token, "S9")).unwrap();
    let next = at(&r.root, 23, |tx| launch::reserve_next(tx, D1, &cap)).unwrap();
    assert!(
        next.is_none(),
        "S8 still holds the slot, but it was handed out: {next:?}"
    );

    at(&r.root, 24, |tx| launch::settle_orphan(tx, &token, "S8")).unwrap();
    let next = at(&r.root, 25, |tx| launch::reserve_next(tx, D1, &cap)).unwrap();
    assert!(
        matches!(next, Some(Reservation::Reserved { ref launch, .. }) if launch == "L2"),
        "{next:?}"
    );
}

#[test]
fn reserve_next_takes_the_launch_that_queued_first_not_the_one_requested_first() {
    let r = temp_root();
    let t1 = reserved(&r.root, "L1", &[ISSUE_A]);
    let t2 = reserved(&r.root, "L2", &[ISSUE_B]); // "lanes" is now full
    // L3 is requested before L4, but L4 reaches the queue first.
    request(&r.root, "L3", &[ISSUE_C], "lanes");
    request(&r.root, "L4", &["I_kwDOissueD"], "lanes");
    assert!(matches!(
        reserve(&r.root, "L4", D1).unwrap(),
        Reservation::Queued { .. }
    ));
    assert!(matches!(
        reserve(&r.root, "L3", D1).unwrap(),
        Reservation::Queued { .. }
    ));

    // Both slots free at once, so only the order decides who gets the one reserve_next hands out.
    for (token, session) in [(&t1, "S1"), (&t2, "S2")] {
        at(&r.root, 3, |tx| launch::started(tx, token, session)).unwrap();
        at(&r.root, 4, |tx| launch::ended(tx, token, EndState::Landed)).unwrap();
    }
    let next = at(&r.root, 5, |tx| launch::reserve_next(tx, D1, &cap)).unwrap();
    assert!(
        matches!(next, Some(Reservation::Reserved { ref launch, .. }) if launch == "L4"),
        "the queue is first in first out by when launches queued: {next:?}"
    );
}
