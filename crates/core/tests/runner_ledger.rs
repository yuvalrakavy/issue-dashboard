//! The ledger half of the bridge runner (runner spec 1.4, 2.1, 3.2, 4.1): the claim rule both
//! ways, the claimer filter of `reserve_next`, `release_never_started`, and a runner-owned end
//! from `reserved`.

mod common;

use common::{ISSUE_A, ISSUE_B, launch_doc, t, temp_root};
use issue_dashboard_core::launch::{self, EndState, Reservation, State, Via, RUNNER};
use issue_dashboard_core::ledger::Tx;
use issue_dashboard_core::{Error, Result, Root};
use serde_json::json;

const D1: &str = "local_dispatcher_1";

fn cap(_: &str) -> Result<u32> {
    Ok(1)
}

fn cap2(_: &str) -> Result<u32> {
    Ok(2)
}

fn at<T>(root: &Root, minutes: i64, f: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
    root.mutate(t(minutes), f)
}

/// Requests launch `id` on ISSUE-less recipe `solo`, routed to `harness` when given.
fn request(root: &Root, id: &str, item: &str, harness: Option<&str>) {
    let mut doc = launch_doc(id, "home", &[item], "solo");
    doc.routing.harness = harness.map(str::to_string);
    at(root, 0, |tx| launch::request(tx, &doc, Via::Ingest)).unwrap();
}

