//! Triage phase 3/5, the page (triage spec ch. 4 and 6.1's rows for it): label cards, the
//! combined card, second opinions and their cap, badges, the accepted-suggestion layer in
//! derive, a stale accept, apply all's gating, and Details edits.
//!
//! The fixture is three open Store issues labelled bug, p1, kernel (`tracked`); the first is
//! stripped of its labels where a test needs an untriaged item.

mod common;

use std::collections::BTreeMap;

use common::fixture::*;
use common::triage::*;
use common::*;
use issue_dashboard_core::config::ConfigSet;
use issue_dashboard_core::decision::{self, DecidedData};
use issue_dashboard_core::derive;
use issue_dashboard_core::docs::{Action, Decision, DecisionValue, LaunchKind, Priority, Target};
use issue_dashboard_core::eval;
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::ledger;
use issue_dashboard_core::root::format_time;
use issue_dashboard_core::suggest;
use issue_dashboard_core::triage::{Catalogue, Prepared, Stage};
use issue_dashboard_core::truth;
use serde_json::{Value, json};

const MAP: &str = "00000000000000aa";

fn config(t: &TempRoot) -> ConfigSet {
    ConfigSet::load(t.path()).unwrap()
}

/// `answer_all`'s answers, with `f(item, decision)` giving the probabilities where it answers.
fn answer_with(p: &Prepared, f: impl Fn(&str, &str) -> Option<Vec<(&'static str, f64)>>) -> Value {
    let mut v = answer_all(p);
    let qs = questions(p).clone();
    for (q, a) in qs.iter().zip(v["answers"].as_array_mut().unwrap()) {
        if let Some(probs) = f(q["item"].as_str().unwrap(), q["decision"].as_str().unwrap()) {
            let m: BTreeMap<&str, f64> = probs.into_iter().collect();
            a["probabilities"] = json!(m);
        }
    }
    v
}

fn type_probs(top: &'static str, p: f64) -> Vec<(&'static str, f64)> {
    let rest = (1.0 - p) / 3.0;
    ["bug", "todo", "decision", "docs"]
        .into_iter()
        .map(|a| (a, if a == top { p } else { rest }))
        .collect()
}

/// `bug` at `p_bug`, `todo` taking the rest but a little.
fn against_bug(p_bug: f64) -> Vec<(&'static str, f64)> {
    vec![
        ("bug", p_bug),
        ("todo", 0.98 - p_bug),
        ("decision", 0.01),
        ("docs", 0.01),
    ]
}

fn priority_probs(top: &'static str, p: f64) -> Vec<(&'static str, f64)> {
    let rest = (1.0 - p) / 3.0;
    ["p0", "p1", "p2", "p3"]
        .into_iter()
        .map(|a| (a, if a == top { p } else { rest }))
        .collect()
}

fn against_p1(p_p1: f64) -> Vec<(&'static str, f64)> {
    vec![("p0", 0.01), ("p1", p_p1), ("p2", 0.01), ("p3", 0.98 - p_p1)]
}

/// Strips `item`'s labels: untriaged on type, priority and area.
fn untriage(tr: &Tracked, item: &str, minutes: i64) {
    edit(&tr.t, item, minutes, |c| c["labels"] = json!([]));
}

/// An `evaluated` event for the type key, writing map [`MAP`] at T 1, its bars from n 120, ECE
/// 0.05 and `accuracy`.
fn evaluate_type(t: &TempRoot, minutes: i64, accuracy: f64) {
    evaluate_type_at(t, minutes, accuracy, 0.05);
}

/// [`evaluate_type`] at ECE `ece`.
fn evaluate_type_at(t: &TempRoot, minutes: i64, accuracy: f64, ece: f64) {
    let hash = Catalogue::embedded().question_hash("type", &[]).unwrap();
    let data = eval::EvaluatedData {
        project: "home".into(),
        backend: BACKEND.into(),
        decision: "type".into(),
        question_hash: hash.clone(),
        mode: "replay".into(),
        map_id: Some(MAP.into()),
        uncalibratable: false,
        t: 1.0,
        n: 120,
        n_tuning: 80,
        accuracy,
        ece,
        ece_interval: [0.03, 0.08],
        classwise_ece: BTreeMap::new(),
        false_alarm_rate: Some(0.04),
        exclusions: Vec::new(),
        bars: eval::bars(120, ece, accuracy),
        unanswered: 0,
        by_source: BTreeMap::new(),
    };
    let subject = eval::key_subject("home", BACKEND, "type", &hash);
    t.root
        .mutate(at(minutes), |tx| {
            tx.record_valid(
                Kind::Evaluated,
                &subject,
                json!({"manifest": "live", "answers": format!("{minutes:064}")}),
                serde_json::to_value(&data)?,
                ledger::Dedupe::Latest,
            )
        })
        .unwrap();
}

/// A triage run T1 over the tracked items, both stages answered by `stage1` and `stage2`.
fn run(
    tr: &Tracked,
    stage1: impl Fn(&str, &str) -> Option<Vec<(&'static str, f64)>>,
    stage2: impl Fn(&str, &str) -> Option<Vec<(&'static str, f64)>>,
) {
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items, 10);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 11).unwrap();
    record(&tr.t, "T1", answer_with(&p, stage1), 12).unwrap();
    let p = prepare(&tr.t, "T1", BACKEND, Stage::Priority, 13).unwrap();
    record(&tr.t, "T1", answer_with(&p, stage2), 14).unwrap();
}

fn docs(t: &TempRoot) -> Vec<Value> {
    let c = config(t);
    let d = t.root.read(|l| suggest::documents(l.conn(), &c, "home")).unwrap();
    for x in &d {
        assert_valid("doc-suggestion", x);
    }
    d
}

fn card<'a>(docs: &'a [Value], item: &str, kind: &str) -> Option<&'a Value> {
    docs.iter().find(|d| d["item"] == item && d["kind"] == kind)
}

