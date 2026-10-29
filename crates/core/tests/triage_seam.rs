//! Triage phase 1/5, the seam (triage spec 6.1's rows for 1.1-2.4): the question files and
//! their hash, the rubric, blinded inputs and the replay cutoff, two-stage `prepare` over the
//! due (item, decision) pairs, and `record-suggestions` checking each answer against what
//! `prepare` froze, with raw and (identity) calibrated probabilities.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use chrono::Duration;
use common::fixture::*;
use common::triage::*;
use issue_dashboard_core::config::ConfigSet;
use issue_dashboard_core::docs::{LaunchKind, Priority};
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::github::{Replay, Request};
use issue_dashboard_core::launch::{self, Via};
use issue_dashboard_core::observe::{self, Observation, ObservedData};
use issue_dashboard_core::root::parse_time;
use issue_dashboard_core::suggest;
use issue_dashboard_core::translate::Translation;
use issue_dashboard_core::triage::{
    self, AnswerKind, Catalogue, Comment, DECISION_FILES, LabelEvent, PreparedData, Source, Stage,
};
use issue_dashboard_core::{Error, ledger};
use serde_json::{Value, json};

const STORE_AREAS: [&str; 11] = [
    "admin-ui",
    "behaviors",
    "dev-loop",
    "drivers",
    "harness",
    "hometouch",
    "kernel",
    "media-port",
    "process",
    "renderer",
    "sdl",
];
/// What stage 1 asks about an inside author's Store issue.
const FIRST_STAGE: [&str; 8] = [
    "area",
    "detectability",
    "duplicate",
    "likely_fixed",
    "obsolete",
    "reach",
    "severity",
    "type",
];
const CLOSURES: [&str; 3] = ["duplicate", "likely_fixed", "obsolete"];
/// A day in fixture minutes.
const DAY: i64 = 24 * 60;

// --- Question files and the hash (1.2, 1.3) ------------------------------------------------

#[test]
fn every_file_in_decisions_is_embedded_and_parses() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../decisions");
    let mut on_disk: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    on_disk.sort();
    let embedded: Vec<String> = DECISION_FILES.iter().map(|(s, _)| format!("{s}.toml")).collect();
    assert_eq!(embedded, on_disk);
    let cat = Catalogue::embedded();
    let names: Vec<&str> = cat.decisions().map(|f| f.decision.as_str()).collect();
    assert_eq!(
        names,
        [
            "area",
            "detectability",
            "duplicate",
            "instructs_agent",
            "likely_fixed",
            "obsolete",
            "priority",
            "reach",
            "severity",
            "support_question",
            "type"
        ]
    );
    assert_eq!(
        cat.file("priority").unwrap().reads,
        ["severity", "reach", "detectability"]
    );
    assert_eq!(cat.allowed("type", &[]).unwrap(), ["bug", "todo", "decision", "docs"]);
    assert_eq!(
        cat.allowed("severity", &[]).unwrap(),
        ["cosmetic", "degraded", "wrong_result", "hang_or_data_loss"]
    );
    assert_eq!(cat.allowed("likely_fixed", &[]).unwrap(), ["true", "false"]);
}

#[test]
fn a_file_naming_another_decision_or_reading_a_missing_one_is_refused() {
    let m = refused(Catalogue::parse(&[(
        "type",
        "decision = \"area\"\nkind = \"areas\"\nscope = \"full\"\nquestion = \"q\"\n",
    )]));
    assert!(m.contains("decisions/type.toml names decision area"), "{m}");
    let m = refused(Catalogue::parse(&[(
        "priority",
        "decision = \"priority\"\nkind = \"choice\"\nscope = \"full\"\nanswers = [\"p0\", \"p1\"]\nreads = [\"severity\"]\nquestion = \"q\"\n",
    )]));
    assert!(m.contains("reads severity, which has no file"), "{m}");
}

/// The embedded files with `stem`'s text replaced.
fn files_with(stem: &str, text: &str) -> Catalogue {
    let texts: Vec<(&str, &str)> = DECISION_FILES
        .iter()
        .map(|(s, t)| (*s, if *s == stem { text } else { *t }))
        .collect();
    Catalogue::parse(&texts).unwrap()
}

#[test]
fn priority_s_hash_changes_when_severity_toml_does_and_type_s_does_not() {
    let cat = Catalogue::embedded();
    let severity = DECISION_FILES.iter().find(|(s, _)| *s == "severity").unwrap().1;
    let reworded = files_with("severity", &severity.replace("how bad is its effect", "how bad is it"));
    let hash = |c: &Catalogue, d: &str| c.question_hash(d, &[]).unwrap();
    assert_eq!(hash(cat, "priority").len(), 12);
    assert!(hash(cat, "priority").bytes().all(|b| b.is_ascii_hexdigit()));
    assert_ne!(hash(cat, "priority"), hash(&reworded, "priority"));
    assert_ne!(hash(cat, "severity"), hash(&reworded, "severity"));
    assert_eq!(hash(cat, "type"), hash(&reworded, "type"));
    assert_eq!(hash(cat, "reach"), hash(&reworded, "reach"));
}

#[test]
fn area_s_hash_follows_the_repo_s_area_list_and_no_other_hash_does() {
    let cat = Catalogue::embedded();
    let a = |areas: &[&str], d: &str| {
        let v: Vec<String> = areas.iter().map(|s| s.to_string()).collect();
        cat.question_hash(d, &v).unwrap()
    };
    assert_ne!(a(&["kernel", "sdl"], "area"), a(&["kernel", "vm"], "area"));
    assert_eq!(a(&["kernel", "sdl"], "area"), a(&["sdl", "kernel"], "area"));
    assert_eq!(a(&["kernel", "sdl"], "type"), a(&["vm"], "type"));
}

// --- The rubric (1.5) -----------------------------------------------------------------------

#[test]
fn the_rubric_s_boundaries_and_the_silent_bump() {
    let rubric = Catalogue::embedded().file("priority").unwrap().rubric.clone().unwrap();
    for (points, level) in [
        (25, Priority::P1),
        (26, Priority::P0),
        (8, Priority::P2),
        (9, Priority::P1),
        (2, Priority::P3),
        (3, Priority::P2),
    ] {
        assert_eq!(rubric.level_for_points(points), level, "{points} points");
    }
    // 8 x 1 = 8: p2, one level up when silent.
    assert_eq!(
        rubric.level("wrong_result", "one_corner", "visible"),
        Some(Priority::P2)
    );
    assert_eq!(rubric.level("wrong_result", "one_corner", "silent"), Some(Priority::P1));
    // 13 x 2 = 26: p0, and silent cannot raise it further.
    assert_eq!(
        rubric.level("hang_or_data_loss", "one_subsystem", "silent"),
        Some(Priority::P0)
    );
    // 1 x 1 = 1: p3, p2 when silent.
    assert_eq!(rubric.level("cosmetic", "one_corner", "visible"), Some(Priority::P3));
    assert_eq!(rubric.level("cosmetic", "one_corner", "silent"), Some(Priority::P2));
    assert_eq!(rubric.level("cosmetic", "nowhere", "silent"), None);
}

// --- Blinding and the replay cutoff (1.1) ---------------------------------------------------

