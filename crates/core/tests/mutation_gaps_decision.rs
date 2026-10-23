//! Guards of the decision ledger no other test removed or inverted (the pre-merge mutation
//! pass): who may repeat, which verdict stands, what undo cannot reach.

mod common;

use common::{ISSUE_A, ISSUE_B, OWNER, set_priority, t, temp_root, undo};
use issue_dashboard_core::Root;
use issue_dashboard_core::decision::{self, DecidedData, Status};
use issue_dashboard_core::docs::{Action, Decision, DecisionValue, Priority, Target};
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::ledger::{Dedupe, Recorded};
use serde_json::json;

fn ingest_as(root: &Root, project: &str, owner: &str, doc: &Decision) -> Recorded {
    root.mutate(t(0), |tx| decision::ingest(tx, project, owner, doc))
        .unwrap()
}

fn status(root: &Root, project: &str, id: &str) -> Option<Status> {
    root.read(|l| decision::statuses(l.conn(), project)).unwrap().remove(id)
}

#[test]
fn a_document_changed_after_it_was_ingested_is_refused() {
    let r = temp_root();
    let doc = set_priority("D1", ISSUE_A, Priority::P1);
    ingest_as(&r.root, "home", OWNER, &doc);
    let mut other = doc.clone();
    other.reason = Some("a different document".into());
    let e = r
        .root
        .mutate(t(1), |tx| decision::ingest(tx, "home", OWNER, &other))
        .unwrap_err();
    assert!(e.to_string().contains("different document"), "{e}");
}

#[test]
fn a_decision_ingested_for_another_project_is_not_the_home_projects_and_does_not_repeat_it() {
    let r = temp_root();
    let doc = set_priority("D1", ISSUE_A, Priority::P1);
    assert!(ingest_as(&r.root, "home", OWNER, &doc).is_new());
    // Another project reads the same page: a refusal of its own, recorded as new even though
    // this document is accepted for home.
    assert!(ingest_as(&r.root, "work", OWNER, &doc).is_new());
    assert!(matches!(status(&r.root, "work", "D1"), Some(Status::Refused { .. })));
    assert_eq!(status(&r.root, "home", "D1"), Some(Status::Applied));
    // A document only another project ingested is not among home's decisions at all.
    let mut elsewhere = set_priority("D2", ISSUE_B, Priority::P1);
    elsewhere.project = "work".into();
    ingest_as(&r.root, "work", OWNER, &elsewhere);
    assert_eq!(status(&r.root, "home", "D2"), None);
    assert!(matches!(status(&r.root, "work", "D2"), Some(Status::Applied)));
}

#[test]
fn a_refusal_is_a_repeat_for_its_own_project_whoever_else_ingested_the_document_since() {
    let r = temp_root();
    let doc = set_priority("D1", ISSUE_A, Priority::P1);
    assert!(
        ingest_as(&r.root, "home", "someone_else", &doc).is_new(),
        "refused: not the owner"
    );
    assert!(
        ingest_as(&r.root, "work", OWNER, &doc).is_new(),
        "refused: not the project"
    );
    assert!(
        !ingest_as(&r.root, "home", "someone_else", &doc).is_new(),
        "home's own refusal is not recorded twice"
    );
    let events = r
        .root
        .read(|l| issue_dashboard_core::ledger::events_by_kind(l.conn(), Kind::Decided))
        .unwrap();
    assert_eq!(events.len(), 2);
}

#[test]
fn a_refused_decision_is_accepted_when_the_owner_is_corrected_and_stays_so() {
    let r = temp_root();
    let doc = set_priority("D1", ISSUE_A, Priority::P2);
    ingest_as(&r.root, "home", "not_the_owner", &doc);
    assert!(matches!(status(&r.root, "home", "D1"), Some(Status::Refused { .. })));
    assert!(ingest_as(&r.root, "home", OWNER, &doc).is_new());
    assert_eq!(status(&r.root, "home", "D1"), Some(Status::Applied));
    let p = r.root.read(|l| decision::project_priorities(l.conn(), "home")).unwrap();
    assert_eq!(p[ISSUE_A], Priority::P2);
    // The owner changes again: the decision already counted, whatever the ingest would say now.
    assert!(!ingest_as(&r.root, "home", "not_the_owner", &doc).is_new());
    assert_eq!(status(&r.root, "home", "D1"), Some(Status::Applied));
}