/// The suggestion document id of the latest `decision` suggestion about `item`.
fn suggestion_id(t: &TempRoot, item: &str, decision: &str) -> String {
    let found = t.root.read(|l| truth::suggestions(l.conn(), "home")).unwrap();
    found
        .iter()
        .filter(|(_, s)| s.item == item && s.decision == decision)
        .max_by_key(|(_, s)| s.at)
        .map(|(d, _)| d.clone())
        .unwrap_or_else(|| panic!("no {decision} suggestion of {item}"))
}

fn on(id: &str, action: Action, target: Target, value: Option<DecisionValue>, minutes: i64) -> Decision {
    let mut d = decision(id, OWNER, action, target, value);
    d.created_at = format_time(at(minutes));
    d
}

fn accept(id: &str, suggestion: &str, bulk: bool, minutes: i64) -> Decision {
    on(
        id,
        Action::AcceptSuggestion,
        Target {
            suggestion: Some(suggestion.into()),
            ..Default::default()
        },
        bulk.then(|| DecisionValue {
            bulk: Some(true),
            ..Default::default()
        }),
        minutes,
    )
}

/// Ingests `d` with the page checks, as a cycle does; the refusal, if any.
fn ingest(t: &TempRoot, d: &Decision, minutes: i64) -> Option<String> {
    let c = config(t);
    let r = t
        .root
        .mutate(at(minutes), |tx| decision::ingest_checked(tx, &c, "home", OWNER, d))
        .unwrap();
    let data: DecidedData = serde_json::from_value(r.event().data.clone()).unwrap();
    data.refusal
}

fn issue(t: &TempRoot, item: &str) -> Value {
    let c = config(t);
    let o = t
        .root
        .read(|l| derive::derive(l.conn(), &c, "home", 1, at(90)))
        .unwrap();
    let i = o.issues.into_iter().find(|i| i["id"] == item).unwrap();
    assert_valid("doc-issue", &i);
    i
}

// --- Cards (4.1) ----------------------------------------------------------------------------

