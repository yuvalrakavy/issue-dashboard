//! The SQLite ledger: append-only events, identity and repeats (10.2, 10.3).

mod common;

use common::{t, temp_root};
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::ledger::{self, Dedupe, Recorded};
use serde_json::json;

#[test]
fn the_ledger_is_in_wal_mode() {
    let r = temp_root();
    r.root.mutate(t(0), |_| Ok(())).unwrap();
    let mode: String = r
        .root
        .read(|l| Ok(l.conn().query_row("PRAGMA journal_mode", [], |row| row.get(0))?))
        .unwrap();
    assert_eq!(mode, "wal");
}

#[test]
fn a_repeat_against_the_latest_is_a_no_op_and_a_to_b_to_a_is_recorded() {
    let r = temp_root();
    let rec = |content: &str| {
        r.root
            .mutate(t(0), |tx| {
                tx.record(
                    Kind::Config,
                    "config",
                    json!({"content_hash": content}),
                    json!({"content": content}),
                    Dedupe::Latest,
                )
            })
            .unwrap()
    };
    let a1 = rec("A");
    let a_again = rec("A");
    let b = rec("B");
    let a2 = rec("A");
    let b_again = rec("B");
    let b2 = rec("B");

    assert!(a1.is_new());
    assert_eq!(
        a_again,
        Recorded::Repeat(a1.event().clone()),
        "a retry against an unchanged latest is a no-op"
    );
    assert!(b.is_new());
    assert!(a2.is_new(), "A -> B -> A is a new occurrence");
    assert_ne!(a2.event().identity, a1.event().identity);
    assert!(b_again.is_new(), "A -> B -> A -> B is a new occurrence too");
    assert!(!b2.is_new());

    assert_eq!(a1.event().pred, 0);
    assert_eq!(b.event().pred, a1.event().seq);
    assert_eq!(a2.event().pred, b.event().seq);

    let configs = r
        .root
        .read(|l| ledger::events_of(l.conn(), "config", Kind::Config))
        .unwrap();
    let contents: Vec<_> = configs
        .iter()
        .map(|e| e.distinct["content_hash"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(contents, ["A", "B", "A", "B"]);
}

#[test]
fn dedupe_any_finds_a_repeat_that_is_not_the_latest() {
    let r = temp_root();
    let rec = |decision: &str| {
        r.root
            .mutate(t(0), |tx| {
                tx.record(
                    Kind::Suggested,
                    "I_item",
                    json!({"run_id": "run1", "decision": decision}),
                    json!({}),
                    Dedupe::Any,
                )
            })
            .unwrap()
    };
    assert!(rec("type").is_new());
    assert!(rec("area").is_new());
    // A retried record-suggestions run: "type" is no longer the latest, but it is a repeat.
    assert!(!rec("type").is_new());
    assert!(!rec("area").is_new());
}

#[test]
fn identity_is_the_hash_of_kind_subject_predecessor_and_distinct_fields() {
    let r = temp_root();
    let e = r
        .root
        .mutate(t(0), |tx| {
            tx.record(
                Kind::Exported,
                "ledger",
                json!({"cursor": 7}),
                json!({"cursor": 7}),
                Dedupe::Latest,
            )
        })
        .unwrap();
    let e = e.event();
    assert_eq!(e.identity, e.computed_identity());
    assert_eq!(e.time, "2026-10-23T08:00:00.000Z");
}

#[test]
fn sequence_numbers_are_contiguous_and_a_failed_command_leaves_no_trace() {
    let r = temp_root();
    for i in 0..3 {
        r.root
            .mutate(t(i), |tx| {
                tx.record(
                    Kind::Config,
                    "config",
                    json!({"content_hash": i}),
                    json!({}),
                    Dedupe::Latest,
                )
            })
            .unwrap();
    }
    // A command that records, then fails, rolls back.
    let failed: issue_dashboard_core::Result<()> = r.root.mutate(t(5), |tx| {
        tx.record(
            Kind::Config,
            "config",
            json!({"content_hash": "doomed"}),
            json!({}),
            Dedupe::Latest,
        )?;
        Err(issue_dashboard_core::Error::refused("a later check failed"))
    });
    assert!(failed.is_err());
    r.root
        .mutate(t(6), |tx| {
            tx.record(
                Kind::Config,
                "config",
                json!({"content_hash": "after"}),
                json!({}),
                Dedupe::Latest,
            )
        })
        .unwrap();

    let events = r.root.read(|l| ledger::events_after(l.conn(), 0)).unwrap();
    let seqs: Vec<i64> = events.iter().map(|e| e.seq).collect();
    let expected: Vec<i64> = (1..=seqs.len() as i64).collect();
    assert_eq!(seqs, expected, "sequence numbers have a gap");
    assert!(events.iter().all(|e| e.distinct["content_hash"] != "doomed"));
}

#[test]
fn events_cannot_be_updated_or_deleted() {
    let r = temp_root();
    r.root
        .mutate(t(0), |tx| {
            tx.record(
                Kind::Config,
                "config",
                json!({"content_hash": "A"}),
                json!({}),
                Dedupe::Latest,
            )
        })
        .unwrap();
    let update = r
        .root
        .read(|l| Ok(l.conn().execute("UPDATE events SET data = '{}'", [])));
    assert!(update.unwrap().is_err(), "an UPDATE of an event was allowed");
    let delete = r.root.read(|l| Ok(l.conn().execute("DELETE FROM events", [])));
    assert!(delete.unwrap().is_err(), "a DELETE of an event was allowed");
}
