//! Harness phase 1 (harness spec 1.1-1.3, 1.6): per-phase sizes, their consistency rules, and
//! the estimate obligations a full triage's first stage records and `record-suggestions`
//! checks.

mod common;

use common::fixture::*;
use common::triage::*;
use issue_dashboard_core::config::ConfigSet;
use issue_dashboard_core::derive;
use issue_dashboard_core::docs::LaunchKind;
use issue_dashboard_core::estimate;
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::ledger::Dedupe;
use issue_dashboard_core::suggest;
use issue_dashboard_core::triage::Stage;
use serde_json::{Value, json};

const WEEK: i64 = 7 * 24 * 60;

/// A phased estimate of `alias`.
fn est(alias: &str, tier: &str, design: &str, implementation: &str, review: &str) -> Value {
    json!({"item": alias, "sites": ["src/a.rs:1"], "read_at": "abc1234", "tier": tier, "blast": "quick",
        "phases": {
            "design": {"size": design, "reason": "the design"},
            "implementation": {"size": implementation, "reason": "the change"},
            "review": {"size": review, "reason": "the reading"},
        },
        "reason": "the cited site"})
}

fn left_out(alias: &str) -> Value {
    json!({"item": alias, "why": "needs_design", "reason": "the fix needs a decision"})
}

/// `record-suggestions` on exactly `file`: no owed estimate filled in.
fn record_as_is(tr: &Tracked, launch: &str, file: Value, minutes: i64) -> issue_dashboard_core::Result<suggest::Report> {
    let input: suggest::Input = serde_json::from_value(file).unwrap();
    let commits = commits_for(&tr.t, &input, &commit_lookup());
    record_exact(&tr.t, launch, &input, minutes, &commits)
}

fn owed_aliases(p: &issue_dashboard_core::triage::Prepared) -> Vec<String> {
    let mut v: Vec<String> = p.file["estimates"]
        .as_array()
        .unwrap()
        .iter()
        .map(|o| o["item"].as_str().unwrap().to_string())
        .collect();
    v.sort();
    v
}

fn aliases(tr: &Tracked, which: &[usize]) -> Vec<String> {
    let mut v: Vec<String> = which.iter().map(|&i| tr.items[i].1.clone()).collect();
    v.sort();
    v
}

fn triage_of(tr: &Tracked, item: &str, minutes: i64) -> Value {
    let c = ConfigSet::load(tr.t.path()).unwrap();
    let o = tr.t.root.read(|l| derive::derive(l.conn(), &c, "home", 1, at(minutes))).unwrap();
    let i = o.issues.into_iter().find(|i| i["id"] == item).unwrap();
    common::assert_valid("doc-issue", &i);
    i["triage"].clone()
}

#[test]
fn the_consistency_rules_match_the_shared_fixture() {
    let f: Value =
        serde_json::from_str(include_str!("fixtures/estimate-check.json")).unwrap();
    for case in f["cases"].as_array().unwrap() {
        let tier = case["tier"].as_str();
        let design = serde_json::from_value(case["design"].clone()).unwrap();
        let review = serde_json::from_value(case["review"].clone()).unwrap();
        let want: Vec<String> = serde_json::from_value(case["problems"].clone()).unwrap();
        assert_eq!(estimate::check(tier, design, review), want, "{case}");
    }
}

#[test]
fn a_full_triage_owes_an_estimate_per_item_and_record_refuses_one_left_unanswered() {
    let tr = tracked();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items, 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    assert_eq!(p.report.obligations, 3);
    assert_eq!(owed_aliases(&p), aliases(&tr, &[0, 1, 2]));

    let m = refused(record_as_is(&tr, "T1", answer_all(&p), 4));
    for (_, alias) in &tr.items {
        assert!(m.contains(&format!("{alias} owes a phased estimate")), "{m}");
    }
    // Answered in `estimates` or listed in `left_out`, every obligation is met.
    let mut file = answer_all(&p);
    file["estimates"] = json!([est(&tr.items[0].1, "T1", "S", "S", "S"), est(&tr.items[1].1, "T0", "none", "M", "S")]);
    file["left_out"] = json!([left_out(&tr.items[2].1)]);
    let m = refused(record_as_is(&tr, "T1", { let mut f = file.clone(); f["left_out"] = json!([]); f }, 4));
    assert!(m.contains(&format!("{} owes a phased estimate", tr.items[2].1)), "{m}");
    assert!(!m.contains(&format!("{} owes", tr.items[0].1)), "{m}");
    record_as_is(&tr, "T1", file, 4).unwrap();
    let t = triage_of(&tr, &tr.items[1].0, 5);
    assert_eq!(t["estimates"]["phases"]["design"], json!({"size": "none", "reason": "the design"}));
    assert_eq!(t["estimates"]["effort"], Value::Null);
}