fn store_translation() -> Translation {
    let c = ConfigSet::load(&fixtures().join("config")).unwrap();
    Translation::for_repo(&c, STORE)
}

fn replay_source() -> Source {
    let comment = |inside: bool, at: &str, body: &str| Comment {
        author: if inside { "yuvalrakavy" } else { "someone" }.into(),
        inside,
        at: at.into(),
        body: Some(body.into()),
    };
    Source {
        id: "I_kwDOAbCdEf5xYz34".into(),
        alias: "yuvalrakavy/Store#405".into(),
        title: "Scheduler stalls".into(),
        body: Some("The scheduler stops after a reload.".into()),
        body_truncated: false,
        labels: vec!["bug".into(), "p1".into(), "kernel".into(), "upstream".into()],
        author: "yuvalrakavy".into(),
        inside: true,
        comments: vec![
            comment(true, "2026-10-20T10:00:00Z", "reproduced on main"),
            comment(true, "2026-10-20T12:00:00Z", "bumped to p1"),
            comment(false, "2026-10-20T09:00:00Z", "me too"),
        ],
        reactions: 0,
    }
}

#[test]
fn a_replay_input_holds_no_comment_after_the_item_s_earliest_label_decision() {
    let t = store_translation();
    let events = [
        LabelEvent {
            label: "upstream".into(),
            at: "2026-10-20T08:00:00Z".into(),
        },
        LabelEvent {
            label: "p1".into(),
            at: "2026-10-20T11:00:00Z".into(),
        },
        LabelEvent {
            label: "bug".into(),
            at: "2026-10-20T13:00:00Z".into(),
        },
    ];
    let cutoff = triage::replay_cutoff(&t, &events).unwrap();
    // `upstream` is no label decision's, so the priority label's time is the cutoff.
    assert_eq!(cutoff.as_deref(), Some("2026-10-20T11:00:00Z"));
    let cat = Catalogue::embedded();
    let input = triage::input(cat, "type", &replay_source(), &t, cutoff.as_deref(), None).unwrap();
    assert_eq!(
        input["comments"],
        json!([{"author": "yuvalrakavy", "at": "2026-10-20T10:00:00Z", "body": "reproduced on main"}])
    );
    // Without a cutoff (live), both inside comments; never the outside one.
    let live = triage::input(cat, "type", &replay_source(), &t, None, None).unwrap();
    let bodies: Vec<&str> = live["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["body"].as_str().unwrap())
        .collect();
    assert_eq!(bodies, ["reproduced on main", "bumped to p1"]);
}

#[test]
fn an_input_shows_only_labels_that_are_no_label_decision_s() {
    let t = store_translation();
    let cat = Catalogue::embedded();
    for f in cat.decisions() {
        let input = triage::input(cat, &f.decision, &replay_source(), &t, None, None).unwrap();
        assert_eq!(input["labels"], json!(["upstream"]), "{}", f.decision);
    }
    assert_eq!(
        triage::blinded_labels(&t, &["Bug".into(), "needs-decision".into(), "sdl".into(), "P3".into()]),
        ["needs-decision"]
    );
}

#[test]
fn no_prepared_input_holds_a_type_area_or_priority_label_in_either_stage() {
    let tr = tracked();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items, 2);
    let first = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    assert_eq!(questions(&first).len(), 3 * FIRST_STAGE.len());
    record(&tr.t, "T1", answer_all(&first), 4).unwrap();
    let second = prepare(&tr.t, "T1", BACKEND, Stage::Priority, 5).unwrap();
    assert_eq!(questions(&second).len(), 3);
    for q in questions(&first).iter().chain(questions(&second)) {
        assert_eq!(q["input"]["labels"], json!(["upstream"]), "{}", q["decision"]);
        let text = q["input"].to_string();
        for leak in ["\"bug\"", "\"todo\""] {
            assert!(!text.contains(leak), "{} input holds {leak}: {text}", q["decision"]);
        }
    }
}

// --- prepare (2.1, 2.2) ---------------------------------------------------------------------

#[test]
fn prepare_refuses_a_session_model_unlike_the_routing_and_records_nothing() {
    let tr = tracked();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items, 2);
    let m = refused(prepare(&tr.t, "T1", "claude-sonnet-5-5@medium", Stage::First, 3));
    assert!(
        m.contains("this session runs claude-sonnet-5-5@medium, but launch T1 is routed to claude-opus-5-5@medium"),
        "{m}"
    );
    assert!(events(&tr.t, Kind::Prepared).is_empty());
}

#[test]
fn prepare_refuses_a_launch_that_answers_no_questions_or_never_held_a_slot() {
    let tr = tracked();
    let mut fix = launch_of("F1", LaunchKind::Fix, &tr.items[..1]);
    fix.recipe = "lanes".into();
    tr.t.root
        .mutate(at(2), |tx| launch::request(tx, &fix, Via::Reserve))
        .unwrap();
    let m = refused(prepare(&tr.t, "F1", BACKEND, Stage::First, 3));
    assert!(m.contains("launch F1 is a fix launch"), "{m}");
    let doc = launch_of("T2", LaunchKind::Triage, &tr.items);
    tr.t.root
        .mutate(at(2), |tx| launch::request(tx, &doc, Via::Reserve))
        .unwrap();
    let m = refused(prepare(&tr.t, "T2", BACKEND, Stage::First, 3));
    assert!(m.contains("launch T2 is requested"), "{m}");
}

#[test]
fn a_prepared_event_freezes_each_question_s_hashes_observation_and_map() {
    let tr = tracked();
    let (id, alias) = tr.items[0].clone();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    assert_eq!(asked_about(&p, &alias), FIRST_STAGE);
    assert_eq!(p.report.questions, 8);
    assert!(!p.report.repeat);
    let evs = events(&tr.t, Kind::Prepared);
    assert_eq!(evs.len(), 1);
    assert_eq!(evs[0].subject, "T1");
    assert_eq!(Some(evs[0].seq), p.report.seq);
    let d: PreparedData = serde_json::from_value(evs[0].data.clone()).unwrap();
    assert_eq!(
        (d.project.as_str(), d.launch.as_str(), d.backend.as_str()),
        ("home", "T1", BACKEND)
    );
    assert_eq!(d.stage, Stage::First);
    let seen =
        tr.t.root
            .read(|l| ledger::latest(l.conn(), &id, Kind::Observed))
            .unwrap()
            .unwrap()
            .seq;
    let cat = Catalogue::embedded();
    let areas: Vec<String> = STORE_AREAS.iter().map(|s| s.to_string()).collect();
    for q in &d.questions {
        assert_eq!((q.item.as_str(), q.alias.as_str()), (id.as_str(), alias.as_str()));
        assert_eq!(q.observed, seen);
        assert_eq!(q.map_id, None);
        assert_eq!(q.question_hash, cat.question_hash(&q.decision, &areas).unwrap());
        let file_q = questions(&p).iter().find(|x| x["id"] == q.id.as_str()).unwrap();
        assert_eq!(q.input_hash, triage::input_hash(&file_q["input"]));
    }
    let area = questions(&p).iter().find(|q| q["decision"] == "area").unwrap();
    assert_eq!(area["answers"], json!(STORE_AREAS));
    assert_eq!(area["input"]["areas"], json!(STORE_AREAS));
    // Prepared again before anything is recorded: the same questions, a repeat.
    let again = prepare(&tr.t, "T1", BACKEND, Stage::First, 4).unwrap();
    assert!(again.report.repeat);
    assert_eq!(again.report.seq, p.report.seq);
    assert_eq!(again.file, p.file);
}

