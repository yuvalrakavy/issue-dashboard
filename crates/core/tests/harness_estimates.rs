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

// --- Gaps a mutation run found (each test names what it guards) ------------------------------

/// Another launch records stage one, so this launch can prepare its priority stage while its
/// own first-stage obligations are still unanswered. `record` must read the FIRST stage's
/// obligations, not those of the latest prepare of any stage.
#[test]
fn a_later_priority_prepare_does_not_replace_the_first_stage_s_obligations() {
    let tr = tracked();
    launched(&tr.t, "A", LaunchKind::Triage, &tr.items, 2);
    launched(&tr.t, "B", LaunchKind::Triage, &tr.items, 3);
    let pb = prepare(&tr.t, "B", BACKEND, Stage::First, 4).unwrap();
    assert_eq!(pb.report.obligations, 3);
    let pa = prepare(&tr.t, "A", BACKEND, Stage::First, 5).unwrap();
    let mut file = answer_all(&pa);
    file["estimates"] = json!(tr.items.iter().map(|(_, a)| est(a, "T1", "S", "S", "S")).collect::<Vec<_>>());
    record_as_is(&tr, "A", file, 6).unwrap();
    // B's priority stage is askable (the project has stage one), and asks no estimate.
    let pr = prepare(&tr.t, "B", BACKEND, Stage::Priority, 7).unwrap();
    assert_eq!(pr.report.obligations, 0);
    assert!(pr.report.questions > 0, "{}", pr.file);
    // B answers its priority questions but still owes its first stage's estimates.
    let m = refused(record_as_is(&tr, "B", answer_all(&pr), 8));
    for (_, alias) in &tr.items {
        assert!(m.contains(&format!("{alias} owes a phased estimate")), "{m}");
    }
}

/// The prepare digest covers the obligations: same question ids, other obligations, is a new
/// event; the same again is a repeat.
#[test]
fn a_prepare_with_other_obligations_is_not_a_repeat_of_one_with_the_same_questions() {
    let tr = tracked();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items, 2);
    let p1 = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    assert_eq!(p1.report.obligations, 3);
    assert!(!p1.report.repeat);
    let again = prepare(&tr.t, "T1", BACKEND, Stage::First, 4).unwrap();
    assert!(again.report.repeat, "identical questions and obligations are a repeat");
    assert_eq!(again.report.seq, p1.report.seq);
    // Estimates only: no question is answered, so the questions keep their ids.
    let file = json!({"backend": BACKEND,
        "estimates": tr.items.iter().map(|(_, a)| est(a, "T1", "S", "S", "S")).collect::<Vec<_>>()});
    record_as_is(&tr, "T1", file, 5).unwrap();
    let p2 = prepare(&tr.t, "T1", BACKEND, Stage::First, 6).unwrap();
    assert_eq!(p2.report.obligations, 0);
    let ids = |p: &issue_dashboard_core::triage::Prepared| -> Vec<Value> {
        questions(p).iter().map(|q| q["id"].clone()).collect()
    };
    assert_eq!(ids(&p1), ids(&p2), "the fixture must keep the question ids equal");
    assert!(!p2.report.repeat, "{:?}", p2.report);
    assert_ne!(p2.report.seq, p1.report.seq);
    let p3 = prepare(&tr.t, "T1", BACKEND, Stage::First, 7).unwrap();
    assert!(p3.report.repeat);
    assert_eq!(p3.report.seq, p2.report.seq);
}

/// Two prepares of one launch that both ask an item (the issue changed between them) are ONE
/// obligation: one `left_out` of it is "left out once", not twice in a row.
#[test]
fn two_prepares_of_one_launch_are_one_obligation_for_the_left_out_skip() {
    let tr = tracked();
    let (id, alias) = tr.items[0].clone();
    launched(&tr.t, "L1", LaunchKind::Triage, &tr.items, 2);
    prepare(&tr.t, "L1", BACKEND, Stage::First, 3).unwrap();
    edit(&tr.t, &id, 4, |v| v["body"] = json!("Edited between the prepares."));
    let p = prepare(&tr.t, "L1", BACKEND, Stage::First, 5).unwrap();
    assert!(!p.report.repeat, "the edit must make the second prepare a new event");
    let prepared: Vec<_> = events(&tr.t, Kind::Prepared).into_iter().filter(|e| e.subject == "L1").collect();
    assert_eq!(prepared.len(), 2);
    let mut file = answer_all(&p);
    file["left_out"] = json!(tr.items.iter().map(|(_, a)| left_out(a)).collect::<Vec<_>>());
    record_as_is(&tr, "L1", file, 6).unwrap();
    launched(&tr.t, "L2", LaunchKind::Triage, &tr.items, 10);
    let p2 = prepare(&tr.t, "L2", BACKEND, Stage::First, 11).unwrap();
    assert!(owed_aliases(&p2).contains(&alias), "left out once is still owed: {}", p2.file);
}

