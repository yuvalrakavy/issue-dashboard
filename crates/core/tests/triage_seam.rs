//! Triage phase 1/5, the seam (triage spec 6.1's rows for 1.1-2.4): the question files and
//! their hash, the rubric, blinded inputs and the replay cutoff, two-stage `prepare` over the
//! due (item, decision) pairs, and `record-suggestions` checking each answer against what
//! `prepare` froze, with raw and (identity) calibrated probabilities.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;

use chrono::Duration;
use common::fixture::*;
use common::*;
use issue_dashboard_core::config::{ConfigSet, READ_ONLY_RECIPE};
use issue_dashboard_core::cycle;
use issue_dashboard_core::docs::{LaunchKind, LaunchRequest, Priority};
use issue_dashboard_core::event::{Event, Kind};
use issue_dashboard_core::launch::{self, Via};
use issue_dashboard_core::observe::{self, Observation, ObservedData};
use issue_dashboard_core::root::parse_time;
use issue_dashboard_core::suggest;
use issue_dashboard_core::translate::Translation;
use issue_dashboard_core::triage::{
    self, AnswerKind, Catalogue, Comment, DECISION_FILES, LabelEvent, PreparedData, Source, Stage,
};
use issue_dashboard_core::{Error, Result, ledger};
use serde_json::{Value, json};

const D1: &str = "local_dispatcher_1";
/// The fixture launches' routing (`launch_doc`), as a backend.
const BACKEND: &str = "claude-opus-5-5@medium";
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

fn cap(recipe: &str) -> Result<u32> {
    match recipe {
        READ_ONLY_RECIPE => Ok(1),
        other => Err(Error::refused(format!("no recipe {other}"))),
    }
}