#[test]
fn an_untriaged_item_gets_one_combined_card_and_a_labelled_one_only_disagreements() {
    let tr = tracked();
    let (a, b, c) = (tr.items[0].0.clone(), tr.items[1].0.clone(), tr.items[2].0.clone());
    untriage(&tr, &a, 5);
    run(
        &tr,
        |item, d| match (d, item == tr.items[1].1) {
            ("type", true) => Some(against_bug(0.05)),
            ("type", false) => Some(type_probs("bug", 0.85)),
            _ => None,
        },
        |item, _| {
            Some(if item == tr.items[1].1 {
                against_p1(0.05)
            } else {
                priority_probs("p1", 0.85)
            })
        },
    );
    let d = docs(&tr.t);
    // a: type and priority share one card, "Set bug · p1"; its area is a card of its own.
    let (t, p) = (card(&d, &a, "type").unwrap(), card(&d, &a, "priority").unwrap());
    assert_eq!(t["card"], p["card"]);
    assert!(t["card"].as_str().unwrap().starts_with("cd-"));
    assert_ne!(t["card"], t["id"]);
    assert_eq!(t["action_label"], "Set bug · p1");
    assert_eq!(t["current"], Value::Null);
    assert!(t.get("second_opinion").is_none());
    assert!((t["confidence"].as_f64().unwrap() - 0.85).abs() < 1e-9);
    let area = card(&d, &a, "area").unwrap();
    assert_eq!(area["current"], json!([]));
    assert_eq!(area["card"], area["id"]);
    // b: second opinions on type and priority, naming the current label.
    let bt = card(&d, &b, "type").unwrap();
    assert_eq!(bt["second_opinion"]["current"], "bug");
    assert_eq!(bt["action_label"], "Change to todo");
    assert_eq!(card(&d, &b, "priority").unwrap()["second_opinion"]["current"], "p1");
    // c agrees on type and priority: no card, recorded silently.
    assert!(card(&d, &c, "type").is_none());
    assert!(card(&d, &c, "priority").is_none());
    let recorded = events(&tr.t, Kind::Suggested);
    assert_eq!(suggestion_of(&recorded, &c, "type")["current"], "bug");
    assert!(suggestion_of(&recorded, &c, "type").get("second_opinion").is_none());
    // Without a map every card is uncalibrated, and apply all takes none.
    assert!(d.iter().all(|x| x["badge"] == "uncalibrated" && x["bulk_ok"] == false));
}

#[test]
fn a_label_at_0_2_is_a_second_opinion_and_above_it_none() {
    let tr = tracked();
    let (a, b) = (tr.items[0].1.clone(), tr.items[1].1.clone());
    run(
        &tr,
        |item, d| match d {
            "type" if item == a => Some(against_bug(0.2)),
            "type" if item == b => Some(against_bug(0.21)),
            _ => None,
        },
        |_, _| None,
    );
    let d = docs(&tr.t);
    assert!(card(&d, &tr.items[0].0, "type").is_some(), "0.2 is at most 0.2");
    assert!(card(&d, &tr.items[1].0, "type").is_none(), "0.21 is not");
}

