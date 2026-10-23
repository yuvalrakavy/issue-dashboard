//! Page decisions (10.5): only the owner's count, undo works whichever arrives first, and
//! decisions apply in sequence order.

mod common;

use std::collections::BTreeMap;

use common::{ISSUE_A, ISSUE_B, OWNER, set_priority, t, temp_root, undo};
use issue_dashboard_core::Root;
use issue_dashboard_core::decision::{self, Status};
use issue_dashboard_core::docs::{Decision, Priority};

fn ingest(root: &Root, doc: &Decision) -> bool {
    root.mutate(t(0), |tx| decision::ingest(tx, "home", OWNER, doc)).unwrap().is_new()
}

fn priorities(root: &Root) -> BTreeMap<String, Priority> {
    root.read(|l| decision::project_priorities(l.conn(), "home")).unwrap()
}

fn statuses(root: &Root) -> BTreeMap<String, Status> {
    root.read(|l| decision::statuses(l.conn(), "home")).unwrap()
}

#[test]
fn a_decision_by_someone_other_than_the_owner_is_recorded_as_refused() {
    let r = temp_root();
    let mut doc = set_priority("D1", ISSUE_A, Priority::P0);
    doc.author = "user_intruder".into();
    assert!(ingest(&r.root, &doc));
    assert!(priorities(&r.root).is_empty(), "a non-owner's decision took effect");
    match &statuses(&r.root)["D1"] {
        Status::Refused { reason } => assert!(reason.contains("user_intruder"), "{reason}"),
        other => panic!("{other:?}"),
    }
    // The refused event validates against its schema, accepted = false.
    let events = r.root.read(|l| issue_dashboard_core::ledger::events_by_kind(l.conn(), issue_dashboard_core::Kind::Decided)).unwrap();
    assert_eq!(events[0].data["accepted"], false);
    common::assert_valid("event-decided", &serde_json::to_value(&events[0]).unwrap());
}

#[test]
fn a_decision_for_another_project_is_refused() {
    let r = temp_root();
    let mut doc = set_priority("D1", ISSUE_A, Priority::P0);
    doc.project = "work".into();
    ingest(&r.root, &doc);
    assert!(matches!(statuses(&r.root)["D1"], Status::Refused { .. }));
}

#[test]
fn ingesting_the_same_decision_twice_is_a_no_op() {
    let r = temp_root();
    let doc = set_priority("D1", ISSUE_A, Priority::P1);
    assert!(ingest(&r.root, &doc));
    assert!(!ingest(&r.root, &doc));
    ingest(&r.root, &set_priority("D2", ISSUE_B, Priority::P2));
    assert!(!ingest(&r.root, &doc), "a repeat that is no longer the latest decision");
}

#[test]
fn the_later_of_two_decisions_about_one_item_wins() {
    let r = temp_root();
    ingest(&r.root, &set_priority("D1", ISSUE_A, Priority::P1));
    ingest(&r.root, &set_priority("D2", ISSUE_A, Priority::P3));
    ingest(&r.root, &set_priority("D3", ISSUE_B, Priority::P0));
    let p = priorities(&r.root);
    assert_eq!(p[ISSUE_A], Priority::P3);
    assert_eq!(p[ISSUE_B], Priority::P0);
}

#[test]
fn undo_after_its_target_cancels_it() {
    let r = temp_root();
    ingest(&r.root, &set_priority("D1", ISSUE_A, Priority::P1));
    ingest(&r.root, &set_priority("D2", ISSUE_A, Priority::P3));
    ingest(&r.root, &undo("U2", "D2"));
    assert_eq!(priorities(&r.root)[ISSUE_A], Priority::P1, "undoing the later decision restores the earlier one");
    assert_eq!(statuses(&r.root)["D2"], Status::Cancelled { by: "U2".into() });
}

#[test]
fn undo_before_its_target_cancels_it_the_moment_it_arrives() {
    let r = temp_root();
    ingest(&r.root, &undo("U1", "D1"));
    assert!(matches!(statuses(&r.root)["U1"], Status::Applied));
    ingest(&r.root, &set_priority("D1", ISSUE_A, Priority::P0));
    assert!(priorities(&r.root).is_empty(), "the undone decision took effect");
    assert_eq!(statuses(&r.root)["D1"], Status::Cancelled { by: "U1".into() });
}

#[test]
fn arrival_order_of_a_decision_and_its_undo_does_not_matter() {
    let docs = [set_priority("D0", ISSUE_A, Priority::P2), set_priority("D1", ISSUE_A, Priority::P0), undo("U1", "D1")];
    let forward = temp_root();
    for d in &docs {
        ingest(&forward.root, d);
    }
    let backward = temp_root();
    for d in docs.iter().rev() {
        ingest(&backward.root, d);
    }
    assert_eq!(priorities(&forward.root), priorities(&backward.root));
    assert_eq!(priorities(&forward.root)[ISSUE_A], Priority::P2);
}

#[test]
fn an_undo_by_a_non_owner_cancels_nothing() {
    let r = temp_root();
    ingest(&r.root, &set_priority("D1", ISSUE_A, Priority::P1));
    let mut u = undo("U1", "D1");
    u.author = "user_intruder".into();
    ingest(&r.root, &u);
    assert_eq!(priorities(&r.root)[ISSUE_A], Priority::P1);
    assert_eq!(statuses(&r.root)["D1"], Status::Applied);
}

#[test]
fn undoing_an_undo_restores_the_decision() {
    let r = temp_root();
    ingest(&r.root, &set_priority("D1", ISSUE_A, Priority::P1));
    ingest(&r.root, &undo("U1", "D1"));
    ingest(&r.root, &undo("U2", "U1"));
    assert_eq!(priorities(&r.root)[ISSUE_A], Priority::P1);
}

#[test]
fn a_cleared_priority_removes_the_item() {
    use issue_dashboard_core::docs::{Action, Target};
    let r = temp_root();
    ingest(&r.root, &set_priority("D1", ISSUE_A, Priority::P1));
    let clear = common::decision("D2", OWNER, Action::ClearPriority, Target { item: Some(ISSUE_A.into()), ..Default::default() }, None);
    ingest(&r.root, &clear);
    assert!(!priorities(&r.root).contains_key(ISSUE_A));
}