#[test]
fn an_outside_author_s_issue_is_also_asked_support_question_and_instructs_agent() {
    let tr = tracked();
    let (id, alias) = tr.items[0].clone();
    edit(&tr.t, &id, 2, |c| {
        c["author"] = json!("someone");
        c["author_association"] = json!("NONE");
    });
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 3);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 4).unwrap();
    let mut want: Vec<&str> = FIRST_STAGE.to_vec();
    want.extend(["instructs_agent", "support_question"]);
    want.sort();
    assert_eq!(asked_about(&p, &alias), want);
}

#[test]
fn a_closures_launch_asks_closures_only_and_has_no_priority_stage() {
    let tr = tracked();
    let alias = tr.items[0].1.clone();
    launched(&tr.t, "R1", LaunchKind::RecommendClosures, &tr.items[..1], 2);
    let p = prepare(&tr.t, "R1", BACKEND, Stage::First, 3).unwrap();
    assert_eq!(asked_about(&p, &alias), CLOSURES);
    let m = refused(prepare(&tr.t, "R1", BACKEND, Stage::Priority, 3));
    assert!(
        m.contains("launch R1 asks closures only; it has no priority stage"),
        "{m}"
    );
}

#[test]
fn priority_cannot_be_prepared_before_stage_one_is_recorded() {
    let tr = tracked();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let m = refused(prepare(&tr.t, "T1", BACKEND, Stage::Priority, 3));
    assert!(
        m.contains("priority cannot be prepared before stage 1 is recorded"),
        "{m}"
    );
    assert!(
        m.contains("stage 1 has no recorded severity, reach, detectability answer"),
        "{m}"
    );
    // Prepared but not recorded is not enough.
    let first = prepare(&tr.t, "T1", BACKEND, Stage::First, 4).unwrap();
    let m = refused(prepare(&tr.t, "T1", BACKEND, Stage::Priority, 5));
    assert!(m.contains("before stage 1 is recorded"), "{m}");
    record(&tr.t, "T1", answer_all(&first), 6).unwrap();
    let p = prepare(&tr.t, "T1", BACKEND, Stage::Priority, 7).unwrap();
    let q = &questions(&p)[0];
    assert_eq!(q["decision"], "priority");
    assert_eq!(q["answers"], json!(["p0", "p1", "p2", "p3"]));
    // The first answers at 0.7: cosmetic x one_corner = 1 point, p3, raised by silent to p2.
    let r = &q["input"]["recorded"];
    assert_eq!(r["severity"]["answer"], "cosmetic");
    assert_eq!(r["reach"]["answer"], "one_corner");
    assert_eq!(r["detectability"]["answer"], "silent");
    assert_eq!(
        r["detectability"]["probabilities"],
        json!({"silent": 0.7, "visible": 0.3})
    );
    assert_eq!(q["input"]["rubric_level"], "p2");
    assert_eq!(
        q["input"]["rubric"]["levels"][0],
        json!({"at_least": 26, "level": "p0"})
    );
}

#[test]
fn a_question_counts_as_asked_once_recorded_and_returns_when_the_item_changes() {
    let tr = tracked();
    let (id, alias) = tr.items[0].clone();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    // An abandoned run: prepared, never recorded. The next launch asks everything again.
    launched(&tr.t, "T2", LaunchKind::Triage, &tr.items[..1], 4);
    let p2 = prepare(&tr.t, "T2", BACKEND, Stage::First, 5).unwrap();
    assert_eq!(asked_about(&p2, &alias), FIRST_STAGE);
    record(&tr.t, "T1", answer_all(&p), 6).unwrap();
    launched(&tr.t, "T3", LaunchKind::Triage, &tr.items[..1], 7);
    let p3 = prepare(&tr.t, "T3", BACKEND, Stage::First, 8).unwrap();
    assert_eq!(p3.report.questions, 0);
    assert_eq!(p3.report.seq, None, "nothing due records nothing");
    assert_eq!(p3.report.skipped, [json!({"item": alias, "why": "nothing due"})]);
    // A new observation (facts change, spec 5.8) makes every question due again.
    edit(&tr.t, &id, 9, |c| c["title"] = json!("Scheduler stalls after reload"));
    launched(&tr.t, "T4", LaunchKind::Triage, &tr.items[..1], 10);
    let p4 = prepare(&tr.t, "T4", BACKEND, Stage::First, 11).unwrap();
    assert_eq!(asked_about(&p4, &alias), FIRST_STAGE);
}

#[test]
fn an_item_due_only_for_a_closure_recheck_gets_no_full_questions() {
    let tr = tracked();
    let (id, alias) = tr.items[0].clone();
    // An outside author's issue: instructs_agent is a boolean of scope both too, but no closure.
    edit(&tr.t, &id, 2, |c| {
        c["author"] = json!("someone");
        c["author_association"] = json!("NONE");
    });
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 3);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    assert_eq!(p.report.questions, 10);
    record(&tr.t, "T1", answer_all(&p), 4).unwrap();
    // Answered at minute 4: the recheck is due 14 days later to the minute, not a minute before.
    launched(&tr.t, "T2", LaunchKind::Triage, &tr.items[..1], 14 * DAY + 3);
    let early = prepare(&tr.t, "T2", BACKEND, Stage::First, 14 * DAY + 3).unwrap();
    assert_eq!(early.report.questions, 0, "a minute short of 14 days: nothing due");
    launched(&tr.t, "T3", LaunchKind::Triage, &tr.items[..1], 14 * DAY + 4);
    let late = prepare(&tr.t, "T3", BACKEND, Stage::First, 14 * DAY + 4).unwrap();
    assert_eq!(asked_about(&late, &alias), CLOSURES);
}

#[test]
fn a_project_s_closure_recheck_days_replaces_the_default() {
    let tr = tracked();
    let alias = tr.items[0].1.clone();
    let path = tr.t.path().join("projects/home.toml");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        text.replace("history_days = 30\n", "history_days = 30\nclosure_recheck_days = 2\n"),
    )
    .unwrap();
    assert_eq!(
        ConfigSet::load(tr.t.path())
            .unwrap()
            .project("home")
            .unwrap()
            .closure_recheck_days,
        Some(2)
    );
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    record(&tr.t, "T1", answer_all(&p), 4).unwrap();
    launched(&tr.t, "T2", LaunchKind::Triage, &tr.items[..1], 2 * DAY + 4);
    let p2 = prepare(&tr.t, "T2", BACKEND, Stage::First, 2 * DAY + 4).unwrap();
    assert_eq!(asked_about(&p2, &alias), CLOSURES);
}

#[test]
fn a_closed_item_is_skipped() {
    let tr = tracked();
    let (id, alias) = tr.items[0].clone();
    edit(&tr.t, &id, 2, |c| c["state"] = json!("closed"));
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 3);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 4).unwrap();
    assert_eq!(p.report.skipped, [json!({"item": alias, "why": "closed"})]);
    assert_eq!(p.report.questions, 0);
}