#[test]
fn every_consistency_rule_and_an_empty_reason_is_refused_at_record() {
    let tr = tracked();
    let alias = tr.items[0].1.clone();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let file = |e: Value| json!({"backend": BACKEND, "estimates": [e]});
    for (e, why) in [
        (est(&alias, "T0", "S", "S", "S"), "a T0 issue has no design: design is none"),
        (est(&alias, "T1", "none", "S", "S"), "a T1 issue has a design: design is S, M or L"),
        (est(&alias, "T2", "S", "S", "S"), "a T2 issue's design is at least M"),
        (est(&alias, "T3", "M", "S", "S"), "a T3 issue's review is at least M"),
    ] {
        let m = refused(record_as_is(&tr, "T1", file(e), 3));
        assert!(m.contains(why), "{why}: {m}");
    }
    let mut e = est(&alias, "T1", "S", "S", "S");
    e["phases"]["implementation"]["reason"] = json!(" ");
    let m = refused(record_as_is(&tr, "T1", file(e), 3));
    assert!(m.contains("the implementation size needs a reason"), "{m}");
    // A file in the old shape does not parse: `effort` is no field of an estimate now.
    let mut old = est(&alias, "T1", "S", "S", "S");
    old.as_object_mut().unwrap().remove("phases");
    old["effort"] = json!("S");
    assert!(serde_json::from_value::<suggest::Input>(file(old)).is_err());
    record_as_is(&tr, "T1", file(est(&alias, "T3", "M", "S", "M")), 4).unwrap();
}

#[test]
fn another_launch_s_estimate_does_not_answer_this_launch_s_obligation() {
    let tr = tracked();
    launched(&tr.t, "A", LaunchKind::Triage, &tr.items[..1], 2);
    launched(&tr.t, "B", LaunchKind::Triage, &tr.items[..1], 3);
    let pa = prepare(&tr.t, "A", BACKEND, Stage::First, 4).unwrap();
    let pb = prepare(&tr.t, "B", BACKEND, Stage::First, 5).unwrap();
    assert_eq!((pa.report.obligations, pb.report.obligations), (1, 1));
    let mut file = answer_all(&pb);
    file["estimates"] = json!([est(&tr.items[0].1, "T1", "S", "S", "S")]);
    record_as_is(&tr, "B", file, 6).unwrap();
    let m = refused(record_as_is(&tr, "A", answer_all(&pa), 7));
    assert!(m.contains("owes a phased estimate"), "{m}");
}

#[test]
fn a_changed_issue_s_estimate_is_stale_hidden_and_its_item_still_owes_one() {
    let tr = tracked();
    let (b, b_alias) = tr.items[1].clone();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..2], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    edit(&tr.t, &b, 4, |v| v["body"] = json!("Edited after prepare."));
    let mut file = answer_all(&p);
    file["estimates"] = json!([est(&tr.items[0].1, "T1", "S", "S", "S"), est(&b_alias, "T1", "S", "S", "S")]);
    record_as_is(&tr, "T1", file.clone(), 5).unwrap();
    let evs = events(&tr.t, Kind::Suggested);
    assert_eq!(suggestion_of(&evs, &b, "estimates")["stale"], true);
    assert_eq!(suggestion_of(&evs, &tr.items[0].0, "estimates")["stale"], false);
    // A retry of the same file is a repeat, its stale estimate included.
    let rep = record_as_is(&tr, "T1", file.clone(), 6).unwrap();
    assert_eq!(rep.recorded, 0, "{rep:?}");
    assert!(rep.repeats >= 2, "{rep:?}");
    // A re-prepare of the launch does not ask b again: the launch recorded its answer, and
    // the next launch asks it.
    let again = prepare(&tr.t, "T1", BACKEND, Stage::First, 7).unwrap();
    assert_eq!(again.report.obligations, 0, "{}", again.file);
    // The stale estimate is never the effective one.
    assert_eq!(triage_of(&tr, &b, 6).get("estimates"), None);
    assert_eq!(triage_of(&tr, &tr.items[0].0, 6)["estimates"]["tier"], "T1");
    launched(&tr.t, "T2", LaunchKind::Triage, &tr.items[..2], 10);
    let p2 = prepare(&tr.t, "T2", BACKEND, Stage::First, 11).unwrap();
    assert_eq!(owed_aliases(&p2), vec![b_alias]);
}