/// `Owing::due` at the end of a skip: due exactly then, not a moment before.
#[test]
fn a_skip_ends_exactly_seven_days_after_the_second_left_out() {
    let tr = tracked();
    let (_, alias) = tr.items[0].clone();
    let items = &tr.items[..1];
    let leave = |launch: &str, minutes: i64| {
        launched(&tr.t, launch, LaunchKind::Triage, items, minutes);
        let p = prepare(&tr.t, launch, BACKEND, Stage::First, minutes + 1).unwrap();
        let mut file = answer_all(&p);
        file["left_out"] = json!([left_out(&alias)]);
        record_as_is(&tr, launch, file, minutes + 2).unwrap();
    };
    leave("L1", 2);
    leave("L2", 10); // the second left_out is recorded at minute 12
    let owes = |launch: &str, minutes: i64| {
        launched(&tr.t, launch, LaunchKind::Triage, items, 13);
        prepare(&tr.t, launch, BACKEND, Stage::First, minutes).unwrap().report.obligations
    };
    assert_eq!(owes("L3", 12 + WEEK - 1), 0, "a minute before the end");
    assert_eq!(owes("L4", 12 + WEEK), 1, "at the end");
}

/// `select` orders untriaged, then estimate-due, then the rest.
#[test]
fn select_puts_an_estimate_due_item_before_a_triaged_one_that_is_not() {
    use issue_dashboard_core::docs::TriageScope;
    use issue_dashboard_core::root::format_time;
    let tr = tracked();
    let (a, a_alias) = tr.items[0].clone();
    let (b, b_alias) = tr.items[1].clone();
    let (c, c_alias) = tr.items[2].clone();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items, 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    // A: its questions answered, its estimate left out once (so still owed, and triaged).
    // B: an estimate and no answer (not estimate-due, never triaged, questions still due).
    // C: its estimate left out, no answer (estimate-due, never triaged).
    let mut file = answer_all(&p);
    let mine: Vec<Value> = questions(&p)
        .iter()
        .zip(file["answers"].as_array().unwrap())
        .filter(|(q, _)| q["item"] == a_alias.as_str())
        .map(|(_, ans)| ans.clone())
        .collect();
    assert!(!mine.is_empty());
    file["answers"] = json!(mine);
    file["estimates"] = json!([est(&b_alias, "T1", "S", "S", "S")]);
    file["left_out"] = json!([left_out(&a_alias), left_out(&c_alias)]);
    record_as_is(&tr, "T1", file, 4).unwrap();
    let config = ConfigSet::load(tr.t.path()).unwrap();
    let now = format_time(at(20));
    // The fixture's project tracks other issues too: only these three are compared.
    let got: Vec<_> = tr
        .t
        .root
        .read(|l| issue_dashboard_core::triage::select(l.conn(), &config, "home", TriageScope::Full, &now))
        .unwrap()
        .into_iter()
        .filter(|x| [&a, &b, &c].contains(&&x.item))
        .collect();
    assert!(got.iter().all(|x| !x.untriaged), "{got:?}");
    let by = |id: &str| got.iter().find(|x| x.item == id).unwrap_or_else(|| panic!("{id} not selected: {got:?}"));
    assert!(by(&a).estimate_due && by(&a).last_triaged.is_some(), "{got:?}");
    assert!(by(&c).estimate_due && by(&c).last_triaged.is_none(), "{got:?}");
    assert!(!by(&b).estimate_due && by(&b).last_triaged.is_none(), "{got:?}");
    let order: Vec<&str> = got.iter().map(|x| x.item.as_str()).collect();
    assert_eq!(order, vec![c.as_str(), a.as_str(), b.as_str()], "{got:?}");
}

/// 1.7: the owner's edited size wins over the recorded one.
#[test]
fn an_edited_review_size_wins_over_the_recorded_one() {
    use issue_dashboard_core::docs::EstimatesEdit;
    use issue_dashboard_core::estimate::{Effective, PhasesEdit, Size};
    let recorded = |review: &str| {
        json!({"tier": "T3", "phases": {
            "design": {"size": "M", "reason": "r"},
            "implementation": {"size": "M", "reason": "r"},
            "review": {"size": review, "reason": "r"}}})
    };
    let edit = |review: Size| EstimatesEdit {
        phases: Some(PhasesEdit { review: Some(review), ..PhasesEdit::default() }),
        ..EstimatesEdit::default()
    };
    // Recorded L, edited S: a T3 review of S is inconsistent.
    let e = Effective::of(Some(&recorded("L")), Some(&edit(Size::S)));
    assert_eq!(e.review, Some(Size::S));
    assert_eq!(e.problems(), vec!["a T3 issue's review is at least M".to_string()]);
    // Recorded S (inconsistent alone), edited M: the edit wins and the estimate is consistent.
    let e = Effective::of(Some(&recorded("S")), Some(&edit(Size::M)));
    assert_eq!(e.review, Some(Size::M));
    assert!(e.problems().is_empty(), "{:?}", e.problems());
    // The unedited phases stay the recorded ones.
    assert_eq!(e.implementation, Some(Size::M));
}