// --- record-suggestions (2.1, 1.4, 2.3, 2.4) ----------------------------------------------

#[test]
fn an_answer_to_an_unprepared_question_or_from_another_backend_refuses_the_file() {
    let tr = tracked();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    let mut file = answer_all(&p);
    file["answers"][0]["question"] = json!("q-00000000000000000000");
    let m = refused(record(&tr.t, "T1", file, 4));
    assert!(
        m.contains("q-00000000000000000000: launch T1 prepared no such question"),
        "{m}"
    );
    // A question another launch prepared is not this launch's.
    launched(&tr.t, "T2", LaunchKind::Triage, &tr.items[..1], 5);
    let m = refused(record(&tr.t, "T2", answer_all(&p), 6));
    assert!(m.contains("launch T2 prepared no such question"), "{m}");
    let mut file = answer_all(&p);
    file["backend"] = json!("claude-sonnet-5-5@medium");
    let m = refused(record(&tr.t, "T1", file, 7));
    assert!(
        m.contains("the file's backend claude-sonnet-5-5@medium is not launch T1's routing claude-opus-5-5@medium"),
        "{m}"
    );
    let mut file = answer_all(&p);
    file.as_object_mut().unwrap().remove("backend");
    let m = refused(record(&tr.t, "T1", file, 8));
    assert!(m.contains("a file with answers or estimates names its backend"), "{m}");
    assert!(
        events(&tr.t, Kind::Suggested).is_empty(),
        "a refused file records nothing"
    );
}

#[test]
fn recorded_answers_carry_raw_and_identity_calibrated_probabilities() {
    let tr = tracked();
    let (id, alias) = tr.items[0].clone();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    let rep = record(&tr.t, "T1", answer_all(&p), 4).unwrap();
    assert_eq!((rep.recorded, rep.repeats), (8, 0));
    assert!(rep.dropped.is_empty());
    let evs = events(&tr.t, Kind::Suggested);
    let ty = suggestion_of(&evs, &id, "type");
    let qid = questions(&p).iter().find(|q| q["decision"] == "type").unwrap()["id"].clone();
    assert_eq!(ty["question"], qid);
    assert_eq!(ty["answer"], "bug");
    assert_eq!(ty["alias"], alias.as_str());
    assert_eq!(ty["backend"], BACKEND);
    assert_eq!(ty["evaluated"], false);
    assert_eq!(ty["reason"], "from the body");
    assert_eq!(ty["map_id"], Value::Null);
    // Stated 0.7 and 0.3 / 3 each; normalising a sum a rounding away from 1 moves the last digit.
    for (answer, p) in [("bug", 0.7), ("todo", 0.1), ("decision", 0.1), ("docs", 0.1)] {
        let raw = ty["raw"][answer].as_f64().unwrap();
        assert!((raw - p).abs() < 1e-12, "{answer}: {raw}");
    }
    assert_eq!(ty["raw"].as_object().unwrap().len(), 4);
    assert_eq!(ty["probabilities"], ty["raw"], "no map: the identity");
    let area = suggestion_of(&evs, &id, "area");
    assert_eq!(area["answer"], json!(["admin-ui"]));
    assert_eq!(area["raw"]["kernel"], 0.1);
    let fixed = suggestion_of(&evs, &id, "likely_fixed");
    assert_eq!(fixed["answer"], false);
    assert_eq!(fixed["raw"], json!({"true": 0.2, "false": 0.8}));
}

#[test]
fn an_answer_whose_item_changed_or_closed_is_dropped_and_the_rest_recorded() {
    let tr = tracked();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items, 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    let (b, b_alias) = tr.items[1].clone();
    let (c, c_alias) = tr.items[2].clone();
    edit(&tr.t, &b, 4, |v| v["body"] = json!("Edited after prepare."));
    edit(&tr.t, &c, 4, |v| v["state"] = json!("closed"));
    let rep = record(&tr.t, "T1", answer_all(&p), 5).unwrap();
    assert_eq!(rep.recorded, 8);
    assert_eq!(rep.dropped.len(), 16);
    let why: BTreeSet<(String, String)> = rep.dropped.iter().map(|d| (d.item.clone(), d.why.clone())).collect();
    assert_eq!(
        why,
        BTreeSet::from([
            (b_alias, "the issue changed since prepare".to_string()),
            (c_alias, "the issue closed since prepare".to_string()),
        ])
    );
    let evs = events(&tr.t, Kind::Suggested);
    assert!(
        evs.iter().all(|e| e.subject == tr.items[0].0),
        "only the unchanged item is recorded"
    );
    let _ = c;
}

#[test]
fn an_observation_that_leaves_the_input_unchanged_drops_nothing() {
    let tr = tracked();
    let (id, _) = tr.items[0].clone();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    // An assignee is no part of any input.
    edit(&tr.t, &id, 4, |v| v["assignees"] = json!(["yuvalrakavy"]));
    let rep = record(&tr.t, "T1", answer_all(&p), 5).unwrap();
    assert_eq!((rep.recorded, rep.dropped.len()), (8, 0));
}

#[test]
fn a_retry_of_the_same_file_records_identical_payloads() {
    let tr = tracked();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    record(&tr.t, "T1", answer_all(&p), 4).unwrap();
    let before = events(&tr.t, Kind::Suggested);
    let rep = record(&tr.t, "T1", answer_all(&p), 5).unwrap();
    assert_eq!((rep.recorded, rep.repeats), (0, 8));
    assert_eq!(events(&tr.t, Kind::Suggested), before);
}

#[test]
fn a_question_answered_twice_or_with_a_blank_reason_refuses_the_file() {
    let tr = tracked();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    let mut file = answer_all(&p);
    let first = file["answers"][0].clone();
    file["answers"].as_array_mut().unwrap().push(first);
    file["answers"][1]["reason"] = json!("  ");
    let m = refused(record(&tr.t, "T1", file, 4));
    assert!(m.contains("is answered twice"), "{m}");
    assert!(m.contains("needs a reason"), "{m}");
}