fn refused<T: std::fmt::Debug>(r: Result<T>) -> String {
    match r {
        Err(Error::Refused(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
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

#[test]
fn a_harness_routed_launch_is_reserved_only_by_the_runner() {
    let r = temp_root();
    request(&r.root, "L1", ISSUE_A, Some("Codex"));
    let m = refused(at(&r.root, 1, |tx| launch::reserve(tx, "L1", D1, &cap)));
    assert!(m.contains("Codex") && m.contains(RUNNER), "{m}");
    assert_eq!(state(&r.root, "L1"), State::Requested, "a refused claim records nothing");
    let tok = token_of(&at(&r.root, 1, |tx| launch::reserve(tx, "L1", RUNNER, &cap)).unwrap());
    assert!(tok.starts_with("res-"));
}

#[test]
fn a_launch_without_a_harness_is_never_reserved_by_the_runner() {
    let r = temp_root();
    request(&r.root, "L1", ISSUE_A, None);
    let m = refused(at(&r.root, 1, |tx| launch::reserve(tx, "L1", RUNNER, &cap)));
    assert!(m.contains("no harness"), "{m}");
    assert_eq!(state(&r.root, "L1"), State::Requested);
    at(&r.root, 1, |tx| launch::reserve(tx, "L1", D1, &cap)).unwrap();
}

#[test]
fn reserve_next_takes_only_what_its_claimer_may_claim() {
    let r = temp_root();
    // L0 holds the one `solo` slot; L1 (runner's) and L2 (a dispatcher's) queue behind it.
    request(&r.root, "L0", ISSUE_A, None);
    let t0 = token_of(&at(&r.root, 1, |tx| launch::reserve(tx, "L0", D1, &cap)).unwrap());
    request(&r.root, "L1", ISSUE_B, Some("Codex"));
    at(&r.root, 1, |tx| launch::reserve(tx, "L1", RUNNER, &cap)).unwrap();
    request(&r.root, "L2", common::ISSUE_C, None);
    at(&r.root, 1, |tx| launch::reserve(tx, "L2", D1, &cap)).unwrap();
    assert_eq!(state(&r.root, "L1"), State::Queued);
    assert_eq!(state(&r.root, "L2"), State::Queued);
    at(&r.root, 2, |tx| launch::started(tx, &t0, "S0")).unwrap();
    at(&r.root, 3, |tx| launch::ended(tx, &t0, EndState::Landed)).unwrap();
    // FIFO is kept across claimers: L1 is first, so the dispatcher may not take L2 past it…
    assert!(at(&r.root, 4, |tx| launch::reserve_next(tx, D1, &cap)).unwrap().is_none());
    assert_eq!(state(&r.root, "L1"), State::Queued, "the dispatcher never takes a runner launch");
    // …and the runner takes L1.
    let got = at(&r.root, 4, |tx| launch::reserve_next(tx, RUNNER, &cap)).unwrap();
    assert!(matches!(got, Some(Reservation::Reserved { ref launch, .. }) if launch == "L1"), "{got:?}");
}

#[test]
fn release_never_started_frees_a_runner_launch_at_once_with_its_evidence() {
    let r = temp_root();
    request(&r.root, "L1", ISSUE_A, Some("Codex"));
    let tok = token_of(&at(&r.root, 1, |tx| launch::reserve(tx, "L1", RUNNER, &cap)).unwrap());
    let evidence = json!({"state": "error", "never_started": true, "reason": "preflight_rejected_before_send"});
    // No 15-minute wait: one minute after the reservation.
    let m = at(&r.root, 2, |tx| launch::release_never_started(tx, &tok, &evidence)).unwrap();
    assert_eq!((m.state, m.repeat), (State::Released, false));
    let again = at(&r.root, 3, |tx| launch::release_never_started(tx, &tok, &evidence)).unwrap();
    assert!(again.repeat);
    let events = r
        .root
        .read(|l| issue_dashboard_core::ledger::events_of(l.conn(), "L1", issue_dashboard_core::event::Kind::Launch))
        .unwrap();
    assert_eq!(events.last().unwrap().data["evidence"], evidence);
}

#[test]
fn release_never_started_refuses_a_dispatcher_launch_and_a_started_one() {
    let r = temp_root();
    request(&r.root, "L1", ISSUE_A, None);
    let tok = token_of(&at(&r.root, 1, |tx| launch::reserve(tx, "L1", D1, &cap)).unwrap());
    let m = refused(at(&r.root, 2, |tx| launch::release_never_started(tx, &tok, &json!({}))));
    assert!(m.contains(RUNNER), "{m}");
    request(&r.root, "L2", ISSUE_B, Some("Codex"));
    // L1 still holds a slot, so this test's pool has two.
    let t2 = token_of(&at(&r.root, 1, |tx| launch::reserve(tx, "L2", RUNNER, &cap2)).unwrap());
    at(&r.root, 2, |tx| launch::started(tx, &t2, "S2")).unwrap();
    refused(at(&r.root, 3, |tx| launch::release_never_started(tx, &t2, &json!({}))));
    assert_eq!(state(&r.root, "L2"), State::Started);
}

#[test]
fn the_runner_ends_its_own_launch_from_reserved_and_a_dispatcher_cannot() {
    let r = temp_root();
    request(&r.root, "L1", ISSUE_A, Some("Codex"));
    let tok = token_of(&at(&r.root, 1, |tx| launch::reserve(tx, "L1", RUNNER, &cap)).unwrap());
    let ev = json!({"state": "archived", "result": null});
    let m = at(&r.root, 2, |tx| launch::runner_ended(tx, &tok, EndState::Failed, &ev)).unwrap();
    assert_eq!((m.state, m.repeat), (State::Released, false));
    assert!(at(&r.root, 3, |tx| launch::runner_ended(tx, &tok, EndState::Failed, &ev)).unwrap().repeat);
    refused(at(&r.root, 3, |tx| launch::runner_ended(tx, &tok, EndState::Landed, &ev)));

    request(&r.root, "L2", ISSUE_B, None);
    let t2 = token_of(&at(&r.root, 1, |tx| launch::reserve(tx, "L2", D1, &cap)).unwrap());
    let m = refused(at(&r.root, 2, |tx| launch::runner_ended(tx, &t2, EndState::Failed, &ev)));
    assert!(m.contains(RUNNER), "{m}");
    // The dispatcher's own `ended` still refuses a reserved launch, as before.
    refused(at(&r.root, 2, |tx| launch::ended(tx, &t2, EndState::Failed)));
}

#[test]
fn runner_ended_on_a_started_launch_ends_it_like_ended() {
    let r = temp_root();
    request(&r.root, "L1", ISSUE_A, Some("Codex"));
    let tok = token_of(&at(&r.root, 1, |tx| launch::reserve(tx, "L1", RUNNER, &cap)).unwrap());
    at(&r.root, 2, |tx| launch::started(tx, &tok, "S1")).unwrap();
    let m = at(&r.root, 3, |tx| launch::runner_ended(tx, &tok, EndState::Landed, &json!({}))).unwrap();
    assert_eq!(m.state, State::Released);
}