#[test]
fn a_run_shows_at_most_five_second_opinions_priority_first_then_type_by_disagreement() {
    let tr = tracked();
    let p_bug = |item: &str| {
        if item == tr.items[0].1 {
            0.15
        } else if item == tr.items[1].1 {
            0.05
        } else {
            0.10
        }
    };
    // Nine second opinions: priority and type on every item, and kernel at 0.1 on every area.
    run(
        &tr,
        |item, d| (d == "type").then(|| against_bug(p_bug(item))),
        |_, _| Some(against_p1(0.05)),
    );
    let d = docs(&tr.t);
    let mut shown: Vec<(String, String)> = d
        .iter()
        .filter(|x| x["second_opinion"].is_object())
        .map(|x| {
            (
                x["kind"].as_str().unwrap().to_string(),
                x["item"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    shown.sort();
    let mut want = vec![
        ("priority".to_string(), tr.items[0].0.clone()),
        ("priority".to_string(), tr.items[1].0.clone()),
        ("priority".to_string(), tr.items[2].0.clone()),
        ("type".to_string(), tr.items[1].0.clone()),
        ("type".to_string(), tr.items[2].0.clone()),
    ];
    want.sort();
    assert_eq!(shown, want, "the type at 0.15 and every area are past the cap");
}

#[test]
fn a_priority_card_more_than_one_level_from_the_rubric_carries_its_level() {
    let tr = tracked();
    untriage(&tr, &tr.items[0].0, 5);
    // Stage 1: answer_all's first answers, cosmetic · one_corner · silent.
    run(&tr, |_, _| None, |_, _| Some(priority_probs("p0", 0.85)));
    let d = docs(&tr.t);
    let p = card(&d, &tr.items[0].0, "priority").unwrap();
    assert_eq!(p["answer"], "p0");
    let rubric = Catalogue::embedded().file("priority").unwrap().rubric.clone().unwrap();
    let level = rubric.level("cosmetic", "one_corner", "silent").unwrap();
    assert_eq!(p["rubric_level"], json!(level));
}

// --- The accepted-suggestion layer (4.2) ----------------------------------------------------

#[test]
fn derive_takes_an_override_over_an_accepted_suggestion_over_the_label() {
    let tr = tracked();
    let b = tr.items[1].0.clone();
    run(
        &tr,
        |item, d| (d == "type" && item == tr.items[1].1).then(|| against_bug(0.05)),
        |_, _| None,
    );
    let i = issue(&tr.t, &b);
    assert_eq!(
        (i["type"].clone(), i["type_source"].clone()),
        (json!("bug"), json!("label"))
    );
    let doc = suggestion_id(&tr.t, &b, "type");
    assert_eq!(ingest(&tr.t, &accept("d-acc", &doc, false, 20), 20), None);
    let i = issue(&tr.t, &b);
    assert_eq!(
        (i["type"].clone(), i["type_source"].clone()),
        (json!("todo"), json!("suggestion"))
    );
    assert!(
        card(&docs(&tr.t), &b, "type").is_none(),
        "an accepted card does not return"
    );
    let over = on(
        "d-over",
        Action::SetOverride,
        Target {
            item: Some(b.clone()),
            ..Default::default()
        },
        Some(DecisionValue {
            type_: Some("decision".into()),
            priority: Some(Priority::P1),
            ..Default::default()
        }),
        21,
    );
    let mut over = over;
    over.reason = Some("tracking it as a decision".into());
    assert_eq!(ingest(&tr.t, &over, 21), None);
    let i = issue(&tr.t, &b);
    assert_eq!(
        (i["type"].clone(), i["type_source"].clone()),
        (json!("decision"), json!("override"))
    );
    assert_eq!(ingest(&tr.t, &undo("d-undo", "d-over"), 22), None);
    assert_eq!(issue(&tr.t, &b)["type"], "todo");
    assert_eq!(ingest(&tr.t, &undo("d-undo2", "d-acc"), 23), None);
    assert_eq!(issue(&tr.t, &b)["type"], "bug");
}

#[test]
fn accepting_a_priority_suggestion_is_a_project_priority_and_a_later_set_priority_wins() {
    let tr = tracked();
    let a = tr.items[0].0.clone();
    untriage(&tr, &a, 5);
    run(&tr, |_, _| None, |_, _| Some(priority_probs("p2", 0.85)));
    let doc = suggestion_id(&tr.t, &a, "priority");
    assert_eq!(ingest(&tr.t, &accept("d-p", &doc, false, 20), 20), None);
    let i = issue(&tr.t, &a);
    assert_eq!(i["priority"]["value"], "p2");
    assert_eq!(i["priority"]["source"], "project");
    let mut later = set_priority("d-set", &a, Priority::P0);
    later.created_at = format_time(at(21));
    assert_eq!(ingest(&tr.t, &later, 21), None);
    assert_eq!(issue(&tr.t, &a)["priority"]["value"], "p0");
}

#[test]
fn an_edit_sets_the_edited_value() {
    let tr = tracked();
    let a = tr.items[0].0.clone();
    untriage(&tr, &a, 5);
    run(&tr, |_, _| None, |_, _| None);
    let doc = suggestion_id(&tr.t, &a, "type");
    let e = on(
        "d-edit",
        Action::EditSuggestion,
        Target {
            suggestion: Some(doc),
            card: Some("cd-x".into()),
            component: Some("type".into()),
            ..Default::default()
        },
        Some(DecisionValue {
            type_: Some("docs".into()),
            ..Default::default()
        }),
        20,
    );
    assert_valid("page-decision", &serde_json::to_value(&e).unwrap());
    assert_eq!(ingest(&tr.t, &e, 20), None);
    let i = issue(&tr.t, &a);
    assert_eq!(
        (i["type"].clone(), i["type_source"].clone()),
        (json!("docs"), json!("suggestion"))
    );
}

#[test]
fn an_accept_refuses_once_the_item_s_value_changed_since_the_suggestion() {
    let tr = tracked();
    let a = tr.items[0].0.clone();
    untriage(&tr, &a, 5);
    run(&tr, |_, _| None, |_, _| None);
    let doc = suggestion_id(&tr.t, &a, "type");
    assert!(card(&docs(&tr.t), &a, "type").is_some());
    // Someone labelled it on GitHub meanwhile.
    edit(&tr.t, &a, 20, |c| c["labels"] = json!(["todo"]));
    assert!(
        card(&docs(&tr.t), &a, "type").is_none(),
        "the card is gone with its facts"
    );
    let m = ingest(&tr.t, &accept("d-stale", &doc, false, 21), 21).expect("a stale accept refuses");
    assert!(m.contains("stale"), "{m}");
    assert!(m.contains("\"todo\""), "{m}");
    assert_eq!(issue(&tr.t, &a)["type"], "todo");
}

#[test]
fn a_dismissed_second_opinion_confirms_the_label_as_owner_edit_truth() {
    let tr = tracked();
    let b = tr.items[1].0.clone();
    run(
        &tr,
        |item, d| (d == "type" && item == tr.items[1].1).then(|| against_bug(0.05)),
        |_, _| None,
    );
    let doc = suggestion_id(&tr.t, &b, "type");
    let dismiss = on(
        "d-dis",
        Action::DismissSuggestion,
        Target {
            suggestion: Some(doc),
            ..Default::default()
        },
        None,
        20,
    );
    assert_eq!(ingest(&tr.t, &dismiss, 20), None);
    assert!(card(&docs(&tr.t), &b, "type").is_none());
    let records = tr.t.root.read(|l| truth::owner_records(l.conn(), "home")).unwrap();
    let r = records.iter().find(|r| r.item == b && r.decision == "type").unwrap();
    assert_eq!(r.provenance, "owner_edit");
    assert_eq!(r.op, truth::Op::Set(vec!["bug".into()]));
}

// --- Apply all (4.3) ------------------------------------------------------------------------

/// The type key evaluated at `accuracy` before the run; a untriaged, b a type second opinion.
fn bulk_fixture(accuracy: f64) -> Tracked {
    let tr = tracked();
    untriage(&tr, &tr.items[0].0, 5);
    evaluate_type(&tr.t, 6, accuracy);
    run(
        &tr,
        |item, d| match d {
            "type" if item == tr.items[1].1 => Some(against_bug(0.05)),
            "type" => Some(type_probs("bug", 0.9)),
            _ => None,
        },
        |_, _| None,
    );
    tr
}

#[test]
fn apply_all_takes_an_untriaged_card_on_a_bulk_key_and_it_is_never_truth() {
    let tr = bulk_fixture(0.9);
    let a = tr.items[0].0.clone();
    let d = docs(&tr.t);
    let t = card(&d, &a, "type").unwrap();
    assert_eq!(
        (t["badge"].clone(), t["bulk_ok"].clone()),
        (json!("evaluated"), json!(true))
    );
    assert_eq!(t["map_id"], MAP);
    assert_eq!(
        card(&d, &tr.items[1].0, "type").unwrap()["bulk_ok"],
        false,
        "a second opinion"
    );
    assert_eq!(
        card(&d, &a, "area").unwrap()["bulk_ok"],
        false,
        "area has no evaluation"
    );
    let doc = suggestion_id(&tr.t, &a, "type");
    assert_eq!(ingest(&tr.t, &accept("d-bulk", &doc, true, 20), 20), None);
    assert_eq!(issue(&tr.t, &a)["type"], "bug");
    let records = tr.t.root.read(|l| truth::owner_records(l.conn(), "home")).unwrap();
    assert!(!records.iter().any(|r| r.item == a), "owner_bulk is never truth");
}

#[test]
fn apply_all_refuses_second_opinions_closures_and_keys_without_bulk() {
    let tr = bulk_fixture(0.9);
    let (a, b, c) = (tr.items[0].0.clone(), tr.items[1].0.clone(), tr.items[2].0.clone());
    let m = ingest(&tr.t, &accept("d1", &suggestion_id(&tr.t, &b, "type"), true, 20), 20).unwrap();
    assert!(m.contains("never takes a second opinion"), "{m}");
    let m = ingest(
        &tr.t,
        &accept("d2", &suggestion_id(&tr.t, &c, "likely_fixed"), true, 21),
        21,
    )
    .unwrap();
    assert!(m.contains("only type, area and priority"), "{m}");
    let m = ingest(&tr.t, &accept("d3", &suggestion_id(&tr.t, &a, "area"), true, 22), 22).unwrap();
    assert!(m.contains("does not pass Bulk"), "{m}");
    // The same accept one at a time is the owner's: taken.
    assert_eq!(
        ingest(&tr.t, &accept("d4", &suggestion_id(&tr.t, &b, "type"), false, 23), 23),
        None
    );

    let tr = bulk_fixture(0.8);
    let a = tr.items[0].0.clone();
    let d = docs(&tr.t);
    let t = card(&d, &a, "type").unwrap();
    assert_eq!(
        (t["badge"].clone(), t["bulk_ok"].clone()),
        (json!("evaluated"), json!(false))
    );
    let m = ingest(&tr.t, &accept("d5", &suggestion_id(&tr.t, &a, "type"), true, 20), 20).unwrap();
    assert!(m.contains("does not pass Bulk"), "{m}");
}

#[test]
fn a_drifting_key_badges_its_cards_and_apply_all_refuses_it() {
    let tr = bulk_fixture(0.9);
    let a = tr.items[0].0.clone();
    let (b, c) = (tr.items[1].0.clone(), tr.items[2].0.clone());
    let docs_bc = [suggestion_id(&tr.t, &b, "type"), suggestion_id(&tr.t, &c, "type")];
    // 30 owner records on b and c, 2 in 3 agreeing: 23 points below 0.9.
    for i in 0..30 {
        let doc = &docs_bc[i % 2];
        let d = if i % 3 == 2 {
            on(
                &format!("d-{i}"),
                Action::EditSuggestion,
                Target {
                    suggestion: Some(doc.clone()),
                    ..Default::default()
                },
                Some(DecisionValue {
                    type_: Some("docs".into()),
                    ..Default::default()
                }),
                20 + i as i64,
            )
        } else {
            accept(&format!("d-{i}"), doc, false, 20 + i as i64)
        };
        tr.t.root
            .mutate(at(20 + i as i64), |tx| decision::ingest(tx, "home", OWNER, &d))
            .unwrap();
    }
    let t = card(&docs(&tr.t), &a, "type").unwrap().clone();
    assert_eq!(
        (t["badge"].clone(), t["bulk_ok"].clone()),
        (json!("drifting"), json!(false))
    );
    let m = ingest(
        &tr.t,
        &accept("d-bulk", &suggestion_id(&tr.t, &a, "type"), true, 60),
        60,
    )
    .unwrap();
    assert!(m.contains("drifting"), "{m}");
}

// --- Details (4.2, 4.5) ---------------------------------------------------------------------

#[test]
fn a_details_edit_is_stored_shown_on_the_issue_and_owner_edit_truth() {
    let tr = tracked();
    let a = tr.items[0].0.clone();
    run(&tr, |_, _| None, |_, _| None);
    let i = issue(&tr.t, &a);
    assert_eq!(i["triage"]["severity"]["answer"], "cosmetic");
    assert_eq!(i["triage"]["severity"]["badge"], "uncalibrated");
    assert_eq!(i["triage"]["severity"]["edited"], Value::Null);
    assert_eq!(i["triage"]["reach"]["backend"], BACKEND);
    let e = on(
        "d-sev",
        Action::EditSuggestion,
        Target {
            item: Some(a.clone()),
            suggestion: Some(suggestion_id(&tr.t, &a, "severity")),
            ..Default::default()
        },
        Some(DecisionValue {
            severity: Some("wrong_result".into()),
            ..Default::default()
        }),
        20,
    );
    assert_valid("page-decision", &serde_json::to_value(&e).unwrap());
    assert_eq!(ingest(&tr.t, &e, 20), None);
    assert_eq!(issue(&tr.t, &a)["triage"]["severity"]["edited"], "wrong_result");
    let records = tr.t.root.read(|l| truth::owner_records(l.conn(), "home")).unwrap();
    let r = records
        .iter()
        .find(|r| r.item == a && r.decision == "severity")
        .unwrap();
    assert_eq!(
        (r.provenance.as_str(), r.from_backend.as_deref()),
        ("owner_edit", Some(BACKEND))
    );
    assert_eq!(r.op, truth::Op::Set(vec!["wrong_result".into()]));
    // Undo is the generic one.
    assert_eq!(ingest(&tr.t, &undo("d-u", "d-sev"), 21), None);
    assert_eq!(issue(&tr.t, &a)["triage"]["severity"]["edited"], Value::Null);
}

// --- Gate round 1 (the deep gate's findings and the mutation finder's survivors) ------------

/// An unrecorded suggestion id of the right shape.
const NO_SUCH: &str = "sg-000000000000000000000000";

#[test]
fn an_accept_or_edit_naming_no_recorded_suggestion_refuses() {
    let tr = tracked();
    let m = ingest(&tr.t, &accept("d1", NO_SUCH, false, 20), 20).expect("refused");
    assert!(m.contains("names no recorded suggestion"), "{m}");
    let m = ingest(&tr.t, &accept("d2", NO_SUCH, true, 21), 21).expect("refused");
    assert_eq!(m, "apply all names no recorded suggestion");
}

#[test]
fn an_accept_made_before_its_suggestion_was_recorded_refuses() {
    let tr = tracked();
    let a = tr.items[0].0.clone();
    untriage(&tr, &a, 5);
    run(&tr, |_, _| None, |_, _| None);
    let doc = suggestion_id(&tr.t, &a, "type");
    // Made at minute 9; the run recorded its answers at 12.
    let m = ingest(&tr.t, &accept("d-early", &doc, false, 9), 20).expect("refused");
    assert!(m.contains("recorded after this decision"), "{m}");
}

#[test]
fn a_run_does_not_re_answer_a_card_the_owner_acted_on_and_a_retry_still_repeats() {
    let tr = tracked();
    let a = tr.items[0].0.clone();
    untriage(&tr, &a, 5);
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items, 10);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 11).unwrap();
    let file = answer_with(&p, |_, d| (d == "type").then(|| type_probs("bug", 0.9)));
    record(&tr.t, "T1", file.clone(), 12).unwrap();
    let doc = suggestion_id(&tr.t, &a, "type");
    assert_eq!(ingest(&tr.t, &accept("d-acc", &doc, false, 20), 20), None);
    assert_eq!(issue(&tr.t, &a)["type"], "bug");
    // A retry of the same file after the accept: the same suggestions, not a refusal.
    record(&tr.t, "T1", file, 21).expect("an identical retry repeats");
    // The issue changes under the run, which asks again and answers differently.
    edit(&tr.t, &a, 22, |c| c["body"] = json!("a different body"));
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 23).unwrap();
    record(
        &tr.t,
        "T1",
        answer_with(&p, |_, d| (d == "type").then(|| type_probs("docs", 0.9))),
        24,
    )
    .unwrap();
    assert_eq!(
        (
            issue(&tr.t, &a)["type"].clone(),
            issue(&tr.t, &a)["type_source"].clone()
        ),
        (json!("bug"), json!("suggestion")),
        "the accept keeps the answer it was made on"
    );
}

#[test]
fn an_edit_is_checked_as_an_accept_and_never_bulk() {
    let tr = tracked();
    let a = tr.items[0].0.clone();
    untriage(&tr, &a, 5);
    run(&tr, |_, _| None, |_, _| None);
    let doc = suggestion_id(&tr.t, &a, "type");
    let edit_to = |id: &str, value: DecisionValue, minutes| {
        on(
            id,
            Action::EditSuggestion,
            Target {
                suggestion: Some(doc.clone()),
                ..Default::default()
            },
            Some(value),
            minutes,
        )
    };
    let docs_type = DecisionValue {
        type_: Some("docs".into()),
        ..Default::default()
    };
    let bulk = DecisionValue {
        bulk: Some(true),
        ..docs_type.clone()
    };
    assert_eq!(
        ingest(&tr.t, &edit_to("d-b", bulk, 20), 20),
        Some("apply all only accepts".to_string())
    );
    let wrong = DecisionValue {
        priority: Some(Priority::P2),
        ..Default::default()
    };
    let m = ingest(&tr.t, &edit_to("d-w", wrong, 21), 21).expect("refused");
    assert!(m.contains("carries no type"), "{m}");
    edit(&tr.t, &a, 22, |c| c["labels"] = json!(["todo"]));
    let m = ingest(&tr.t, &edit_to("d-s", docs_type, 23), 23).expect("a stale edit refuses");
    assert!(m.contains("stale"), "{m}");
    assert_eq!(issue(&tr.t, &a)["type"], "todo");
}

#[test]
fn an_area_edit_stores_a_set() {
    let tr = tracked();
    let a = tr.items[0].0.clone();
    untriage(&tr, &a, 5);
    run(&tr, |_, _| None, |_, _| None);
    let e = on(
        "d-area",
        Action::EditSuggestion,
        Target {
            suggestion: Some(suggestion_id(&tr.t, &a, "area")),
            ..Default::default()
        },
        Some(DecisionValue {
            areas: Some(vec!["ui".into(), "kernel".into(), "kernel".into()]),
            ..Default::default()
        }),
        20,
    );
    assert_eq!(ingest(&tr.t, &e, 20), None);
    let i = issue(&tr.t, &a);
    assert_eq!(
        (i["areas"].clone(), i["areas_source"].clone()),
        (json!(["kernel", "ui"]), json!("suggestion"))
    );
}

#[test]
fn a_details_edit_needs_no_suggestion() {
    let tr = tracked();
    let a = tr.items[0].0.clone();
    let e = on(
        "d-reach",
        Action::EditSuggestion,
        Target {
            item: Some(a.clone()),
            ..Default::default()
        },
        Some(DecisionValue {
            reach: Some("most".into()),
            ..Default::default()
        }),
        20,
    );
    assert_valid("page-decision", &serde_json::to_value(&e).unwrap());
    assert_eq!(ingest(&tr.t, &e, 20), None);
    assert_eq!(issue(&tr.t, &a)["triage"]["reach"]["edited"], "most");
}

#[test]
fn dismissing_a_stale_second_opinion_refuses_and_records_no_truth() {
    let tr = tracked();
    let b = tr.items[1].0.clone();
    run(
        &tr,
        |item, d| (d == "type" && item == tr.items[1].1).then(|| against_bug(0.05)),
        |_, _| None,
    );
    let doc = suggestion_id(&tr.t, &b, "type");
    edit(&tr.t, &b, 20, |c| c["labels"] = json!(["docs", "p1", "kernel"]));
    let dismiss = on(
        "d-dis",
        Action::DismissSuggestion,
        Target {
            suggestion: Some(doc),
            ..Default::default()
        },
        None,
        21,
    );
    let m = ingest(&tr.t, &dismiss, 21).expect("refused");
    assert!(m.contains("stale"), "{m}");
    let records = tr.t.root.read(|l| truth::owner_records(l.conn(), "home")).unwrap();
    assert!(!records.iter().any(|r| r.item == b && r.decision == "type"));
}

#[test]
fn apply_all_refuses_a_labelled_item_s_agreeing_suggestion() {
    let tr = bulk_fixture(0.9);
    let c = tr.items[2].0.clone();
    let m = ingest(&tr.t, &accept("d1", &suggestion_id(&tr.t, &c, "type"), true, 20), 20).expect("refused");
    assert!(m.contains("only an untriaged item's card"), "{m}");
}

#[test]
fn apply_all_refuses_a_key_whose_map_is_not_evaluated() {
    let tr = tracked();
    let a = tr.items[0].0.clone();
    untriage(&tr, &a, 5);
    evaluate_type_at(&tr.t, 6, 0.9, 0.2);
    run(&tr, |_, d| (d == "type").then(|| type_probs("bug", 0.9)), |_, _| None);
    assert_eq!(card(&docs(&tr.t), &a, "type").unwrap()["badge"], "unevaluated");
    let m = ingest(&tr.t, &accept("d1", &suggestion_id(&tr.t, &a, "type"), true, 20), 20).expect("refused");
    assert!(m.contains("(badge unevaluated)"), "{m}");
}

#[test]
fn a_closed_item_has_no_label_card() {
    let tr = tracked();
    let a = tr.items[0].0.clone();
    untriage(&tr, &a, 5);
    run(&tr, |_, _| None, |_, _| None);
    assert!(card(&docs(&tr.t), &a, "type").is_some());
    edit(&tr.t, &a, 20, |c| c["state"] = json!("closed"));
    assert!(card(&docs(&tr.t), &a, "type").is_none());
}

#[test]
fn a_retry_after_the_item_s_value_moved_is_the_same_suggestion() {
    let tr = tracked();
    let a = tr.items[0].0.clone();
    untriage(&tr, &a, 5);
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items, 10);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 11).unwrap();
    record(&tr.t, "T1", answer_all(&p), 12).unwrap();
    let p = prepare(&tr.t, "T1", BACKEND, Stage::Priority, 13).unwrap();
    let file = answer_with(&p, |_, d| (d == "priority").then(|| priority_probs("p2", 0.85)));
    record(&tr.t, "T1", file.clone(), 14).unwrap();
    // The owner sets the priority by hand: the suggestion's `current` would now be p0.
    let mut set = set_priority("d-set", &a, Priority::P0);
    set.created_at = format_time(at(20));
    assert_eq!(ingest(&tr.t, &set, 20), None);
    let r = record(&tr.t, "T1", file, 21).expect("an identical retry repeats");
    assert_eq!(r.recorded, 0);
}