fn refused<T: std::fmt::Debug>(r: Result<T>) -> String {
    match r {
        Err(Error::Refused(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A fixture root after one cycle, and three of its open Store issues, each re-observed as
/// written by the owner with the labels `bug`, `p1`, `kernel`, `upstream`.
struct Tracked {
    t: TempRoot,
    items: Vec<(String, String)>,
}

fn tracked() -> Tracked {
    let t = fixture_root();
    let r = cycle::cycle(&t.root, &gh(), "home", &no_pages(), &move || at(0), PAGE).unwrap();
    let d: Value = serde_json::from_str(&fs::read_to_string(&r.outbox).unwrap()).unwrap();
    let items: Vec<(String, String)> = d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| {
            i["alias"].as_str().unwrap().starts_with(&format!("{STORE}#"))
                && i["state"] == "open"
                && i["kind"] == "issue"
        })
        .take(3)
        .map(|i| {
            (
                i["id"].as_str().unwrap().to_string(),
                i["alias"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(items.len(), 3, "the fixture tracks open Store issues");
    for (id, _) in &items {
        edit(&t, id, 1, |c| {
            c["author"] = json!("yuvalrakavy");
            c["author_association"] = json!("OWNER");
            c["labels"] = json!(["bug", "p1", "kernel", "upstream"]);
        });
    }
    Tracked { t, items }
}

/// Records a fresher observation of `id` at fixture minute `minutes`, its content as `f`
/// leaves it: what a later read of an edited issue leaves in the ledger.
fn edit(t: &TempRoot, id: &str, minutes: i64, f: impl FnOnce(&mut Value)) {
    t.root
        .mutate(at(minutes), |tx| {
            let e = ledger::latest(tx.conn(), id, Kind::Observed)?.expect("an observed issue");
            let data: ObservedData = serde_json::from_value(e.data)?;
            let mut content = data.content.clone();
            f(&mut content);
            observe::record(
                tx,
                &Observation {
                    node_id: id.to_string(),
                    item: data.item,
                    alias: data.alias.clone(),
                    updated_at: parse_time(&data.updated_at)? + Duration::seconds(1),
                    read_at: parse_time(&data.read_at)? + Duration::seconds(1),
                    read_id: format!("read-{minutes}"),
                    content,
                },
            )
        })
        .unwrap();
}

fn launch_of(id: &str, kind: LaunchKind, items: &[(String, String)]) -> LaunchRequest {
    let mut d = launch_doc(id, "home", &[], READ_ONLY_RECIPE);
    d.kind = kind;
    d.items = items.iter().map(|(i, _)| i.clone()).collect();
    d.aliases = items.iter().map(|(_, a)| a.clone()).collect();
    d
}

/// Requests and reserves a launch, then frees the read-only slot (its session ran), so the
/// next launch of a test can reserve too.
fn launched(t: &TempRoot, id: &str, kind: LaunchKind, items: &[(String, String)], minutes: i64) {
    let doc = launch_of(id, kind, items);
    t.root
        .mutate(at(minutes), |tx| {
            launch::request(tx, &doc, Via::Reserve)?;
            let token = match launch::reserve(tx, id, D1, &cap)? {
                launch::Reservation::Reserved { token, .. } => token,
                other => panic!("expected a reservation, got {other:?}"),
            };
            launch::started(tx, &token, &format!("local_{id}"))?;
            launch::ended(tx, &token, launch::EndState::Landed)
        })
        .unwrap();
}

fn prepare(t: &TempRoot, launch: &str, model: &str, stage: Stage, minutes: i64) -> Result<triage::Prepared> {
    let c = ConfigSet::load(t.path()).unwrap();
    t.root
        .mutate(at(minutes), |tx| triage::prepare(tx, &c, launch, model, stage))
}

fn record(t: &TempRoot, launch: &str, input: Value, minutes: i64) -> Result<suggest::Report> {
    let c = ConfigSet::load(t.path()).unwrap();
    let input: suggest::Input = serde_json::from_value(input).unwrap();
    t.root.mutate(at(minutes), |tx| suggest::record(tx, &c, launch, &input))
}

fn events(t: &TempRoot, kind: Kind) -> Vec<Event> {
    t.root.read(|l| ledger::events_by_kind(l.conn(), kind)).unwrap()
}

fn questions(p: &triage::Prepared) -> &Vec<Value> {
    p.file["questions"].as_array().unwrap()
}

/// The decisions a prepared file asks about `alias`, sorted.
fn asked_about(p: &triage::Prepared, alias: &str) -> Vec<String> {
    let mut d: Vec<String> = questions(p)
        .iter()
        .filter(|q| q["item"] == alias)
        .map(|q| q["decision"].as_str().unwrap().to_string())
        .collect();
    d.sort();
    d
}

/// An answer to every question of `p`: the first allowed answer at 0.7 (the rest share 0.3),
/// a closure false at 0.8, the first area at 0.8 and every other at 0.1.
fn answer_all(p: &triage::Prepared) -> Value {
    let answers: Vec<Value> = questions(p)
        .iter()
        .map(|q| {
            let allowed: Vec<&str> = q["answers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap())
                .collect();
            let probabilities: BTreeMap<&str, f64> = match q["kind"].as_str().unwrap() {
                "boolean" => [("true", 0.2), ("false", 0.8)].into(),
                "areas" => allowed
                    .iter()
                    .enumerate()
                    .map(|(i, a)| (*a, if i == 0 { 0.8 } else { 0.1 }))
                    .collect(),
                _ => {
                    let rest = 0.3 / (allowed.len() - 1) as f64;
                    allowed
                        .iter()
                        .enumerate()
                        .map(|(i, a)| (*a, if i == 0 { 0.7 } else { rest }))
                        .collect()
                }
            };
            json!({"question": q["id"], "probabilities": probabilities, "reason": "from the body"})
        })
        .collect();
    json!({"backend": BACKEND, "answers": answers})
}

fn suggestion_of<'a>(evs: &'a [Event], item: &str, decision: &str) -> &'a Value {
    &evs.iter()
        .rev()
        .find(|e| e.subject == item && e.distinct["decision"] == decision)
        .unwrap_or_else(|| panic!("no {decision} suggestion for {item}"))
        .data["suggestion"]
}

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
    let alias = tr.items[0].1.clone();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    record(&tr.t, "T1", answer_all(&p), 4).unwrap();
    launched(&tr.t, "T2", LaunchKind::Triage, &tr.items[..1], 13 * DAY);
    let early = prepare(&tr.t, "T2", BACKEND, Stage::First, 13 * DAY + 1).unwrap();
    assert_eq!(early.report.questions, 0, "13 days: nothing due");
    launched(&tr.t, "T3", LaunchKind::Triage, &tr.items[..1], 14 * DAY + 5);
    let late = prepare(&tr.t, "T3", BACKEND, Stage::First, 14 * DAY + 5).unwrap();
    assert_eq!(asked_about(&late, &alias), CLOSURES);
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
    let docs = tr.t.root.read(|l| suggest::documents(l.conn(), "home")).unwrap();
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
fn label_decisions_make_no_card_until_the_page_shows_them() {
    let tr = tracked();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items[..1], 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    record(&tr.t, "T1", answer_all(&p), 4).unwrap();
    let docs = tr.t.root.read(|l| suggest::documents(l.conn(), "home")).unwrap();
    assert_eq!(
        docs,
        Vec::<Value>::new(),
        "every closure was answered false; no label decision is a card yet"
    );
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