#[test]
fn stated_probabilities_are_checked_clipped_and_normalised() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let ty = s(&["bug", "todo", "decision", "docs"]);
    let p = |v: &[(&str, f64)]| v.iter().map(|(k, x)| (k.to_string(), *x)).collect::<BTreeMap<_, _>>();
    let check = |kind, allowed: &[String], stated| triage::check_probabilities(kind, allowed, &stated);
    let ok = check(
        AnswerKind::Choice,
        &ty,
        p(&[("bug", 1.0), ("todo", 0.0), ("decision", 0.0), ("docs", 0.0)]),
    )
    .unwrap();
    assert_eq!(ok.0, json!("bug"));
    assert_eq!(ok.1["bug"], 0.99 / 1.02);
    assert_eq!(ok.1["todo"], 0.01 / 1.02);
    let ok = check(
        AnswerKind::Choice,
        &ty,
        p(&[("bug", 0.1), ("todo", 0.519), ("decision", 0.2), ("docs", 0.2)]),
    )
    .unwrap();
    assert_eq!(ok.0, json!("todo"), "a sum of 1.019 is accepted");
    let e = check(
        AnswerKind::Choice,
        &ty,
        p(&[("bug", 0.1), ("todo", 0.521), ("decision", 0.2), ("docs", 0.2)]),
    )
    .unwrap_err();
    assert!(e.contains("sum to 1.021"), "{e}");
    let e = check(AnswerKind::Choice, &ty, p(&[("bug", 0.5), ("todo", 0.5)])).unwrap_err();
    assert!(e.contains("must name exactly bug, todo, decision, docs"), "{e}");
    let e = check(
        AnswerKind::Choice,
        &ty,
        p(&[("bug", 1.2), ("todo", -0.2), ("decision", 0.0), ("docs", 0.0)]),
    )
    .unwrap_err();
    assert!(e.contains("is not between 0 and 1"), "{e}");
    let tf = s(&["true", "false"]);
    assert_eq!(
        check(AnswerKind::Boolean, &tf, p(&[("true", 0.5), ("false", 0.5)]))
            .unwrap()
            .0,
        json!(false)
    );
    assert_eq!(
        check(AnswerKind::Boolean, &tf, p(&[("true", 0.6), ("false", 0.4)]))
            .unwrap()
            .0,
        json!(true)
    );
    // Areas: yes/no each, never normalised; the set is every area at 0.5 or more, else the top.
    let areas = s(&["kernel", "renderer", "sdl"]);
    let (set, raw) = check(
        AnswerKind::Areas,
        &areas,
        p(&[("kernel", 0.6), ("renderer", 0.5), ("sdl", 1.0)]),
    )
    .unwrap();
    assert_eq!(set, json!(["kernel", "renderer", "sdl"]));
    assert_eq!(raw["sdl"], 0.99);
    let (set, _) = check(
        AnswerKind::Areas,
        &areas,
        p(&[("kernel", 0.2), ("renderer", 0.4), ("sdl", 0.0)]),
    )
    .unwrap();
    assert_eq!(set, json!(["renderer"]));
}

#[test]
fn a_closure_answered_true_names_its_subject_and_becomes_a_card_and_false_does_not() {
    let tr = tracked();
    let (id, _) = tr.items[0].clone();
    let other = tr.items[1].1.clone();
    launched(&tr.t, "R1", LaunchKind::RecommendClosures, &tr.items[..1], 2);
    let p = prepare(&tr.t, "R1", BACKEND, Stage::First, 3).unwrap();
    let mut file = answer_all(&p);
    for a in file["answers"].as_array_mut().unwrap() {
        a["probabilities"] = json!({"true": 0.9, "false": 0.1});
    }
    let m = refused(record(&tr.t, "R1", file.clone(), 4));
    assert!(m.contains("likely_fixed needs the fixing commit"), "{m}");
    assert!(m.contains("duplicate needs the other issue (related)"), "{m}");
    let decision_of = |q: &Value| -> String {
        questions(&p).iter().find(|x| x["id"] == q["question"]).unwrap()["decision"]
            .as_str()
            .unwrap()
            .to_string()
    };
    for a in file["answers"].as_array_mut().unwrap() {
        match decision_of(a).as_str() {
            "likely_fixed" => a["commit"] = json!("abc1234"),
            "duplicate" => a["related"] = json!(other),
            _ => a["probabilities"] = json!({"true": 0.3, "false": 0.7}),
        }
    }
    let rep = record(&tr.t, "R1", file, 4).unwrap();
    assert_eq!(rep.recorded, 3);
    let docs =
        tr.t.root
            .read(|l| suggest::documents(l.conn(), &ConfigSet::load(tr.t.path()).unwrap(), "home"))
            .unwrap();
    let mut kinds: Vec<&str> = docs.iter().map(|d| d["kind"].as_str().unwrap()).collect();
    kinds.sort();
    assert_eq!(kinds, ["duplicate", "likely_fixed"], "obsolete was answered false");
    let fixed = docs.iter().find(|d| d["kind"] == "likely_fixed").unwrap();
    assert_eq!(
        (fixed["item"].as_str(), fixed["commit"].as_str()),
        (Some(id.as_str()), Some("abc1234"))
    );
    assert_eq!(fixed["confidence"], 0.9);
    assert_eq!(fixed["action_label"], "Check and close…");
    assert_eq!(
        docs.iter().find(|d| d["kind"] == "duplicate").unwrap()["related"],
        tr.items[1].0.as_str()
    );
}