#[test]
fn an_accepted_verdict_outlives_any_later_event_about_the_document() {
    let r = temp_root();
    let doc = set_priority("D1", ISSUE_A, Priority::P2);
    ingest_as(&r.root, "home", OWNER, &doc);
    // A refusal recorded after the acceptance (as an older version of the ingest could have).
    let data = DecidedData {
        project: "home".into(),
        author: doc.author.clone(),
        accepted: false,
        refusal: Some("late refusal".into()),
        document: doc.clone(),
    };
    r.root
        .mutate(t(1), |tx| {
            tx.record_valid(
                Kind::Decided,
                "D1",
                json!({"project": "home", "accepted": false}),
                serde_json::to_value(&data).unwrap(),
                Dedupe::Latest,
            )
        })
        .unwrap();
    assert_eq!(status(&r.root, "home", "D1"), Some(Status::Applied));
}

#[test]
fn a_cycle_of_undos_cancels_nothing() {
    let r = temp_root();
    ingest_as(&r.root, "home", OWNER, &set_priority("D1", ISSUE_A, Priority::P1));
    ingest_as(&r.root, "home", OWNER, &undo("U1", "U2"));
    ingest_as(&r.root, "home", OWNER, &undo("U2", "U1"));
    assert_eq!(status(&r.root, "home", "U1"), Some(Status::Applied));
    assert_eq!(status(&r.root, "home", "U2"), Some(Status::Applied));
    assert_eq!(status(&r.root, "home", "D1"), Some(Status::Applied));
}

#[test]
fn an_undo_is_never_itself_an_effective_decision() {
    let r = temp_root();
    ingest_as(&r.root, "home", OWNER, &set_priority("D1", ISSUE_A, Priority::P1));
    ingest_as(&r.root, "home", OWNER, &set_priority("D2", ISSUE_B, Priority::P1));
    ingest_as(&r.root, "home", OWNER, &undo("U1", "D2"));
    let ids: Vec<String> = r
        .root
        .read(|l| decision::effective(l.conn(), "home"))
        .unwrap()
        .into_iter()
        .map(|d| d.id)
        .collect();
    assert_eq!(ids, vec!["D1".to_string()]);
}

#[test]
fn a_cleared_override_is_gone_and_a_set_one_carries_its_reason() {
    let r = temp_root();
    let target = |item: &str| Target {
        item: Some(item.into()),
        ..Default::default()
    };
    let value = |p| DecisionValue {
        priority: Some(p),
        ..Default::default()
    };
    let mut set_a = common::decision(
        "D1",
        OWNER,
        Action::SetOverride,
        target(ISSUE_A),
        Some(value(Priority::P0)),
    );
    set_a.reason = Some("customer is waiting".into());
    let mut set_b = common::decision(
        "D2",
        OWNER,
        Action::SetOverride,
        target(ISSUE_B),
        Some(value(Priority::P3)),
    );
    set_b.reason = Some("for now".into());
    let clear_b = common::decision("D3", OWNER, Action::ClearOverride, target(ISSUE_B), None);
    for d in [&set_a, &set_b, &clear_b] {
        ingest_as(&r.root, "home", OWNER, d);
    }
    let o = r.root.read(|l| decision::overrides(l.conn(), "home")).unwrap();
    assert_eq!(o.len(), 1, "{o:?}");
    assert_eq!(o[ISSUE_A].0.priority, Some(Priority::P0));
    assert_eq!(o[ISSUE_A].1.as_deref(), Some("customer is waiting"));
}
