//! The cycle (10.4): ingest of page documents, derive and the outbox, and the reader lease.

mod common;

use std::sync::mpsc;
use std::time::Duration as StdDuration;

use common::fixture::*;
use common::*;
use issue_dashboard_core::cycle::{self, CycleReport};
use issue_dashboard_core::docs::{Action, Decision, DecisionValue, LaunchRequest, Priority, Target};
use issue_dashboard_core::ingest::Pages;
use issue_dashboard_core::launch::{self, Reservation, State};
use issue_dashboard_core::root::Options;
use issue_dashboard_core::{Error, ledger};
use serde_json::{Value, json};

fn pages(decisions: &[Decision], launches: &[LaunchRequest]) -> Pages {
    Pages {
        decisions: decisions.iter().map(|d| serde_json::to_value(d).unwrap()).collect(),
        launches: launches.iter().map(|l| serde_json::to_value(l).unwrap()).collect(),
        ..Pages::default()
    }
}

fn run(t: &TempRoot, p: &Pages, minutes: i64) -> CycleReport {
    cycle::cycle(&t.root, &gh(), "home", p, &move || at(minutes), PAGE).unwrap()
}

fn doc(r: &CycleReport) -> Value {
    serde_json::from_str(&std::fs::read_to_string(&r.outbox).unwrap()).unwrap()
}

fn ack<'a>(d: &'a Value, id: &str) -> &'a Value {
    d["acks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["decision"] == id)
        .unwrap_or_else(|| panic!("no ack for {id}: {}", d["acks"]))
}

fn issue<'a>(d: &'a Value, id: &str) -> &'a Value {
    d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == id)
        .unwrap_or_else(|| panic!("no issue {id}"))
}

/// Every document of an outbox against its schema.
fn assert_outbox_valid(d: &Value) {
    assert_valid("doc-meta", &d["meta"]);
    for (stem, key) in [
        ("doc-issue", "issues"),
        ("doc-acks", "acks"),
        ("doc-launch-status", "launch_status"),
    ] {
        for x in d[key].as_array().unwrap() {
            assert_valid(stem, x);
        }
    }
}

fn cancel(id: &str, launch: &str) -> Decision {
    decision(
        id,
        OWNER,
        Action::CancelLaunch,
        Target {
            launch: Some(launch.into()),
            ..Default::default()
        },
        None,
    )
}

fn state(t: &TempRoot, id: &str) -> State {
    t.root.read(|l| launch::load(l.conn(), id)).unwrap().unwrap().state
}