#[test]
fn an_agreeing_label_is_no_card_and_a_confident_disagreement_a_second_opinion() {
    // Triage phase 3 (spec 4.1) replaced phase 1's "no label card": the item is labelled bug and
    // kernel; answer_all says bug at 0.7 (agreeing: no card) and kernel at 0.1 (a second opinion).
    let tr = tracked();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    record(&tr.t, "T1", answer_all(&p), 4).unwrap();
    let docs =
        tr.t.root
            .read(|l| suggest::documents(l.conn(), &ConfigSet::load(tr.t.path()).unwrap(), "home"))
            .unwrap();
    let kinds: Vec<&str> = docs.iter().map(|d| d["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds, ["area"], "every closure was answered false and type agrees");
    assert_eq!(docs[0]["second_opinion"]["current"], json!(["kernel"]));
    assert_eq!(docs[0]["second_opinion"]["flagged"], json!(["kernel"]));
    assert_eq!(docs[0]["action_label"], "Change to admin-ui");
}

#[test]
fn a_triage_launch_answers_closures_through_its_questions_not_phase_one_entries() {
    let tr = tracked();
    let alias = tr.items[0].1.clone();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let m = refused(record(
        &tr.t,
        "T1",
        json!({"suggestions": [{"kind": "obsolete", "item": alias, "confidence": 0.6, "reason": "superseded"}]}),
        3,
    ));
    assert!(
        m.contains("a triage launch answers its prepared obsolete questions instead"),
        "{m}"
    );
}

#[test]
fn estimates_are_recorded_as_a_record_without_probabilities() {
    let tr = tracked();
    let (id, alias) = tr.items[0].clone();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let est = json!({"item": alias, "sites": ["store_server/src/command/delete.rs:212"], "read_at": "95c2531bb",
        "tier": "T1", "blast": "quick", "effort": "S", "reason": "one guarded path"});
    let mut bad = est.clone();
    bad["read_at"] = json!("main");
    let m = refused(record(&tr.t, "T1", json!({"backend": BACKEND, "estimates": [bad]}), 3));
    assert!(m.contains("main is not a commit hash"), "{m}");
    let rep = record(&tr.t, "T1", json!({"backend": BACKEND, "estimates": [est]}), 4).unwrap();
    assert_eq!(rep.recorded, 1);
    let evs = events(&tr.t, Kind::Suggested);
    let s = suggestion_of(&evs, &id, "estimates");
    assert_eq!(
        s["estimate"],
        json!({"sites": ["store_server/src/command/delete.rs:212"], "read_at": "95c2531bb", "tier": "T1", "blast": "quick", "effort": "S"})
    );
    assert_eq!(s.get("probabilities"), None);
    assert_eq!(s["backend"], BACKEND);
}

#[test]
fn a_phase_one_recommender_file_still_records_under_the_launch_s_routing() {
    let tr = tracked();
    let alias = tr.items[0].1.clone();
    launched(&tr.t, "R1", LaunchKind::RecommendClosures, &tr.items[..1], 2);
    let rep = record(
        &tr.t,
        "R1",
        json!({"suggestions": [{"kind": "obsolete", "item": alias, "confidence": 0.6, "reason": "superseded"}]}),
        3,
    )
    .unwrap();
    assert_eq!(rep.recorded, 1);
    let evs = events(&tr.t, Kind::Suggested);
    assert_eq!(evs[0].data["suggestion"]["backend"], BACKEND);
}

// --- Mutation-gate gaps (round 1) -----------------------------------------------------------

#[test]
fn stated_probabilities_name_every_answer_once_and_ties_go_to_the_first() {
    let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
    let p = |v: &[(&str, f64)]| v.iter().map(|(k, x)| (k.to_string(), *x)).collect::<BTreeMap<_, _>>();
    let ty = s(&["bug", "todo", "decision", "docs"]);
    let e = triage::check_probabilities(
        AnswerKind::Choice,
        &ty,
        &p(&[
            ("bug", 0.7),
            ("todo", 0.1),
            ("decision", 0.1),
            ("docs", 0.1),
            ("wontfix", 0.0),
        ]),
    )
    .unwrap_err();
    assert!(e.contains("must name exactly bug, todo, decision, docs"), "{e}");
    for bad in [f64::NAN, f64::INFINITY] {
        let e = triage::check_probabilities(
            AnswerKind::Choice,
            &ty,
            &p(&[("bug", bad), ("todo", 0.0), ("decision", 0.0), ("docs", 0.0)]),
        )
        .unwrap_err();
        assert!(e.contains("is not between 0 and 1"), "{bad}: {e}");
    }
    // Each bound on its own, the sum within tolerance.
    for v in [
        [("bug", 1.01), ("todo", 0.0), ("decision", 0.0), ("docs", 0.0)],
        [("bug", 0.51), ("todo", 0.5), ("decision", -0.01), ("docs", 0.0)],
    ] {
        let e = triage::check_probabilities(AnswerKind::Choice, &ty, &p(&v)).unwrap_err();
        assert!(e.contains("is not between 0 and 1"), "{v:?}: {e}");
    }
    let tie = p(&[("bug", 0.4), ("todo", 0.4), ("decision", 0.1), ("docs", 0.1)]);
    assert_eq!(
        triage::check_probabilities(AnswerKind::Choice, &ty, &tie).unwrap().0,
        json!("bug")
    );
    let areas = s(&["kernel", "renderer", "sdl"]);
    let under = p(&[("kernel", 0.45), ("renderer", 0.4), ("sdl", 0.0)]);
    assert_eq!(
        triage::check_probabilities(AnswerKind::Areas, &areas, &under)
            .unwrap()
            .0,
        json!(["kernel"])
    );
    let tie = p(&[("kernel", 0.3), ("renderer", 0.3), ("sdl", 0.1)]);
    assert_eq!(
        triage::check_probabilities(AnswerKind::Areas, &areas, &tie).unwrap().0,
        json!(["kernel"])
    );
}

fn outside_source() -> Source {
    let comment = |author: &str, inside: bool| Comment {
        author: author.into(),
        inside,
        at: "2026-10-20T10:00:00Z".into(),
        body: Some("+1".into()),
    };
    Source {
        author: "reporter".into(),
        inside: false,
        comments: vec![
            comment("reporter", false),
            comment("someone", false),
            comment("someone", false),
            comment("yuvalrakavy", true),
        ],
        reactions: 3,
        ..replay_source()
    }
}

#[test]
fn instructs_agent_sees_an_outside_author_s_text_and_never_an_inside_author_s() {
    let t = store_translation();
    let cat = Catalogue::embedded();
    let inside = triage::input(cat, "instructs_agent", &replay_source(), &t, None, None).unwrap();
    assert_eq!((&inside["title"], &inside["body"]), (&Value::Null, &Value::Null));
    let outside = triage::input(cat, "instructs_agent", &outside_source(), &t, None, None).unwrap();
    assert_eq!(outside["title"], "Scheduler stalls");
    assert_eq!(outside["body"], "The scheduler stops after a reload.");
    let authors: Vec<&str> = outside["comments"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["author"].as_str().unwrap())
        .collect();
    assert_eq!(authors, ["reporter", "someone", "someone"], "outside comments only");
}

#[test]
fn reach_counts_distinct_outside_commenters_other_than_the_author_and_reactions() {
    let t = store_translation();
    let input = triage::input(Catalogue::embedded(), "reach", &outside_source(), &t, None, None).unwrap();
    assert_eq!(
        input["outside_demand"],
        json!({"outside_commenters": 1, "reactions": 3})
    );
}

#[test]
fn prepare_skips_an_item_never_read() {
    let tr = tracked();
    let mut items = tr.items[..1].to_vec();
    items.push(("I_kwDOneverRead0001".into(), format!("{STORE}#99999")));
    launched(&tr.t, "T1", LaunchKind::Triage, &items, 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    assert_eq!(
        p.report.skipped,
        [json!({"item": format!("{STORE}#99999"), "why": "never read"})]
    );
    assert_eq!(p.report.questions, 8);
}

#[test]
fn prepare_skips_a_pull_request() {
    let tr = tracked();
    let (id, alias) = tr.items[0].clone();
    // The observation's item kind is what makes a pull request.
    tr.t.root
        .mutate(at(2), |tx| {
            let e = ledger::latest(tx.conn(), &id, Kind::Observed)?.expect("observed");
            let mut data: ObservedData = serde_json::from_value(e.data)?;
            // An unchanged content is no new observation.
            data.content["title"] = json!("Scheduler stalls (pull request)");
            observe::record(
                tx,
                &Observation {
                    node_id: id.clone(),
                    item: serde_json::from_value(json!("pull_request"))?,
                    alias: data.alias.clone(),
                    updated_at: parse_time(&data.updated_at)? + Duration::seconds(1),
                    read_at: parse_time(&data.read_at)? + Duration::seconds(1),
                    read_id: "read-pr".into(),
                    content: data.content,
                },
            )
        })
        .unwrap();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 3);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 4).unwrap();
    assert_eq!(p.report.skipped, [json!({"item": alias, "why": "not an issue"})]);
}

#[test]
fn a_reworded_stage_one_file_holds_priority_back_until_answered_again() {
    let tr = tracked();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let first = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    record(&tr.t, "T1", answer_all(&first), 4).unwrap();
    let severity = DECISION_FILES.iter().find(|(s, _)| *s == "severity").unwrap().1;
    let reworded = files_with("severity", &severity.replace("how bad is its effect", "how bad is it"));
    let c = ConfigSet::load(tr.t.path()).unwrap();
    let m = refused(tr.t.root.mutate(at(5), |tx| {
        triage::prepare_with(tx, &c, &reworded, "T1", BACKEND, Stage::Priority)
    }));
    assert!(m.contains("stage 1 has no recorded severity answer"), "{m}");
    // Under the reworded files stage 1 asks severity again, and nothing else.
    let again =
        tr.t.root
            .mutate(at(6), |tx| {
                triage::prepare_with(tx, &c, &reworded, "T1", BACKEND, Stage::First)
            })
            .unwrap();
    assert_eq!(asked_about(&again, &tr.items[0].1), ["severity"]);
}

#[test]
fn two_questions_of_one_item_and_decision_answered_together_refuse_the_file() {
    let tr = tracked();
    let (id, _) = tr.items[0].clone();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    edit(&tr.t, &id, 4, |c| c["body"] = json!("Edited."));
    let p2 = prepare(&tr.t, "T1", BACKEND, Stage::First, 5).unwrap();
    let type_id = |p: &triage::Prepared| questions(p).iter().find(|q| q["decision"] == "type").unwrap()["id"].clone();
    assert_ne!(type_id(&p), type_id(&p2), "a changed input is a new question");
    let mut file = answer_all(&p2);
    let old = answer_all(&p)["answers"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["question"] == type_id(&p))
        .unwrap()
        .clone();
    file["answers"].as_array_mut().unwrap().push(old);
    let m = refused(record(&tr.t, "T1", file, 6));
    assert!(m.contains("has two type answers"), "{m}");
}

#[test]
fn a_closure_s_subject_must_be_a_commit_hash_or_another_open_tracked_issue() {
    let tr = tracked();
    let (_, alias) = tr.items[0].clone();
    let (other, other_alias) = tr.items[1].clone();
    edit(&tr.t, &other, 2, |c| c["state"] = json!("closed"));
    launched(&tr.t, "R1", LaunchKind::RecommendClosures, &tr.items[..1], 3);
    let p = prepare(&tr.t, "R1", BACKEND, Stage::First, 4).unwrap();
    let decision_of = |q: &Value| -> String {
        questions(&p).iter().find(|x| x["id"] == q["question"]).unwrap()["decision"]
            .as_str()
            .unwrap()
            .to_string()
    };
    let file_with = |decision: &str, field: &str, value: Value| {
        let mut file = answer_all(&p);
        for a in file["answers"].as_array_mut().unwrap() {
            if decision_of(a) == decision {
                a["probabilities"] = json!({"true": 0.9, "false": 0.1});
                a[field] = value.clone();
            }
        }
        file
    };
    let forty_one = "a".repeat(41);
    for bad in ["main", "abc123", "ABC1234", forty_one.as_str()] {
        let m = refused(record(&tr.t, "R1", file_with("likely_fixed", "commit", json!(bad)), 5));
        assert!(m.contains(&format!("{bad} is not a commit hash")), "{bad}: {m}");
    }
    let m = refused(record(&tr.t, "R1", file_with("duplicate", "related", json!(alias)), 5));
    assert!(m.contains("cannot be a duplicate of itself"), "{m}");
    let m = refused(record(
        &tr.t,
        "R1",
        file_with("duplicate", "related", json!(other_alias)),
        5,
    ));
    assert!(m.contains(&format!("{other_alias} is not an open issue")), "{m}");
    let untracked = format!("{STORE}#99999");
    let m = refused(record(
        &tr.t,
        "R1",
        file_with("duplicate", "related", json!(untracked)),
        5,
    ));
    assert!(m.contains("is not tracked by project home"), "{m}");
    assert!(events(&tr.t, Kind::Suggested).is_empty());
}

/// A closure launch on one Store item, prepared, and its answers with likely_fixed true on
/// `commit` and the rest false.
fn likely_fixed_on(commit: &str) -> (Tracked, Value) {
    let tr = tracked();
    launched(&tr.t, "R1", LaunchKind::RecommendClosures, &tr.items[..1], 2);
    let p = prepare(&tr.t, "R1", BACKEND, Stage::First, 3).unwrap();
    let mut file = answer_all(&p);
    for a in file["answers"].as_array_mut().unwrap() {
        let q = questions(&p).iter().find(|x| x["id"] == a["question"]).unwrap();
        if q["decision"] == "likely_fixed" {
            a["probabilities"] = json!({"true": 0.9, "false": 0.1});
            a["commit"] = json!(commit);
        } else {
            a["probabilities"] = json!({"true": 0.1, "false": 0.9});
        }
    }
    (tr, file)
}

/// The commit lookup GitHub answered with a real commit of Store (a recording, no hook).
#[test]
fn a_closure_s_commit_that_github_finds_makes_the_card() {
    let (tr, file) = likely_fixed_on(STORE_COMMIT);
    let gh = Replay::load(&fixtures().join("replay")).unwrap();
    record_via(&tr.t, "R1", file, 4, &gh).unwrap();
    let docs =
        tr.t.root
            .read(|l| suggest::documents(l.conn(), &ConfigSet::load(tr.t.path()).unwrap(), "home"))
            .unwrap();
    assert_eq!(docs.len(), 1, "{docs:?}");
    assert_eq!(
        (docs[0]["kind"].as_str(), docs[0]["commit"].as_str()),
        (Some("likely_fixed"), Some(STORE_COMMIT))
    );
}

/// GitHub's recorded answer for a hash Store does not hold: the answer is refused by name.
#[test]
fn a_closure_s_commit_that_github_does_not_hold_is_refused() {
    let (tr, file) = likely_fixed_on(MISSING_COMMIT);
    let gh = Replay::load(&fixtures().join("replay")).unwrap();
    let m = refused(record_via(&tr.t, "R1", file, 4, &gh));
    assert!(m.contains(&format!("{MISSING_COMMIT} is no commit of {STORE}")), "{m}");
    assert!(events(&tr.t, Kind::Suggested).is_empty());
}

/// A failed lookup is never taken for "exists": the answer is refused with its own reason.
#[test]
fn a_closure_s_commit_whose_lookup_fails_is_refused_not_trusted() {
    let (tr, file) = likely_fixed_on(STORE_COMMIT);
    let gh = Replay::load(&fixtures().join("replay"))
        .unwrap()
        .with_hook(Box::new(|req: &Request, recorded| match (req.name, recorded) {
            ("commit", _) => Err(Error::GitHub("gh: connection reset".into())),
            (_, Some(r)) => Ok(r),
            (name, None) => Err(Error::GitHub(format!("no recording for {name}"))),
        }));
    let m = refused(record_via(&tr.t, "R1", file.clone(), 4, &gh));
    assert!(
        m.contains(&format!("commit {STORE_COMMIT} of {STORE} could not be checked")),
        "{m}"
    );
    assert!(m.contains("connection reset"), "{m}");
    let input: suggest::Input = serde_json::from_value(file).unwrap();
    let m = refused(record_with(&tr.t, "R1", &input, 4, &suggest::Commits::default()));
    assert!(
        m.contains(&format!("commit {STORE_COMMIT} of {STORE} was not looked up")),
        "{m}"
    );
    assert!(events(&tr.t, Kind::Suggested).is_empty());
}

/// GitHub serving every commit lookup with `answer`.
fn commit_answer(answer: Value) -> Replay {
    Replay::load(&fixtures().join("replay"))
        .unwrap()
        .with_hook(Box::new(move |req: &Request, recorded| match (req.name, recorded) {
            ("commit", _) => Ok(answer.clone()),
            (_, Some(r)) => Ok(r),
            (name, None) => Err(Error::GitHub(format!("no recording for {name}"))),
        }))
}

/// `object(expression:)` resolves any revision: a ref named like the hash resolves to another
/// commit, which is not the commit the answer names.
#[test]
fn a_closure_s_commit_that_resolves_to_another_commit_is_refused() {
    let (tr, file) = likely_fixed_on(STORE_COMMIT);
    let other = json!({"data": {"repository": {"object": {"__typename": "Commit", "oid": "1".repeat(40)}}}});
    let m = refused(record_via(&tr.t, "R1", file, 4, &commit_answer(other)));
    assert!(m.contains(&format!("{STORE_COMMIT} is no commit of {STORE}")), "{m}");
}

/// An answer that carries errors is a failed lookup, even with a null object beside them.
#[test]
fn a_closure_s_commit_lookup_answered_with_errors_is_a_failure_not_a_miss() {
    let (tr, file) = likely_fixed_on(STORE_COMMIT);
    let partial = json!({"data": {"repository": {"object": null}},
        "errors": [{"type": "RATE_LIMITED", "message": "API rate limit exceeded"}]});
    let m = refused(record_via(&tr.t, "R1", file, 4, &commit_answer(partial)));
    assert!(
        m.contains("could not be checked (GitHub answered: API rate limit exceeded)"),
        "{m}"
    );
}

#[test]
fn an_estimate_needs_a_reason_a_blast_level_and_one_entry_per_item() {
    let tr = tracked();
    let alias = tr.items[0].1.clone();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let est = json!({"item": alias, "sites": ["src/a.rs:1"], "read_at": "95c2531bb",
        "tier": "T1", "blast": "quick", "effort": "S", "reason": "one guarded path"});
    let with = |field: &str, v: Value| {
        let mut e = est.clone();
        e[field] = v;
        json!({"backend": BACKEND, "estimates": [e]})
    };
    let m = refused(record(&tr.t, "T1", with("reason", json!(" ")), 3));
    assert!(m.contains("needs a reason"), "{m}");
    let m = refused(record(&tr.t, "T1", with("blast", json!("")), 3));
    assert!(m.contains("blast names the gate level a fix needs"), "{m}");
    let m = refused(record(
        &tr.t,
        "T1",
        json!({"backend": BACKEND, "estimates": [est, est]}),
        3,
    ));
    assert!(m.contains("has two estimates entries"), "{m}");
    let m = refused(record(&tr.t, "T1", json!({"estimates": [est]}), 3));
    assert!(m.contains("a file with answers or estimates names its backend"), "{m}");
    record(&tr.t, "T1", json!({"backend": BACKEND, "estimates": [est]}), 4).unwrap();
    let evs = events(&tr.t, Kind::Suggested);
    let s = &evs[0].data["suggestion"];
    assert_eq!(
        (&s["evaluated"], &s["reason"]),
        (&json!(false), &json!("one guarded path"))
    );
}

#[test]
fn a_triage_launch_still_records_a_batch() {
    let tr = tracked();
    let members: Vec<String> = tr.items[..2].iter().map(|(_, a)| a.clone()).collect();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..2], 2);
    let rep = record(
        &tr.t,
        "T1",
        json!({"suggestions": [{"kind": "batch", "members": members, "surface": "the scheduler",
            "confidence": 0.6, "reason": "one fix"}]}),
        3,
    )
    .unwrap();
    assert_eq!(rep.recorded, 1);
}

