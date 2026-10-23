//! Observations (10.3, 10.4 step 2): recorded on a change, never overwritten by a stale fetch.

mod common;

use chrono::{DateTime, Utc};
use common::{ISSUE_A, t, temp_root};
use issue_dashboard_core::Root;
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::ledger;
use issue_dashboard_core::observe::{self, ItemKind, Observation, Observed};
use serde_json::{Value, json};

fn content(labels: &[&str]) -> Value {
    json!({
        "state": "open",
        "labels": labels,
        "assignees": [],
        "links": {"parent": null, "sub_issues": [], "blocked_by": [], "blocking": []},
        "timeline_ids": [],
    })
}

fn obs(labels: &[&str], updated: DateTime<Utc>, read: DateTime<Utc>) -> Observation {
    Observation {
        node_id: ISSUE_A.into(),
        item: ItemKind::Issue,
        alias: "example-org/server#403".into(),
        updated_at: updated,
        read_at: read,
        read_id: format!("read-{}", read.timestamp()),
        content: content(labels),
    }
}

fn record(root: &Root, o: &Observation) -> Observed {
    root.mutate(o.read_at, |tx| observe::record(tx, o)).unwrap()
}

fn recorded_labels(root: &Root) -> Vec<Value> {
    root.read(|l| ledger::events_of(l.conn(), ISSUE_A, Kind::Observed))
        .unwrap()
        .iter()
        .map(|e| e.data["content"]["labels"].clone())
        .collect()
}

#[test]
fn a_stale_observation_arriving_after_a_fresher_one_is_discarded() {
    let r = temp_root();
    // Two projects' cycles read the same issue. The second read (fresher, updatedAt t(10))
    // commits first; the first read (updatedAt t(5)) commits after it.
    let fresh = obs(&["bug", "p1"], t(10), t(12));
    let stale = obs(&["bug"], t(5), t(11));
    assert!(matches!(record(&r.root, &fresh), Observed::Recorded(_)));
    assert!(matches!(record(&r.root, &stale), Observed::Stale { .. }), "the stale fetch was recorded");
    assert_eq!(recorded_labels(&r.root), [json!(["bug", "p1"])]);
}

#[test]
fn with_equal_update_times_the_earlier_read_is_the_stale_one() {
    let r = temp_root();
    // A change that does not move updatedAt (10.3), read by two cycles at 11 and 12.
    let later_read = obs(&["bug", "renamed"], t(5), t(12));
    let earlier_read = obs(&["bug"], t(5), t(11));
    record(&r.root, &later_read);
    assert!(matches!(record(&r.root, &earlier_read), Observed::Stale { .. }));
    assert_eq!(recorded_labels(&r.root), [json!(["bug", "renamed"])]);
}

#[test]
fn a_change_that_does_not_move_updated_at_is_still_recorded() {
    let r = temp_root();
    record(&r.root, &obs(&["bug"], t(5), t(10)));
    assert!(matches!(record(&r.root, &obs(&["bug", "renamed"], t(5), t(20))), Observed::Recorded(_)));
    assert_eq!(recorded_labels(&r.root).len(), 2);
}

#[test]
fn an_unchanged_observation_is_not_recorded_but_a_to_b_to_a_is() {
    let r = temp_root();
    record(&r.root, &obs(&["bug"], t(1), t(2)));
    assert!(matches!(record(&r.root, &obs(&["bug"], t(1), t(3))), Observed::Unchanged(_)));
    assert!(matches!(record(&r.root, &obs(&["bug", "p0"], t(4), t(5))), Observed::Recorded(_)));
    // The label is removed again: the content returns to an earlier state.
    assert!(matches!(record(&r.root, &obs(&["bug"], t(6), t(7))), Observed::Recorded(_)));
    assert_eq!(recorded_labels(&r.root), [json!(["bug"]), json!(["bug", "p0"]), json!(["bug"])]);
}

#[test]
fn observed_events_validate_against_their_schema() {
    let r = temp_root();
    record(&r.root, &obs(&["bug"], t(1), t(2)));
    let events = r.root.read(|l| ledger::events_by_kind(l.conn(), Kind::Observed)).unwrap();
    assert_eq!(events.len(), 1);
    common::assert_valid("event-observed", &serde_json::to_value(&events[0]).unwrap());
}

#[test]
fn item_gone_is_recorded_once_per_confirming_read() {
    let r = temp_root();
    let gone = |read_id: &str| {
        r.root
            .mutate(t(0), |tx| observe::item_gone(tx, ISSUE_A, "example-org/server#403", read_id, t(0)))
            .unwrap()
            .is_new()
    };
    assert!(gone("full-1"));
    assert!(!gone("full-1"));
    assert!(gone("full-2"));
    let events = r.root.read(|l| ledger::events_by_kind(l.conn(), Kind::ItemGone)).unwrap();
    common::assert_valid("event-item-gone", &serde_json::to_value(&events[0]).unwrap());
}