#[test]
fn left_out_twice_in_a_row_skips_the_item_for_seven_days_and_once_does_not() {
    let tr = tracked();
    let (_, alias) = tr.items[0].clone();
    let items = &tr.items[..1];
    let leave = |launch: &str, minutes: i64| {
        launched(&tr.t, launch, LaunchKind::Triage, items, minutes);
        let p = prepare(&tr.t, launch, BACKEND, Stage::First, minutes + 1).unwrap();
        assert_eq!(owed_aliases(&p), vec![alias.clone()], "{launch}");
        let mut file = answer_all(&p);
        file["left_out"] = json!([left_out(&alias)]);
        record_as_is(&tr, launch, file, minutes + 2).unwrap();
    };
    leave("L1", 2);
    // Left out once: still owed.
    leave("L2", 10);
    // Left out twice in a row: skipped for 7 days from the second, then owed again.
    let owes = |launch: &str, minutes: i64| {
        launched(&tr.t, launch, LaunchKind::Triage, items, minutes);
        prepare(&tr.t, launch, BACKEND, Stage::First, minutes + 1).unwrap().report.obligations
    };
    assert_eq!(owes("L3", 20), 0);
    assert_eq!(owes("L4", 12 + WEEK - 2), 0);
    assert_eq!(owes("L5", 12 + WEEK + 5), 1);
}

#[test]
fn a_legacy_effort_estimate_still_loads_and_its_item_owes_phases() {
    let tr = tracked();
    let (id, alias) = tr.items[0].clone();
    tr.t.root
        .mutate(at(2), |tx| {
            tx.record_valid(
                Kind::Suggested,
                &id,
                json!({"run_id": "OLD", "decision": "estimates"}),
                json!({"project": "home", "run_id": "OLD", "suggestion": {
                    "item": id, "alias": alias, "decision": "estimates",
                    "estimate": {"sites": ["src/a.rs:1"], "read_at": "abc1234", "tier": "T1", "blast": "quick", "effort": "S"},
                    "backend": BACKEND, "evaluated": false, "reason": "from before phases"}}),
                Dedupe::Any,
            )
        })
        .unwrap();
    let t = triage_of(&tr, &id, 3);
    assert_eq!(t["estimates"]["effort"], "S");
    assert_eq!(t["estimates"]["phases"], Value::Null);
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 4);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 5).unwrap();
    assert_eq!(owed_aliases(&p), vec![alias]);
}

#[test]
fn an_item_owing_only_an_estimate_is_prepared_with_no_label_question() {
    let tr = tracked();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items, 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    let mut file = answer_all(&p);
    file["left_out"] = json!(tr.items.iter().map(|(_, a)| left_out(a)).collect::<Vec<_>>());
    record_as_is(&tr, "T1", file, 4).unwrap();
    launched(&tr.t, "T2", LaunchKind::Triage, &tr.items, 10);
    let p2 = prepare(&tr.t, "T2", BACKEND, Stage::First, 11).unwrap();
    assert_eq!(p2.report.questions, 0, "{}", p2.file);
    assert_eq!(owed_aliases(&p2), aliases(&tr, &[0, 1, 2]));
    // An event with obligations and no question is recorded: `prepare` records through
    // `record_valid`, so its schema accepted it.
    assert!(p2.report.seq.is_some());
    let mut file = json!({"backend": BACKEND});
    file["estimates"] = json!(tr.items.iter().map(|(_, a)| est(a, "T1", "S", "S", "S")).collect::<Vec<_>>());
    record_as_is(&tr, "T2", file, 12).unwrap();
}

#[test]
fn a_closures_run_and_a_stronger_look_owe_no_estimate() {
    let tr = tracked();
    launched(&tr.t, "R1", LaunchKind::RecommendClosures, &tr.items, 2);
    let p = prepare(&tr.t, "R1", BACKEND, Stage::First, 3).unwrap();
    assert_eq!(p.report.obligations, 0);
    assert_eq!(p.file["estimates"], json!([]));
    launched(&tr.t, "S1", LaunchKind::StrongerLook, &tr.items, 4);
    let p = prepare(&tr.t, "S1", BACKEND, Stage::First, 5).unwrap();
    assert_eq!(p.report.obligations, 0);
    // The priority stage of a full triage owes nothing either.
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items, 6);
    let first = prepare(&tr.t, "T1", BACKEND, Stage::First, 7).unwrap();
    // Left out once, so each item is still estimate-due when the priority stage prepares.
    let mut file = answer_all(&first);
    file["left_out"] = json!(tr.items.iter().map(|(_, a)| left_out(a)).collect::<Vec<_>>());
    record_as_is(&tr, "T1", file, 8).unwrap();
    let pr = prepare(&tr.t, "T1", BACKEND, Stage::Priority, 9).unwrap();
    assert_eq!(pr.report.obligations, 0);
}