// --- Depth-gate findings (round 1) ----------------------------------------------------------

#[test]
fn priority_never_reads_stage_one_answers_about_older_facts() {
    let tr = tracked();
    let (id, _) = tr.items[0].clone();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let first = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    record(&tr.t, "T1", answer_all(&first), 4).unwrap();
    edit(&tr.t, &id, 5, |c| c["body"] = json!("It also loses the queue."));
    let m = refused(prepare(&tr.t, "T1", BACKEND, Stage::Priority, 6));
    assert!(
        m.contains("stage 1 has no recorded severity, reach, detectability answer"),
        "{m}"
    );
}

#[test]
fn a_decision_label_that_is_also_a_modifier_is_still_blinded_and_still_cuts_off() {
    let mut t = store_translation();
    t.priorities.get_mut(&Priority::P1).unwrap().push("urgent".into());
    t.modifiers.insert("needs-decision".into(), vec!["urgent".into()]);
    assert_eq!(
        triage::blinded_labels(&t, &["URGENT".into(), "upstream".into()]),
        ["upstream"]
    );
    let events = [LabelEvent {
        label: "Urgent".into(),
        at: "2026-10-20T11:00:00Z".into(),
    }];
    assert_eq!(
        triage::replay_cutoff(&t, &events).unwrap().as_deref(),
        Some("2026-10-20T11:00:00Z")
    );
}