/// A Store issue the home project tracks, after one cycle.
fn tracked(t: &TempRoot) -> String {
    let r = run(t, &no_pages(), 0);
    doc(&r)["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["alias"].as_str().unwrap().starts_with("yuvalrakavy/Store#") && i["state"] == "open")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string()
}

#[test]
fn an_undo_arriving_before_its_target_still_cancels_it() {
    let t = fixture_root();
    let x = tracked(&t);
    let set = set_priority("D_set", &x, Priority::P0);
    run(&t, &pages(&[undo("D_undo", "D_set")], &[]), 10);
    let r = run(&t, &pages(&[set], &[]), 20);
    let d = doc(&r);
    assert_eq!(ack(&d, "D_set")["result"], "cancelled");
    assert_eq!(ack(&d, "D_undo")["result"], "applied");
    assert_ne!(
        issue(&d, &x)["priority"]["value"],
        "p0",
        "an undone decision has no effect"
    );
}

#[test]
fn a_decision_by_anyone_but_the_owner_is_refused() {
    let t = fixture_root();
    let x = tracked(&t);
    let mut d = set_priority("D_intruder", &x, Priority::P0);
    d.author = "user_someone_else".into();
    let r = run(&t, &pages(&[d], &[]), 10);
    assert_eq!(r.ingest.refused, 1);
    let out = doc(&r);
    assert_eq!(ack(&out, "D_intruder")["result"], "refused");
    assert!(ack(&out, "D_intruder")["reason"].as_str().unwrap().contains("owner_id"));
    assert_ne!(issue(&out, &x)["priority"]["value"], "p0");
}

#[test]
fn a_launch_by_anyone_but_the_owner_is_skipped() {
    let t = fixture_root();
    let x = tracked(&t);
    let mut l = launch_doc("L_intruder", "home", &[x.as_str()], "lanes");
    l.author = "user_someone_else".into();
    let r = run(&t, &pages(&[], &[l]), 10);
    assert_eq!(r.ingest.launches, 0);
    assert_eq!(r.ingest.skipped.len(), 1, "{:?}", r.ingest.skipped);
    assert!(t.root.read(|l| launch::load(l.conn(), "L_intruder")).unwrap().is_none());
}

#[test]
fn a_cancel_arriving_before_its_launch_withdraws_it() {
    let t = fixture_root();
    let x = tracked(&t);
    run(&t, &pages(&[cancel("D_cancel", "L_one")], &[]), 10);
    let r = run(
        &t,
        &pages(&[], &[launch_doc("L_one", "home", &[x.as_str()], "lanes")]),
        20,
    );
    assert_eq!(r.ingest.withdrawn, vec!["L_one".to_string()]);
    assert_eq!(state(&t, "L_one"), State::Withdrawn);
    assert_eq!(ack(&doc(&r), "D_cancel")["result"], "applied");
}

#[test]
fn a_cancel_and_its_launch_in_one_fetch_withdraw_it() {
    let t = fixture_root();
    let x = tracked(&t);
    let r = run(
        &t,
        &pages(
            &[cancel("D_cancel", "L_one")],
            &[launch_doc("L_one", "home", &[x.as_str()], "lanes")],
        ),
        10,
    );
    assert_eq!(state(&t, "L_one"), State::Withdrawn);
    assert_eq!(ack(&doc(&r), "D_cancel")["result"], "applied");
}

#[test]
fn a_cancel_of_a_reserved_launch_is_refused() {
    let t = fixture_root();
    let x = tracked(&t);
    run(
        &t,
        &pages(&[], &[launch_doc("L_one", "home", &[x.as_str()], "lanes")]),
        10,
    );
    let res = t
        .root
        .mutate(at(11), |tx| launch::reserve(tx, "L_one", "dispatcher-1", &|_| Ok(1)))
        .unwrap();
    assert!(matches!(res, Reservation::Reserved { .. }), "{res:?}");
    let r = run(&t, &pages(&[cancel("D_cancel", "L_one")], &[]), 20);
    assert_eq!(state(&t, "L_one"), State::Reserved);
    let a = ack(&doc(&r), "D_cancel").clone();
    assert_eq!(a["result"], "refused");
    assert!(a["reason"].as_str().unwrap().contains("reserved"), "{a}");
}

#[test]
fn an_undo_of_a_cancel_that_already_withdrew_its_launch_is_too_late() {
    let t = fixture_root();
    let x = tracked(&t);
    run(
        &t,
        &pages(
            &[cancel("D_cancel", "L_one")],
            &[launch_doc("L_one", "home", &[x.as_str()], "lanes")],
        ),
        10,
    );
    let r = run(&t, &pages(&[undo("D_undo", "D_cancel")], &[]), 20);
    assert_eq!(state(&t, "L_one"), State::Withdrawn, "a withdrawal is final");
    let d = doc(&r);
    assert_eq!(ack(&d, "D_undo")["result"], "refused");
    assert!(ack(&d, "D_undo")["reason"].as_str().unwrap().contains("too late"));
    assert_eq!(ack(&d, "D_cancel")["result"], "applied");
}

#[test]
fn outbox_sequences_rise_and_acks_are_cumulative() {
    let t = fixture_root();
    let x = tracked(&t);
    let a = run(&t, &pages(&[set_priority("D_a", &x, Priority::P1)], &[]), 10);
    let b = run(&t, &pages(&[], &[]), 20);
    let c = run(&t, &pages(&[set_priority("D_c", &x, Priority::P2)], &[]), 30);
    assert!(a.sequence < b.sequence && b.sequence < c.sequence);
    let (da, dc) = (doc(&a), doc(&c));
    assert_eq!(ack(&da, "D_a")["cycle"], a.sequence);
    // Each outbox carries every ack of the last 30 days, not only its own cycle's.
    assert_eq!(ack(&dc, "D_a")["cycle"], a.sequence);
    assert_eq!(ack(&dc, "D_c")["cycle"], c.sequence);
    assert_eq!(issue(&dc, &x)["priority"]["value"], "p2");
    assert_eq!(dc["sequence"], c.sequence);
    for d in [&da, &doc(&b), &dc] {
        assert_outbox_valid(d);
    }
    // An ack older than 30 days is dropped.
    let late = run(&t, &no_pages(), 31 * 24 * 60);
    assert!(doc(&late)["acks"].as_array().unwrap().is_empty());
}

#[test]
fn the_sequence_continues_after_a_rebuild() {
    let t = fixture_root();
    let a = run(&t, &no_pages(), 0);
    t.root.rebuild(at(1), true).unwrap();
    let b = run(&t, &no_pages(), 10);
    assert!(b.sequence > a.sequence, "{} then {}", a.sequence, b.sequence);
    assert_ne!(a.outbox, b.outbox);
}

#[test]
fn rebuild_waits_for_a_reader_and_refuses_while_it_holds_the_lease() {
    let t = fixture_root_with(Options {
        lock_wait: StdDuration::from_millis(200),
        ..Options::default()
    });
    run(&t, &no_pages(), 0);
    let (held, release) = (mpsc::channel(), mpsc::channel::<()>());
    std::thread::scope(|s| {
        let root = &t.root;
        let (held_tx, release_rx) = (held.0, release.1);
        s.spawn(move || {
            root.read(|_| {
                held_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                Ok(())
            })
            .unwrap();
        });
        held.1.recv().unwrap();
        let r = t.root.rebuild(at(1), true);
        // Release the reader before asserting, so a failure fails rather than hangs.
        release.0.send(()).unwrap();
        assert!(matches!(r, Err(Error::ReadersActive { .. })), "{r:?}");
    });
    t.root.rebuild(at(2), true).unwrap();
    // A cycle's derive holds the lease too, from inside its read transaction.
    let (held, release) = (mpsc::channel(), mpsc::channel::<()>());
    std::thread::scope(|s| {
        let root = &t.root;
        let (held_tx, release_rx) = (held.0, release.1);
        s.spawn(move || {
            root.mutate_then_read(
                || at(3),
                |_| Ok(()),
                |_, ()| {
                    held_tx.send(()).unwrap();
                    release_rx.recv().unwrap();
                    Ok(())
                },
            )
            .unwrap();
        });
        held.1.recv().unwrap();
        let r = t.root.rebuild(at(4), true);
        // Release the reader before asserting, so a failure fails rather than hangs.
        release.0.send(()).unwrap();
        assert!(matches!(r, Err(Error::ReadersActive { .. })), "{r:?}");
    });
    t.root.rebuild(at(5), true).unwrap();
}

#[test]
fn derive_reads_the_ledger_as_of_its_own_commit() {
    let t = fixture_root();
    let x = tracked(&t);
    // A decision committed while the derive runs is not in this outbox.
    let r = t
        .root
        .mutate_then_read(
            || at(10),
            |tx| cycle::take_sequence(tx, "home").map(|s| s.0),
            |l, seq| {
                t.root
                    .mutate(at(11), |tx| {
                        issue_dashboard_core::decision::ingest(
                            tx,
                            "home",
                            OWNER,
                            &set_priority("D_late", &x, Priority::P0),
                        )
                    })
                    .unwrap();
                let config = issue_dashboard_core::config::ConfigSet::load(t.path())?;
                issue_dashboard_core::derive::derive(l.conn(), &config, "home", seq, at(10))
            },
        )
        .unwrap();
    assert_ne!(
        r.issues.iter().find(|i| i["id"] == x.as_str()).unwrap()["priority"]["value"],
        "p0"
    );
    let next = run(&t, &no_pages(), 20);
    assert_eq!(issue(&doc(&next), &x)["priority"]["value"], "p0");
}

#[test]
fn an_invalid_page_document_is_skipped_and_the_rest_ingested() {
    let t = fixture_root();
    let x = tracked(&t);
    let mut p = pages(&[set_priority("D_good", &x, Priority::P1)], &[]);
    p.decisions
        .push(json!({"id": "D_bad", "project": "home", "action": "set_priority"}));
    p.views.push(json!({
        "id": "V_mine", "project": "home", "author": OWNER, "created_at": "2026-10-23T08:30:00Z",
        "name": "Mine", "include": [], "exclude": []
    }));
    p.requests.push(json!({
        "id": "R_triage", "project": "home", "author": OWNER, "created_at": "2026-10-23T08:31:00Z",
        "kind": "triage", "items": [x]
    }));
    let r = run(&t, &p, 10);
    assert_eq!(r.ingest.decided, 1);
    assert_eq!((r.ingest.views, r.ingest.requests), (1, 1));
    assert_eq!(r.ingest.skipped.len(), 1, "{:?}", r.ingest.skipped);
    assert_eq!(r.ingest.skipped[0].id.as_deref(), Some("D_bad"));
    // Ingest is idempotent: the same pages again record nothing new.
    let again = run(&t, &p, 20);
    assert_eq!(
        (again.ingest.decided, again.ingest.views, again.ingest.requests),
        (0, 0, 0)
    );
    // A different document under an id already ingested is refused.
    let mut changed = p.clone();
    changed.views[0]["name"] = json!("Renamed");
    let r = run(&t, &changed, 30);
    assert!(
        r.ingest.skipped.iter().any(|s| s.id.as_deref() == Some("V_mine")),
        "{:?}",
        r.ingest.skipped
    );
}

#[test]
fn a_pages_document_with_an_unknown_collection_is_refused() {
    let r = serde_json::from_value::<Pages>(json!({"decisions": [], "launchez": []}));
    assert!(r.is_err());
}

#[test]
fn overrides_and_priorities_reach_the_outbox() {
    let t = fixture_root();
    let x = tracked(&t);
    let mut o = decision(
        "D_override",
        OWNER,
        Action::SetOverride,
        Target {
            item: Some(x.clone()),
            ..Default::default()
        },
        Some(DecisionValue {
            priority: Some(Priority::P3),
            ..Default::default()
        }),
    );
    o.reason = Some("parked".into());
    let r = run(&t, &pages(&[set_priority("D_p", &x, Priority::P0), o], &[]), 10);
    let d = doc(&r);
    assert_eq!(
        issue(&d, &x)["priority"]["value"],
        "p3",
        "an override wins over the project priority"
    );
}

#[test]
fn every_outbox_issue_validates_and_carries_a_status() {
    let t = fixture_root();
    let r = run(&t, &no_pages(), 0);
    let d = doc(&r);
    assert_outbox_valid(&d);
    let issues = d["issues"].as_array().unwrap();
    assert!(issues.len() > 100);
    assert!(
        issues.iter().all(|i| i["status"].is_string()),
        "every issue has a derived status"
    );
    // rhai is declared read-only.
    let rhai = d["meta"]["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["repo"] == RHAI)
        .unwrap();
    assert_eq!(rhai["read_only"]["why"], "declared");
}

#[test]
fn ingest_events_are_in_the_ledger_before_the_outbox() {
    let t = fixture_root();
    let x = tracked(&t);
    let r = run(&t, &pages(&[set_priority("D_x", &x, Priority::P1)], &[]), 10);
    let decided = t
        .root
        .read(|l| ledger::events_of(l.conn(), "D_x", issue_dashboard_core::event::Kind::Decided))
        .unwrap();
    assert_eq!(decided.len(), 1);
    assert!(decided[0].seq < r.sequence);
}