#[test]
fn reach_counts_no_outside_commenter_after_the_replay_cutoff() {
    let t = store_translation();
    let cat = Catalogue::embedded();
    let input = triage::input(cat, "reach", &outside_source(), &t, Some("2026-10-20T09:00:00Z"), None).unwrap();
    assert_eq!(
        input["outside_demand"],
        json!({"outside_commenters": 0, "reactions": 3})
    );
}

#[test]
fn a_launch_re_asked_after_its_item_changed_records_the_new_answers_too() {
    let tr = tracked();
    let (id, _) = tr.items[0].clone();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    record(&tr.t, "T1", answer_all(&p), 4).unwrap();
    edit(&tr.t, &id, 5, |c| c["body"] = json!("It also loses the queue."));
    let p2 = prepare(&tr.t, "T1", BACKEND, Stage::First, 6).unwrap();
    assert_eq!(p2.report.questions, 8);
    let rep = record(&tr.t, "T1", answer_all(&p2), 7).unwrap();
    assert_eq!((rep.recorded, rep.repeats), (8, 0));
    // And a retry of either file is a repeat.
    assert_eq!(record(&tr.t, "T1", answer_all(&p2), 8).unwrap().repeats, 8);
}

#[test]
fn a_read_that_changes_only_blinded_labels_makes_new_questions_not_a_repeat() {
    let tr = tracked();
    let (id, _) = tr.items[0].clone();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    edit(&tr.t, &id, 4, |c| {
        c["labels"] = json!(["bug", "p2", "kernel", "upstream"])
    });
    let p2 = prepare(&tr.t, "T1", BACKEND, Stage::First, 5).unwrap();
    assert!(!p2.report.repeat, "the questions are about a newer observation");
    assert_ne!(p2.report.seq, p.report.seq);
    record(&tr.t, "T1", answer_all(&p2), 6).unwrap();
    launched(&tr.t, "T2", LaunchKind::Triage, &tr.items[..1], 7);
    assert_eq!(
        prepare(&tr.t, "T2", BACKEND, Stage::First, 8).unwrap().report.questions,
        0
    );
}

#[test]
fn an_area_answer_whose_area_list_changed_is_dropped_and_the_rest_recorded() {
    let tr = tracked();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    let path = tr.t.path().join("repos.toml");
    let text = fs::read_to_string(&path).unwrap();
    let line = "translate.area.behaviors  = [\"behaviors\"]\n";
    assert!(text.contains(line));
    fs::write(
        &path,
        text.replace(line, &format!("{line}translate.area.vm         = [\"vm\"]\n")),
    )
    .unwrap();
    let rep = record(&tr.t, "T1", answer_all(&p), 4).unwrap();
    assert_eq!(rep.recorded, 7);
    let dropped: Vec<(&str, &str)> = rep
        .dropped
        .iter()
        .map(|d| (d.decision.as_str(), d.why.as_str()))
        .collect();
    assert_eq!(dropped, [("area", "the issue changed since prepare")]);
}
