//! The bridge runner's flow (runner spec 2, 3.1; as-built D1-D5): the runner eval brief, the
//! eval stage plan, and the status-driven step over the fake bridge.

mod common;

use std::collections::HashMap;
use std::fs;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use common::TempRoot;
use common::fixture::*;
use common::triage::*;
use issue_dashboard_core::bridge::fake::{Answer, Fake};
use issue_dashboard_core::brief;
use issue_dashboard_core::config::ConfigSet;
use issue_dashboard_core::docs::LaunchKind;
use issue_dashboard_core::eval;
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::github::Replay as GhReplay;
use issue_dashboard_core::history::{self, HistoryItem, LabelChange};
use issue_dashboard_core::launch::{self, EndState, RUNNER, Reservation, State, Via};
use issue_dashboard_core::ledger;
use issue_dashboard_core::runner_status;
use issue_dashboard_core::service::{self, Refresher, Tick};
use issue_dashboard_core::root::{format_time, parse_time};
use issue_dashboard_core::runner::{self, Ctx, Stepped};
use issue_dashboard_core::Result;
use serde_json::{Value, json};

fn eval_brief(harness: Option<&str>) -> String {
    let tr = tracked();
    let mut doc = launch_of("E1", LaunchKind::Eval, &[]);
    doc.routing.harness = harness.map(str::to_string);
    tr.t.root
        .mutate(at(2), |tx| launch::request(tx, &doc, Via::Ingest))
        .unwrap();
    let c = ConfigSet::load(tr.t.path()).unwrap();
    tr.t.root
        .read(|l| brief::brief_with(l.conn(), &c, "E1", "issue-dashboard"))
        .unwrap()
        .text
}

#[test]
fn a_runner_eval_brief_has_the_worker_answer_staged_files_and_run_no_command() {
    let text = eval_brief(Some("Codex"));
    for command in ["eval prepare", "eval score", "refresh-history", "record-suggestions"] {
        assert!(!text.contains(command), "the runner's worker is told to run {command}:\n{text}");
    }
    for needed in ["stage_input", "stage_result", "\"answers\"", "launch_result", "correction"] {
        assert!(text.contains(needed), "the brief does not mention {needed}:\n{text}");
    }
    // Run A's worker, told only that attachments hold the questions, read them with a shell and
    // stalled on a permission prompt per stage: the brief names the one way to read them.
    for needed in ["agentbridge_native_snapshot", "transfer", "offset", "no shell command"] {
        assert!(text.contains(needed), "the brief does not say how to read attachments ({needed}):\n{text}");
    }
    // The dispatcher's eval brief is unchanged: its session still runs the commands.
    let dispatcher = eval_brief(None);
    assert!(dispatcher.contains("issue-dashboard eval prepare"), "{dispatcher}");
    assert!(dispatcher.contains("issue-dashboard eval score"), "{dispatcher}");
}

// --- The status-driven step (3.1; D2-D5) ----------------------------------------------------

/// The clock the history recording was made at (`triage_eval.rs`).
fn now() -> DateTime<Utc> {
    parse_time("2026-10-29T12:00:00Z").unwrap()
}

const SESSION: &str = "native-1";

/// A root holding the recorded airzoned and rhai history and an empty Store one, with eval
/// launch E1 routed to Codex and reserved by the runner.
struct Run {
    t: TempRoot,
    fake: Fake,
    files: PathBuf,
    token: String,
    part_bytes: std::cell::Cell<usize>,
}

fn run() -> Run {
    run_with(true)
}

/// `run`, with every Store issue only when `store`: otherwise only the Store issues that are
/// tuning items, so area and priority have candidates but no held-out one, and only type has
/// held-out examples.
fn run_with(store: bool) -> Run {
    let t = fixture_root();
    let gh = GhReplay::load(&fixtures().join("history-replay")).unwrap();
    history::refresh_repo(t.path(), &gh, AIRZONED, None, 730, now(), 5).unwrap();
    history::refresh_repo(t.path(), &gh, RHAI, Some("author:yuvalrakavy"), 730, now(), 5).unwrap();
    let mut s = history::load(t.path(), AIRZONED).unwrap().unwrap();
    // Store issues copied from airzoned#2, each labelled with a type, an area and a priority
    // soon after filing: enough of them that every decision has held-out examples.
    let base = s.items.iter().find(|i| i.alias == format!("{AIRZONED}#2")).unwrap().clone();
    let soon = format_time(parse_time(&base.created_at).unwrap() + Duration::minutes(1));
    let label = |n: u32, name: &str| LabelChange {
        id: format!("LE_{n}_{name}"),
        label: name.into(),
        added: true,
        at: soon.clone(),
        actor: "yuvalrakavy".into(),
    };
    s.repo = STORE.to_string();
    s.items = (800..812)
        .map(|n| HistoryItem {
            id: format!("{}s{n}", base.id),
            alias: format!("{STORE}#{n}"),
            labels: Vec::new(),
            label_events: vec![
                label(n, ["bug", "todo"][n as usize % 2]),
                label(n, "kernel"),
                label(n, ["p1", "p2", "p3"][n as usize % 3]),
            ],
            comments: Vec::new(),
            ..base.clone()
        })
        .filter(|i| store || eval::is_tuning(&i.id))
        .collect();
    assert!(store || !s.items.is_empty(), "no Store issue is a tuning item: the fixture tests nothing");
    history::save(t.path(), &s).unwrap();
    let mut doc = launch_of("E1", LaunchKind::Eval, &[]);
    doc.routing.harness = Some("Codex".into());
    let reserved = t
        .root
        .mutate(now(), |tx| {
            launch::request(tx, &doc, Via::Ingest)?;
            launch::reserve(tx, "E1", RUNNER, &cap)
        })
        .unwrap();
    let token = match reserved {
        Reservation::Reserved { token, .. } => token,
        other => panic!("expected a reservation, got {other:?}"),
    };
    let files = t.path().join("bridge-files");
    Run {
        t,
        fake: Fake::new(),
        files,
        token,
        // One part a stage unless a test cuts finer.
        part_bytes: std::cell::Cell::new(usize::MAX),
    }
}

impl Run {
    fn try_step(&self) -> Result<Stepped> {
        let cx = Ctx {
            root: &self.t.root,
            bridge: &self.fake,
            files: &self.files,
            now: &now,
            part_bytes: self.part_bytes.get(),
        };
        runner::step(&cx, &self.token)
    }

    fn step(&self) -> Stepped {
        self.try_step().unwrap()
    }

    fn status(&self, body: Value) {
        self.fake.set_status(&self.token, body);
    }

    /// The worker's session is up, on the routing asked for, with `stage` active.
    fn running(&self, stage: Value) {
        self.status(json!({
            "operation_id": self.token, "state": "started", "never_started": false,
            "native_session_id": SESSION, "result": null, "stage": stage,
            "routing": {"verified": true, "model": "claude-opus-5-5", "effort": "medium"},
        }));
    }

    fn state(&self) -> State {
        self.t.root.read(|l| launch::load(l.conn(), "E1")).unwrap().unwrap().state
    }

    fn sent(&self) -> Vec<Value> {
        self.fake.calls_to("send")
    }

    /// The question files staged for `part`, an AgentBridge stage name `<stage>#p<n>`.
    fn questions(&self, part: &str) -> Vec<Value> {
        let (stage, n) = part.rsplit_once("#p").expect("a part's stage name");
        let dir = self
            .files
            .join("runner")
            .join(&self.token)
            .join(stage.replace('/', "_"))
            .join(format!("p{n}"));
        let mut out = Vec::new();
        for e in fs::read_dir(&dir).unwrap() {
            let v: Value = serde_json::from_str(&fs::read_to_string(e.unwrap().path()).unwrap()).unwrap();
            out.extend(v["questions"].as_array().unwrap().iter().cloned());
        }
        out
    }

    /// Even odds over each question's answers: acceptable, whatever they score.
    fn answers(&self, stage: &str) -> Value {
        let answers: Vec<Value> = self
            .questions(stage)
            .iter()
            .map(|q| {
                let allowed = q["answers"].as_array().unwrap();
                let p = 1.0 / allowed.len() as f64;
                let probabilities: serde_json::Map<String, Value> =
                    allowed.iter().map(|a| (a.as_str().unwrap().to_string(), json!(p))).collect();
                json!({"question": q["id"], "probabilities": probabilities, "reason": "even odds"})
            })
            .collect();
        json!({"backend": BACKEND, "answers": answers})
    }

    /// AgentBridge's active round for the last stage input sent, its result `content` accepted
    /// as event `event`.
    fn answered(&self, content: Value, event: &str) {
        let m = self.sent().last().unwrap()["message"].clone();
        self.running(json!({
            "input": m, "interaction_id": "i", "command_key": "k",
            "result": {"content": content}, "result_event_id": event, "pending": false,
        }));
    }

    fn evaluated(&self) -> usize {
        self.t
            .root
            .read(|l| ledger::events_by_kind(l.conn(), Kind::Evaluated))
            .unwrap()
            .len()
    }

    /// Steps through every stage with acceptable answers, to the finish request.
    fn drive_to_finish(&self) -> Vec<String> {
        self.running(Value::Null);
        assert_eq!(self.step(), Stepped::RecordedStarted);
        let mut stages = Vec::new();
        for n in 0..200 {
            let stepped = self.try_step().unwrap_or_else(|e| panic!("after {stages:?}: {e}"));
            match stepped {
                Stepped::Sent { stage, round: 1 } => {
                    let a = self.answers(&stage);
                    self.answered(a, &format!("ev-{n}"));
                    stages.push(stage);
                }
                Stepped::Finish => return stages,
                other => panic!("after {stages:?}: {other:?}"),
            }
        }
        panic!("no finish after {stages:?}")
    }
}

const STAGES: [&str; 4] = ["type", "area", "priority-first", "priority"];

fn stage_names(r: &Run) -> Vec<String> {
    let c = ConfigSet::load(r.t.path()).unwrap();
    runner::eval_stages(&r.t.root, &c, "home", BACKEND)
        .unwrap()
        .into_iter()
        .map(|s| s.name)
        .collect()
}

#[test]
fn an_eval_is_staged_one_replay_key_at_a_time_priority_s_first_stage_before_it() {
    assert_eq!(stage_names(&run()), STAGES);
}

#[test]
fn a_replay_with_no_held_out_example_is_left_out_of_the_plan() {
    // Area and priority have truths only among tuning items here: score would refuse them.
    assert_eq!(stage_names(&run_with(false)), ["type"]);
}

#[test]
fn a_transfer_key_over_128_bytes_is_refused() {
    let key = runner::transfer_key(&"t".repeat(100), "type", &"f".repeat(22)).unwrap();
    assert_eq!(key.len(), 128);
    let refused = runner::transfer_key(&"t".repeat(100), "type", &"f".repeat(23));
    assert!(matches!(refused, Err(issue_dashboard_core::Error::Refused(m)) if m.contains("over 128 bytes")));
}

#[test]
fn a_reserved_token_agentbridge_does_not_know_is_started_read_only_on_its_harness() {
    let r = run();
    assert_eq!(r.step(), Stepped::Started);
    let starts = r.fake.calls_to("start");
    assert_eq!(starts.len(), 1);
    let s = &starts[0];
    assert_eq!(s["operation_id"], r.token.as_str());
    assert_eq!(s["launch_id"], "E1");
    assert_eq!(s["profile"], "Codex");
    assert_eq!(s["access"], "read-only");
    assert_eq!(
        (s["model"].as_str(), s["effort"].as_str()),
        (Some("claude-opus-5-5"), Some("medium"))
    );
    assert!(s["brief"].as_str().unwrap().contains("stage_input"), "the runner's brief: {s}");
    // The fake now holds the token, queued: no session yet.
    assert!(matches!(r.step(), Stepped::Waiting(_)));
    assert_eq!(r.state(), State::Reserved);
}

#[test]
fn a_verified_session_is_recorded_started_then_sent_the_first_stage() {
    let r = run();
    r.running(Value::Null);
    assert_eq!(r.step(), Stepped::RecordedStarted);
    assert_eq!(r.state(), State::Started);
    assert_eq!(r.step(), Stepped::Sent { stage: "type#p1".into(), round: 1 });
    let send = r.sent().pop().unwrap();
    assert_eq!(send["interaction_id"], format!("{}-type#p1-1", r.token));
    let m = &send["message"];
    assert_eq!(m["type"], "stage_input");
    assert_eq!((m["stage"].as_str(), m["round"].as_u64()), (Some("type#p1"), Some(1)));
    assert!(m["previous_result_event_id"].is_null());
    let attachments = m["attachments"].as_array().unwrap();
    assert!(!attachments.is_empty() && attachments.iter().all(|a| a["id"].is_string()), "{m}");
    assert!(!r.questions("type#p1").is_empty());
    // Until its result is accepted, nothing more is sent.
    r.running(json!({"input": m, "result": null, "result_event_id": null}));
    assert!(matches!(r.step(), Stepped::Waiting(_)));
    assert_eq!(r.sent().len(), 1);
}

#[test]
fn refused_answers_are_corrected_three_times_then_the_worker_is_archived() {
    let r = run();
    r.running(Value::Null);
    r.step();
    assert_eq!(r.step(), Stepped::Sent { stage: "type#p1".into(), round: 1 });
    let first = r.sent()[0]["message"]["attachments"].clone();
    let wrong = json!({"backend": BACKEND, "answers": [{"question": "nope", "probabilities": {}, "reason": "x"}]});
    for round in 2..=4 {
        r.answered(wrong.clone(), &format!("ev-{round}"));
        assert_eq!(r.step(), Stepped::Sent { stage: "type#p1".into(), round });
        let send = r.sent().pop().unwrap();
        assert_eq!(send["interaction_id"], format!("{}-type#p1-{round}", r.token));
        let m = &send["message"];
        assert_eq!(m["previous_result_event_id"], format!("ev-{round}"));
        assert!(!m["content"]["refused"].as_array().unwrap().is_empty(), "{m}");
        assert_eq!(m["attachments"], first, "a correction re-attaches the round's files");
    }
    r.answered(json!({"not": "answers"}), "ev-5");
    assert_eq!(r.step(), Stepped::Archived);
    assert_eq!(r.fake.calls_to("archive").len(), 1);
    assert_eq!(r.evaluated(), 0);
}

#[test]
fn each_stage_is_scored_as_it_is_accepted_and_priority_is_prepared_from_its_first_stage() {
    let r = run();
    assert_eq!(r.drive_to_finish(), STAGES.map(|s| format!("{s}#p1")));
    assert_eq!(r.evaluated(), 3, "type, area and priority; priority-first is not scored");
    let sent = r.sent();
    assert_eq!(sent.len(), 5);
    assert_eq!(sent[4]["interaction_id"], format!("{}-finish", r.token));
    // Each stage after the first names the result it follows.
    assert_eq!(sent[3]["message"]["stage"], "priority#p1");
    assert_eq!(sent[3]["message"]["previous_result_event_id"], "ev-2");
    assert!(!r.questions("priority#p1").is_empty());
}

#[test]
fn a_step_repeated_after_a_score_scores_nothing_twice_and_resends_the_same_input() {
    let r = run();
    r.running(Value::Null);
    r.step();
    r.step();
    let a = r.answers("type#p1");
    r.answered(a, "ev-1");
    let next = || Stepped::Sent {
        stage: "area#p1".into(),
        round: 1,
    };
    assert_eq!(r.step(), next());
    let once = r.sent().pop().unwrap();
    // AgentBridge has not moved on (the send was lost): the same step again.
    r.fake.calls.lock().unwrap().clear();
    assert_eq!(r.step(), next());
    assert_eq!(r.sent().pop().unwrap(), once, "the same interaction, the same content");
    assert_eq!(r.evaluated(), 1);
}

fn finished(r: &Run, state: &str, outcome: &str) {
    r.status(json!({
        "operation_id": r.token, "state": state, "native_session_id": SESSION,
        "result": {"report": {"outcome": outcome}, "usage": null},
        "routing": {"verified": true, "model": "claude-opus-5-5", "effort": "medium"},
    }));
}

#[test]
fn a_landed_result_after_every_stage_archives_the_worker_then_ends_landed() {
    let r = run();
    r.drive_to_finish();
    finished(&r, "awaiting_caller", "landed");
    assert_eq!(r.step(), Stepped::Archived);
    finished(&r, "archived", "landed");
    assert_eq!(r.step(), Stepped::Ended(EndState::Landed));
    let l = r.t.root.read(|l| launch::load(l.conn(), "E1")).unwrap().unwrap();
    assert_eq!((l.state, l.end_state), (State::Released, Some(EndState::Landed)));
    // An ended launch is past the runner: a further step does nothing.
    assert!(matches!(r.step(), Stepped::Waiting(_)));
}

#[test]
fn a_landed_result_before_every_stage_was_applied_ends_failed() {
    let r = run();
    r.running(Value::Null);
    r.step();
    finished(&r, "archived", "landed");
    assert_eq!(r.step(), Stepped::Ended(EndState::Failed));
}

#[test]
fn an_archived_worker_with_no_result_ends_failed() {
    let r = run();
    r.running(Value::Null);
    r.step();
    r.status(json!({"operation_id": r.token, "state": "archived", "result": null}));
    assert_eq!(r.step(), Stepped::Ended(EndState::Failed));
}

#[test]
fn a_token_agentbridge_forgot_after_start_is_a_rollback() {
    let r = run();
    r.running(Value::Null);
    r.step();
    r.fake.tokens.lock().unwrap().clear();
    assert!(matches!(r.step(), Stepped::Rollback(_)));
    assert!(r.fake.calls_to("start").is_empty(), "a started launch is never started again");
}

#[test]
fn a_never_started_report_releases_the_reserved_launch() {
    let r = run();
    r.status(json!({"operation_id": r.token, "state": "error", "never_started": true, "result": null}));
    assert_eq!(r.step(), Stepped::ReleasedNeverStarted);
    assert_eq!(r.state(), State::Released);
}

#[test]
fn a_routing_mismatch_archives_and_an_unverified_routing_waits() {
    let r = run();
    r.status(json!({"operation_id": r.token, "state": "started", "native_session_id": SESSION, "routing": {"verified": false}}));
    assert!(matches!(r.step(), Stepped::Waiting(_)));
    r.status(json!({
        "operation_id": r.token, "state": "started", "native_session_id": SESSION,
        "routing": {"verified": true, "model": "claude-opus-5-5", "effort": "high"},
    }));
    assert_eq!(r.step(), Stepped::Archived);
    assert_eq!(r.state(), State::Reserved, "started is never recorded on the wrong routing");
}

#[test]
fn a_question_waits_a_failed_status_call_is_an_error_and_an_unknown_state_holds() {
    let r = run();
    r.status(json!({"operation_id": r.token, "state": "waiting_user"}));
    assert!(matches!(r.step(), Stepped::Waiting(_)));
    r.fake.push("status", Answer::Down);
    assert!(r.try_step().is_err());
    r.status(json!({"operation_id": r.token, "state": "error", "reason": "preflight blocked"}));
    assert!(matches!(r.step(), Stepped::Held(_)));
}

/// The Store history keeps only its tuning items: area and priority lose every held-out
/// example, and type loses some of its questions.
fn store_held_out_gone(r: &Run) {
    let mut s = history::load(r.t.path(), STORE).unwrap().unwrap();
    s.items.retain(|i| eval::is_tuning(&i.id));
    history::save(r.t.path(), &s).unwrap();
}

#[test]
fn a_manifest_prepared_for_the_key_after_the_stage_was_sent_leaves_the_launch_s_own_alone() {
    let r = run();
    r.running(Value::Null);
    r.step();
    assert_eq!(r.step(), Stepped::Sent { stage: "type#p1".into(), round: 1 });
    let a = r.answers("type#p1");
    // `eval prepare` by hand, over a history that has since changed: the key's manifest.
    store_held_out_gone(&r);
    let c = ConfigSet::load(r.t.path()).unwrap();
    let st = runner::eval_stages(&r.t.root, &c, "home", BACKEND).unwrap().remove(0);
    let out = r.t.path().join("by-hand");
    r.t.root
        .read(|l| eval::prepare(l.conn(), r.t.path(), &c, &st.replay, st.stage, &[], &out, None))
        .unwrap();
    r.answered(a, "ev-1");
    assert_eq!(r.step(), Stepped::Sent { stage: "area#p1".into(), round: 1 });
    assert_eq!(r.evaluated(), 1);
}

#[test]
fn a_history_change_mid_launch_keeps_the_plan_recorded_at_its_first_stage() {
    let r = run();
    r.running(Value::Null);
    r.step();
    assert_eq!(r.step(), Stepped::Sent { stage: "type#p1".into(), round: 1 });
    store_held_out_gone(&r);
    assert_eq!(stage_names(&r), ["type"], "today's plan is type alone");
    let a = r.answers("type#p1");
    r.answered(a, "ev-1");
    assert_eq!(r.step(), Stepped::Sent { stage: "area#p1".into(), round: 1 }, "not an early finish");
}

#[test]
fn a_launch_whose_plan_is_empty_or_missing_never_lands() {
    let r = run();
    let dir = eval::launch_dir(r.t.path(), "home", BACKEND, "E1");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("plan.json"), "[]").unwrap();
    r.running(Value::Null);
    r.step();
    assert!(matches!(r.step(), Stepped::Held(_)));
    finished(&r, "archived", "landed");
    assert_eq!(r.step(), Stepped::Ended(EndState::Failed), "an empty plan");

    let r = run();
    r.drive_to_finish();
    let plan = eval::launch_dir(r.t.path(), "home", BACKEND, "E1").join("plan.json");
    fs::remove_file(&plan).unwrap();
    finished(&r, "archived", "landed");
    assert_eq!(r.step(), Stepped::Ended(EndState::Failed), "a missing plan");

    let r = run();
    let c = ConfigSet::load(r.t.path()).unwrap();
    let first: Vec<_> = runner::eval_stages(&r.t.root, &c, "home", BACKEND)
        .unwrap()
        .into_iter()
        .filter(|st| st.stage == issue_dashboard_core::eval::EvalStage::First)
        .collect();
    assert!(!first.is_empty());
    let dir = eval::launch_dir(r.t.path(), "home", BACKEND, "E1");
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join("plan.json"), serde_json::to_string(&first).unwrap()).unwrap();
    r.running(Value::Null);
    r.step();
    finished(&r, "archived", "landed");
    assert_eq!(r.step(), Stepped::Ended(EndState::Failed), "a plan with no scored stage");
}

#[test]
fn a_launch_with_nothing_to_evaluate_yet_starts_once_history_has_it() {
    let r = run();
    let saved: Vec<_> = [STORE, AIRZONED, RHAI].iter().map(|repo| history::load(r.t.path(), repo).unwrap().unwrap()).collect();
    for h in &saved {
        history::save(r.t.path(), &history::RepoHistory { items: Vec::new(), ..h.clone() }).unwrap();
    }
    r.running(Value::Null);
    assert_eq!(r.step(), Stepped::RecordedStarted);
    assert!(matches!(r.step(), Stepped::Held(_)), "no replay to evaluate yet");
    for h in &saved {
        history::save(r.t.path(), h).unwrap();
    }
    assert_eq!(r.step(), Stepped::Sent { stage: "type#p1".into(), round: 1 });
}

#[test]
fn a_history_change_between_a_two_stage_decision_s_stages_keeps_the_launch_going() {
    let r = run();
    r.running(Value::Null);
    r.step();
    for n in 0..3 {
        let Stepped::Sent { stage, round: 1 } = r.step() else { panic!("stage {n}") };
        if stage == "priority-first#p1" {
            store_held_out_gone(&r);
        }
        let a = r.answers(&stage);
        r.answered(a, &format!("ev-{n}"));
    }
    assert_eq!(r.step(), Stepped::Sent { stage: "priority#p1".into(), round: 1 });
}

#[test]
fn answers_to_only_some_of_the_questions_draw_a_correction_naming_the_unanswered() {
    let r = run();
    r.running(Value::Null);
    r.step();
    r.step();
    let mut a = r.answers("type#p1");
    a["answers"].as_array_mut().unwrap().remove(0);
    r.answered(a, "ev-1");
    assert_eq!(r.step(), Stepped::Sent { stage: "type#p1".into(), round: 2 });
    let refused = r.sent().pop().unwrap()["message"]["content"]["refused"].to_string();
    assert!(refused.contains("1 questions are unanswered"), "{refused}");
    assert_eq!(r.evaluated(), 0);
}

// --- Stages in parts (D6) --------------------------------------------------------------------

/// A budget that cuts every fixture stage into several parts.
const SMALL_PART: usize = 6_000;

/// Every path under the run's root ending in `tail`.
fn found(r: &Run, tail: &str) -> Vec<PathBuf> {
    fn find(dir: &std::path::Path, tail: &std::path::Path, out: &mut Vec<PathBuf>) {
        for e in fs::read_dir(dir).unwrap() {
            let p = e.unwrap().path();
            if p.ends_with(tail) {
                out.push(p.clone());
            }
            if p.is_dir() {
                find(&p, tail, out);
            }
        }
    }
    let mut out = Vec::new();
    find(r.t.path(), std::path::Path::new(tail), &mut out);
    out
}

/// The cut recorded for `stage`: its parts' question ids, in order.
fn recorded_cut(r: &Run, stage: &str) -> Vec<Vec<String>> {
    let found = found(r, &format!("parts/{stage}/cut.json"));
    assert_eq!(found.len(), 1, "one cut recorded for {stage}: {found:?}");
    serde_json::from_str(&fs::read_to_string(&found[0]).unwrap()).unwrap()
}

/// What an `evaluated` event says about the scoring, without what names the launch.
fn scores(r: &Run) -> Vec<Value> {
    r.t.root
        .read(|l| ledger::events_by_kind(l.conn(), Kind::Evaluated))
        .unwrap()
        .into_iter()
        .map(|e| {
            let d = e.data;
            json!([d["decision"], d["n"], d["n_tuning"], d["accuracy"], d["ece"], d["t"], d["uncalibratable"]])
        })
        .collect()
}

fn ids(questions: &[Value]) -> Vec<String> {
    questions.iter().map(|q| q["id"].as_str().unwrap().to_string()).collect()
}

#[test]
fn a_stage_in_parts_is_sent_part_by_part_and_scored_once_as_if_whole() {
    let whole = run();
    whole.drive_to_finish();
    let r = run();
    r.part_bytes.set(SMALL_PART);
    r.running(Value::Null);
    assert_eq!(r.step(), Stepped::RecordedStarted);
    // (part sent, scores recorded when it was sent)
    let mut sends: Vec<(String, usize)> = Vec::new();
    let mut parts_of: Vec<(String, Value)> = Vec::new();
    for n in 0..200 {
        match r.step() {
            Stepped::Sent { stage, round: 1 } => {
                sends.push((stage.clone(), r.evaluated()));
                let (_, part) = stage.rsplit_once("#p").unwrap();
                let content = r.sent().last().unwrap()["message"]["content"].clone();
                assert_eq!(content["part"], json!(part.parse::<u64>().unwrap()), "{stage}: {content}");
                parts_of.push((stage.clone(), content["of"].clone()));
                let dir = r.files.join("runner").join(&r.token).join(stage.split('#').next().unwrap()).join(format!("p{part}"));
                for e in fs::read_dir(&dir).unwrap() {
                    let v: Value = serde_json::from_str(&fs::read_to_string(e.unwrap().path()).unwrap()).unwrap();
                    assert!(!v["questions"].as_array().unwrap().is_empty(), "{stage} staged a file with no questions");
                }
                r.answered(r.answers(&stage), &format!("ev-{n}"));
            }
            Stepped::Finish => break,
            other => panic!("after {sends:?}: {other:?}"),
        }
    }
    for st in STAGES {
        let cut = recorded_cut(&r, st);
        assert!(cut.len() >= 2, "{st} is not cut at {SMALL_PART} bytes: the test tests nothing");
        let parts: Vec<&(String, usize)> = sends.iter().filter(|(s, _)| s.starts_with(&format!("{st}#p"))).collect();
        let names: Vec<String> = parts.iter().map(|(s, _)| s.clone()).collect();
        let expected: Vec<String> = (1..=cut.len()).map(|n| format!("{st}#p{n}")).collect();
        assert_eq!(names, expected, "{st}'s parts, in order, once each");
        for (part, of) in parts_of.iter().filter(|(s, _)| s.starts_with(&format!("{st}#p"))) {
            assert_eq!(*of, json!(cut.len()), "{part} names how many parts its stage has");
        }
        // Every part of a stage is sent before any of it is scored.
        assert!(parts.iter().all(|(_, e)| *e == parts[0].1), "{st} was scored before its last part: {parts:?}");
        // The parts together ask exactly what the whole stage asks, in the same order.
        let asked: Vec<String> = expected.iter().flat_map(|p| ids(&r.questions(p))).collect();
        assert_eq!(asked, ids(&whole.questions(&format!("{st}#p1"))), "{st}'s questions");
        assert_eq!(asked, cut.concat(), "{st}'s parts follow its recorded cut");
    }
    assert_eq!(scores(&r), scores(&whole), "parts score as the whole stage does");
    assert_eq!(r.evaluated(), 3);
}

fn question(id: &str, pad: usize) -> Value {
    json!({"id": id, "body": "x".repeat(pad)})
}

#[test]
fn a_cut_keeps_each_question_once_in_order_and_each_part_within_its_budget() {
    let qs: Vec<Value> = (0..40).map(|i| question(&format!("q{i}"), (i * 37) % 300)).collect();
    for budget in [1, 150, 400, 1_000, 100_000] {
        let cut = runner::cut(&qs, budget).unwrap();
        assert_eq!(cut.concat(), ids(&qs), "budget {budget}: each question once, in order");
        assert!(cut.iter().all(|p| !p.is_empty()), "budget {budget}: an empty part");
        let size = |id: &String| serde_json::to_vec(qs.iter().find(|q| q["id"] == *id.as_str()).unwrap()).unwrap().len();
        for p in &cut {
            let total: usize = p.iter().map(size).sum();
            assert!(total <= budget || p.len() == 1, "budget {budget}: part {p:?} is {total} bytes");
        }
        // Greedy: no part could have taken its successor's first question.
        for w in cut.windows(2) {
            let total: usize = w[0].iter().map(size).sum::<usize>() + size(&w[1][0]);
            assert!(total > budget, "budget {budget}: {:?} would have fit in {:?}", w[1][0], w[0]);
        }
    }
    // A question over the budget is a part alone, between parts that are not.
    let qs = vec![question("a", 10), question("big", 500), question("b", 10), question("c", 10)];
    assert_eq!(runner::cut(&qs, 100).unwrap(), vec![vec!["a"], vec!["big"], vec!["b", "c"]]);
    assert!(runner::cut(&[], 100).unwrap().is_empty());
    assert!(runner::cut(&[json!({"body": "no id"})], 100).is_err());
}

#[test]
fn a_transfer_key_is_refused_past_128_bytes() {
    let token = "t".repeat(100);
    let file = "f".repeat(128 - 100 - "-type#p1-".len());
    assert_eq!(runner::transfer_key(&token, "type#p1", &file).unwrap().len(), 128);
    assert!(runner::transfer_key(&token, "type#p1", &format!("{file}x")).is_err());
}

/// `r`, cut small, with part 1 of type accepted and part 2 sent.
fn at_second_part() -> Run {
    let r = run();
    r.part_bytes.set(SMALL_PART);
    r.running(Value::Null);
    r.step();
    assert_eq!(r.step(), Stepped::Sent { stage: "type#p1".into(), round: 1 });
    r.answered(r.answers("type#p1"), "ev-1");
    assert_eq!(r.step(), Stepped::Sent { stage: "type#p2".into(), round: 1 });
    r
}

#[test]
fn an_answer_to_a_question_of_another_part_draws_a_correction() {
    let r = run();
    r.part_bytes.set(SMALL_PART);
    r.running(Value::Null);
    r.step();
    r.step();
    let elsewhere = recorded_cut(&r, "type")[1][0].clone();
    let mut a = r.answers("type#p1");
    let mut stray = a["answers"][0].clone();
    stray["question"] = json!(elsewhere);
    a["answers"].as_array_mut().unwrap().push(stray);
    r.answered(a, "ev-1");
    assert_eq!(r.step(), Stepped::Sent { stage: "type#p1".into(), round: 2 });
    let refused = r.sent().pop().unwrap()["message"]["content"]["refused"].to_string();
    assert!(refused.contains("not in this part") && refused.contains(&elsewhere), "{refused}");
}

#[test]
fn a_repeated_step_mid_stage_resends_the_same_part_and_follows_the_recorded_cut() {
    let r = at_second_part();
    let once = r.sent().pop().unwrap();
    let cut = recorded_cut(&r, "type");
    // A restart under another budget: the recorded cut still rules.
    r.part_bytes.set(usize::MAX);
    r.fake.calls.lock().unwrap().clear();
    assert_eq!(r.step(), Stepped::Sent { stage: "type#p2".into(), round: 1 });
    assert_eq!(r.sent().pop().unwrap(), once, "the same interaction, the same content");
    assert_eq!(recorded_cut(&r, "type"), cut);
    assert_eq!(ids(&r.questions("type#p2")), cut[1]);
    assert_eq!(r.evaluated(), 0);
}

#[test]
fn a_part_refused_a_fourth_time_archives_the_worker() {
    let r = at_second_part();
    let wrong = json!({"backend": BACKEND, "answers": []});
    for round in 2..=4 {
        r.answered(wrong.clone(), &format!("ev-{round}"));
        assert_eq!(r.step(), Stepped::Sent { stage: "type#p2".into(), round });
    }
    r.answered(wrong, "ev-5");
    assert_eq!(r.step(), Stepped::Archived);
    assert_eq!(r.evaluated(), 0);
    assert!(found(&r, "parts/type/2.json").is_empty(), "a refused part was kept");
}

#[test]
fn a_scored_stage_repeated_does_not_read_its_parts_again() {
    let r = run();
    r.part_bytes.set(SMALL_PART);
    r.running(Value::Null);
    r.step();
    assert_eq!(r.step(), Stepped::Sent { stage: "type#p1".into(), round: 1 });
    let parts = recorded_cut(&r, "type").len();
    for n in 1..=parts {
        let stage = format!("type#p{n}");
        if n > 1 {
            assert_eq!(r.step(), Stepped::Sent { stage: stage.clone(), round: 1 });
        }
        r.answered(r.answers(&stage), &format!("ev-{n}"));
    }
    assert_eq!(r.step(), Stepped::Sent { stage: "area#p1".into(), round: 1 });
    // The send was lost; meanwhile an earlier part's kept answers are gone. The stage is
    // applied: nothing of it is read or scored again.
    for kept in found(&r, "parts/type/1.json") {
        fs::remove_file(kept).unwrap();
    }
    assert_eq!(r.step(), Stepped::Sent { stage: "area#p1".into(), round: 1 });
    assert_eq!(r.evaluated(), 1);
}

#[test]
fn a_stage_left_without_questions_holds_and_resumes_once_history_has_them_again() {
    let r = run();
    r.running(Value::Null);
    r.step();
    assert_eq!(r.step(), Stepped::Sent { stage: "type#p1".into(), round: 1 });
    r.answered(r.answers("type#p1"), "ev-1");
    // Every candidate leaves history after the plan was recorded.
    let saved: Vec<_> = [STORE, AIRZONED, RHAI]
        .map(|repo| history::load(r.t.path(), repo).unwrap().unwrap())
        .into_iter()
        .collect();
    for s in &saved {
        history::save(r.t.path(), &history::RepoHistory { items: Vec::new(), ..s.clone() }).unwrap();
    }
    let sent = r.sent().len();
    for _ in 0..2 {
        match r.step() {
            Stepped::Held(reason) => assert!(reason.contains("area") && reason.contains("no questions"), "{reason}"),
            other => panic!("{other:?}"),
        }
    }
    assert_eq!(r.sent().len(), sent, "nothing sent for an empty stage");
    for s in &saved {
        history::save(r.t.path(), s).unwrap();
    }
    assert_eq!(r.step(), Stepped::Sent { stage: "area#p1".into(), round: 1 }, "the empty cut was not kept");
    assert!(!r.questions("area#p1").is_empty());
}

#[test]
fn a_missing_accepted_part_stops_the_stage_unscored() {
    let r = run();
    r.part_bytes.set(SMALL_PART);
    r.running(Value::Null);
    r.step();
    assert_eq!(r.step(), Stepped::Sent { stage: "type#p1".into(), round: 1 });
    let parts = recorded_cut(&r, "type").len();
    for n in 1..=parts {
        let stage = format!("type#p{n}");
        if n > 1 {
            assert_eq!(r.step(), Stepped::Sent { stage: stage.clone(), round: 1 });
        }
        r.answered(r.answers(&stage), &format!("ev-{n}"));
    }
    let kept = found(&r, "parts/type/1.json").pop().expect("part 1 of type was kept");
    fs::remove_file(kept).unwrap();
    let sent = r.sent().len();
    let err = r.try_step().expect_err("a stage scored without its first part");
    assert!(err.to_string().contains("part 1"), "{err}");
    assert_eq!((r.evaluated(), r.sent().len()), (0, sent));
}

// --- The loop's tick (4.3)-------------------------------------------------------------------

/// A refresher whose refreshes succeed while `ok` holds, recording each project it is asked for.
fn refresher(ok: Arc<Mutex<bool>>, asked: Arc<Mutex<Vec<String>>>) -> Refresher {
    Refresher::new(Arc::new(move |project: &str| {
        asked.lock().unwrap().push(project.to_string());
        match *ok.lock().unwrap() {
            true => Ok(()),
            false => Err(issue_dashboard_core::Error::GitHub("gh: rate limited".into())),
        }
    }))
}

fn tick_at(r: &Run, refresher: &mut Refresher, at: DateTime<Utc>) -> Tick {
    let clock = move || at;
    let cx = Ctx {
        root: &r.t.root,
        bridge: &r.fake,
        files: &r.files,
        now: &clock,
        part_bytes: r.part_bytes.get(),
    };
    service::tick(&cx, refresher).unwrap()
}

fn launch_reasons(r: &Run) -> Vec<String> {
    r.t.root
        .read(|l| runner_status::held(l.conn()))
        .unwrap()
        .into_iter()
        .map(|h| h.reason)
        .collect()
}

#[test]
fn a_reserved_eval_starts_only_once_its_history_refresh_is_done() {
    let r = run();
    let (ok, asked) = (Arc::new(Mutex::new(true)), Arc::new(Mutex::new(Vec::new())));
    let mut rf = refresher(ok, asked.clone());
    let t = tick_at(&r, &mut rf, now());
    assert_eq!(t.stepped, vec![], "nothing is started before the refresh");
    assert!(r.fake.calls_to("start").is_empty());
    rf.settle(std::time::Duration::from_secs(10), now());
    let t = tick_at(&r, &mut rf, now());
    assert_eq!(t.stepped, vec![(r.token.clone(), Ok(Stepped::Started))]);
    assert_eq!(*asked.lock().unwrap(), vec!["home".to_string()], "one refresh, of the launch's project");
}

#[test]
fn a_failed_refresh_holds_the_eval_with_its_reason_and_is_retried_later() {
    let r = run();
    let (ok, asked) = (Arc::new(Mutex::new(false)), Arc::new(Mutex::new(Vec::new())));
    let mut rf = refresher(ok.clone(), asked.clone());
    tick_at(&r, &mut rf, now());
    rf.settle(std::time::Duration::from_secs(10), now());
    let t = tick_at(&r, &mut rf, now() + Duration::seconds(10));
    assert_eq!(t.stepped, vec![]);
    let reasons = launch_reasons(&r);
    assert_eq!(reasons.len(), 1, "{reasons:?}");
    assert!(reasons[0].contains("history refresh of home failed") && reasons[0].contains("rate limited"), "{reasons:?}");
    assert_eq!(asked.lock().unwrap().len(), 1, "not retried within {}s", service::REFRESH_RETRY_SECONDS);
    *ok.lock().unwrap() = true;
    let later = now() + Duration::seconds(service::REFRESH_RETRY_SECONDS + 1);
    tick_at(&r, &mut rf, later);
    rf.settle(std::time::Duration::from_secs(10), later);
    let t = tick_at(&r, &mut rf, later);
    assert_eq!(t.stepped, vec![(r.token.clone(), Ok(Stepped::Started))]);
    assert_eq!(launch_reasons(&r), Vec::<String>::new(), "the reason is cleared once it starts");
}

#[test]
fn a_rollback_found_by_a_step_stops_the_tick_before_any_other_launch_is_stepped() {
    let r = run();
    r.running(Value::Null);
    assert_eq!(r.step(), Stepped::RecordedStarted);
    // A second started runner launch, after E1, that AgentBridge still knows.
    let mut doc = launch_of("E2", LaunchKind::Eval, &[]);
    doc.routing.harness = Some("Codex".into());
    let e2 = r
        .t
        .root
        .mutate(now(), |tx| {
            launch::request(tx, &doc, Via::Ingest)?;
            let token = match launch::reserve(tx, "E2", RUNNER, &|_| Ok(2))? {
                Reservation::Reserved { token, .. } => token,
                other => panic!("expected a reservation, got {other:?}"),
            };
            launch::started(tx, &token, "native-2")?;
            Ok(token)
        })
        .unwrap();
    r.fake.tokens.lock().unwrap().clear();
    r.fake.set_status(&e2, json!({"operation_id": e2, "state": "waiting_user"}));
    let mut rf = refresher(Arc::new(Mutex::new(true)), Arc::new(Mutex::new(Vec::new())));
    let t = tick_at(&r, &mut rf, now());
    assert!(t.rollback.as_deref().is_some_and(|w| w.contains("no longer knows")), "{t:?}");
    assert_eq!(t.stepped, vec![]);
    let asked: Vec<Value> = r.fake.calls_to("status");
    assert!(asked.iter().all(|p| p["operation_id"] != json!(e2)), "E2 was stepped after the rollback: {asked:?}");
}

#[test]
fn a_reserved_eval_agentbridge_already_started_is_recorded_without_waiting_for_a_refresh() {
    let r = run();
    r.running(Value::Null);
    let asked = Arc::new(Mutex::new(Vec::new()));
    let mut rf = refresher(Arc::new(Mutex::new(false)), asked.clone());
    let t = tick_at(&r, &mut rf, now());
    assert_eq!(t.stepped, vec![(r.token.clone(), Ok(Stepped::RecordedStarted))]);
    assert_eq!(r.state(), State::Started);
    rf.settle(std::time::Duration::from_secs(10), now());
    assert!(asked.lock().unwrap().is_empty(), "a refresh only gates a start");
}

#[test]
fn the_worker_s_question_is_shown_as_the_launch_s_reason_until_it_moves_on() {
    let r = run();
    r.status(json!({"operation_id": r.token, "state": "waiting_user", "reason": "which repo?"}));
    let mut rf = refresher(Arc::new(Mutex::new(true)), Arc::new(Mutex::new(Vec::new())));
    tick_at(&r, &mut rf, now());
    assert_eq!(launch_reasons(&r), ["the worker asked a question: which repo?"]);
    r.running(Value::Null);
    tick_at(&r, &mut rf, now());
    tick_at(&r, &mut rf, now());
    let reasons = launch_reasons(&r);
    assert!(reasons.iter().all(|w| !w.contains("asked a question")), "{reasons:?}");
}

/// A `waiting_user` status as AgentBridge 0.2.4 sends it (`dashboard/store.rs` `token_status`):
/// `reason` is null and the question is the worker's last observation, stamped with when it
/// was observed, which changes on every observation of the same question.
fn asking(r: &Run, question: &str, observed_at_ms: i64) {
    r.status(asking_status(r, question, observed_at_ms, "ready"));
}

fn asking_status(r: &Run, question: &str, observed_at_ms: i64, session_state: &str) -> Value {
    json!({
        "operation_id": r.token, "state": "waiting_user", "session_state": session_state, "reason": null,
        "question": {"observed_at_ms": observed_at_ms, "state": "waiting_user", "source": "opencode",
                     "detail": "permission", "question": {"permission": question}},
    })
}

fn started_run() -> (Run, Refresher) {
    let r = run();
    r.running(Value::Null);
    assert_eq!(r.step(), Stepped::RecordedStarted);
    (r, refresher(Arc::new(Mutex::new(true)), Arc::new(Mutex::new(Vec::new()))))
}

fn blocked(t: &Tick) -> bool {
    matches!(&t.stepped[..], [(_, Ok(Stepped::Blocked(why)))] if why.contains("blocked on a prompt"))
}

/// Run A's worker sat on a permission prompt for half an hour holding the only read-only slot.
#[test]
fn a_worker_waiting_on_a_prompt_past_ten_minutes_is_archived_and_its_launch_ends_failed() {
    let (r, mut rf) = started_run();
    asking(&r, "read /tmp/a.json", 1);
    tick_at(&r, &mut rf, now());
    let shown = launch_reasons(&r);
    assert!(shown.len() == 1 && shown[0].contains("read /tmp/a.json"), "the page shows the prompt: {shown:?}");
    // The same question, observed again later: only its stamp differs.
    asking(&r, "read /tmp/a.json", 2);
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(10) + Duration::seconds(1));
    assert!(blocked(&t), "{t:?}");
    assert_eq!(r.fake.calls_to("archive").len(), 1);
    r.status(json!({"operation_id": r.token, "state": "archived", "result": null}));
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(11));
    assert_eq!(t.stepped, vec![(r.token.clone(), Ok(Stepped::Ended(EndState::Failed)))]);
}

/// The bound runs from when the CURRENT question was asked: an earlier one the worker moved
/// past does not count toward it.
#[test]
fn a_question_under_ten_minutes_old_keeps_waiting_however_long_the_launch_has_run() {
    let (r, mut rf) = started_run();
    asking(&r, "read /tmp/a.json", 1);
    tick_at(&r, &mut rf, now());
    // The worker moves on, and the launch shows another reason for over ten minutes before the
    // worker asks the same question again.
    r.status(json!({"operation_id": r.token, "state": "started", "native_session_id": SESSION, "routing": {"verified": false}}));
    tick_at(&r, &mut rf, now() + Duration::minutes(2));
    assert_eq!(launch_reasons(&r), ["routing not verified yet"]);
    asking(&r, "read /tmp/a.json", 3);
    tick_at(&r, &mut rf, now() + Duration::minutes(13));
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(20));
    assert!(matches!(&t.stepped[..], [(_, Ok(Stepped::Waiting(_)))]), "{t:?}");
    assert!(r.fake.calls_to("archive").is_empty());
}

/// A question answered and the next one asked between two ticks: no tick sees the worker
/// running, and the new question starts its own ten minutes.
#[test]
fn a_new_question_asked_between_two_ticks_starts_its_own_ten_minutes() {
    let (r, mut rf) = started_run();
    asking(&r, "read /tmp/a.json", 1);
    tick_at(&r, &mut rf, now());
    asking(&r, "read /tmp/b.json", 2);
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(11));
    assert!(matches!(&t.stepped[..], [(_, Ok(Stepped::Waiting(_)))]), "{t:?}");
    assert!(r.fake.calls_to("archive").is_empty());
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(22));
    assert!(blocked(&t), "{t:?}");
}

/// A rollback re-queues the launch under the same id: its next worker starts with no question.
#[test]
fn a_rollback_forgets_the_question_the_lost_worker_was_asking() {
    let (r, mut rf) = started_run();
    asking(&r, "read /tmp/a.json", 1);
    tick_at(&r, &mut rf, now());
    let id = by_token_id(&r);
    assert!(r.t.root.read(|l| runner_status::asking(l.conn(), &id)).unwrap().is_some());
    r.fake.tokens.lock().unwrap().clear();
    assert!(matches!(r.step(), Stepped::Rollback(_)));
    assert_eq!(r.t.root.read(|l| runner_status::asking(l.conn(), &id)).unwrap(), None);
}

fn by_token_id(r: &Run) -> String {
    let all = r.t.root.read(|l| launch::all(l.conn())).unwrap();
    all.into_iter().find(|l| l.token.as_deref() == Some(r.token.as_str())).unwrap().id
}

/// Once the bound is past it stays past: a failed archive call, and AgentBridge still reporting
/// `waiting_user` while the archive it accepted is pending, are each retried at the next tick.
#[test]
fn an_expired_question_is_archived_again_each_tick_until_the_worker_is_archived() {
    let (r, mut rf) = started_run();
    asking(&r, "read /tmp/a.json", 1);
    tick_at(&r, &mut rf, now());
    r.fake.push("archive", Answer::Down);
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(11));
    assert!(matches!(&t.stepped[..], [(_, Err(_))]), "{t:?}");
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(11) + Duration::seconds(15));
    assert!(blocked(&t), "the failed archive is retried at once: {t:?}");
    r.status(asking_status(&r, "read /tmp/a.json", 3, "archive_requested"));
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(11) + Duration::seconds(30));
    assert!(blocked(&t), "still expired while the archive is pending: {t:?}");
    let ids: Vec<Value> = r.fake.calls_to("archive").iter().map(|p| p["interaction_id"].clone()).collect();
    assert!(ids.len() == 3 && ids.iter().all(|i| *i == ids[0]), "one interaction id, retried: {ids:?}");
}

/// An idle worker as AgentBridge 0.2.4 reports it (`dashboard/store.rs` `token_status`): the
/// native session is idle and no terminal report arrived, so the state is `unknown`, with a
/// reason it also gives a worker that died. Run A‴'s worker sat here waiting for its first stage.
const IDLE: &str = "native_idle_or_unavailable_without_valid_terminal_report; original identity retained";

fn idle_status(r: &Run, reason: &str, stage: Value, effort: &str) -> Value {
    json!({
        "operation_id": r.token, "state": "unknown", "task_state": "started", "session_state": "ready",
        "never_started": false, "reason": reason, "native_session_id": SESSION, "result": null,
        "stage": stage, "routing": {"verified": true, "model": "claude-opus-5-5", "effort": effort},
    })
}

fn idle(r: &Run, stage: Value) {
    r.status(idle_status(r, IDLE, stage, "medium"));
}

fn idle_blocked(t: &Tick) -> bool {
    matches!(&t.stepped[..], [(_, Ok(Stepped::Blocked(why)))] if why.contains("idle"))
}

#[test]
fn an_idle_worker_waiting_for_its_first_stage_is_sent_it() {
    let (r, _) = started_run();
    idle(&r, Value::Null);
    assert_eq!(r.step(), Stepped::Sent { stage: "type#p1".into(), round: 1 });
}

#[test]
fn an_idle_worker_on_a_reserved_launch_is_recorded_started() {
    let r = run();
    idle(&r, Value::Null);
    assert_eq!(r.step(), Stepped::RecordedStarted);
    assert_eq!(r.state(), State::Started);
}

/// AgentBridge has not recorded the round yet: the same input again, and the bound keeps the
/// time it was first seen idle.
#[test]
fn an_idle_worker_seen_twice_is_resent_the_same_input_and_its_bound_keeps_its_start() {
    let (r, mut rf) = started_run();
    idle(&r, Value::Null);
    tick_at(&r, &mut rf, now());
    tick_at(&r, &mut rf, now() + Duration::minutes(5));
    let ids: Vec<Value> = r.sent().iter().map(|s| s["interaction_id"].clone()).collect();
    assert!(ids.len() == 2 && ids[0] == ids[1], "{ids:?}");
    let since = r.t.root.read(|l| runner_status::asking(l.conn(), &by_token_id(&r))).unwrap();
    assert_eq!(since.map(|(_, at)| at), Some(now()));
}

/// A dead worker reports the same reason as an idle one: past the bound it is archived.
#[test]
fn a_worker_idle_past_ten_minutes_is_archived_and_its_launch_ends_failed() {
    let (r, mut rf) = started_run();
    assert_eq!(r.step(), Stepped::Sent { stage: "type#p1".into(), round: 1 });
    let m = r.sent().pop().unwrap()["message"].clone();
    idle(&r, json!({"input": m, "result": null, "result_event_id": null}));
    let t = tick_at(&r, &mut rf, now());
    assert!(matches!(&t.stepped[..], [(_, Ok(Stepped::Waiting(_)))]), "{t:?}");
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(10) + Duration::seconds(1));
    assert!(idle_blocked(&t), "{t:?}");
    let ids: Vec<Value> = r.fake.calls_to("archive").iter().map(|p| p["interaction_id"].clone()).collect();
    assert_eq!(ids, [json!(format!("{}-idle", r.token))]);
    r.status(json!({"operation_id": r.token, "state": "archived", "result": null}));
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(11));
    assert_eq!(t.stepped, vec![(r.token.clone(), Ok(Stepped::Ended(EndState::Failed)))]);
}

#[test]
fn an_idle_worker_at_exactly_ten_minutes_is_still_stepped() {
    let (r, mut rf) = started_run();
    idle(&r, Value::Null);
    tick_at(&r, &mut rf, now());
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(10));
    assert!(!idle_blocked(&t), "{t:?}");
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(10) + Duration::seconds(1));
    assert!(idle_blocked(&t), "{t:?}");
}

/// The bound runs from the first idle sighting even when that tick's send failed.
#[test]
fn a_failed_send_to_an_idle_worker_does_not_restart_its_bound() {
    let (r, mut rf) = started_run();
    idle(&r, Value::Null);
    r.fake.push("send", Answer::Down);
    let t = tick_at(&r, &mut rf, now());
    assert!(matches!(&t.stepped[..], [(_, Err(_))]), "{t:?}");
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(10) + Duration::seconds(1));
    assert!(idle_blocked(&t), "{t:?}");
}

#[test]
fn a_worker_that_wakes_and_idles_again_starts_its_own_ten_minutes() {
    let (r, mut rf) = started_run();
    idle(&r, Value::Null);
    tick_at(&r, &mut rf, now());
    r.running(Value::Null);
    tick_at(&r, &mut rf, now() + Duration::minutes(5));
    idle(&r, Value::Null);
    tick_at(&r, &mut rf, now() + Duration::minutes(6));
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(12));
    assert!(!idle_blocked(&t), "{t:?}");
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(17));
    assert!(idle_blocked(&t), "{t:?}");
}

/// A prompt replaces the idle bound, and an idle worker does not inherit the prompt's.
#[test]
fn a_question_and_an_idle_spell_each_run_their_own_bound() {
    let (r, mut rf) = started_run();
    asking(&r, "read /tmp/a.json", 1);
    tick_at(&r, &mut rf, now());
    idle(&r, Value::Null);
    tick_at(&r, &mut rf, now() + Duration::minutes(8));
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(12));
    assert!(!idle_blocked(&t), "{t:?}");
    asking(&r, "read /tmp/a.json", 2);
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(13));
    assert!(matches!(&t.stepped[..], [(_, Ok(Stepped::Waiting(_)))]), "{t:?}");
    assert!(r.fake.calls_to("archive").is_empty());
}

#[test]
fn an_unknown_state_with_another_reason_still_holds() {
    let (r, _) = started_run();
    r.status(idle_status(&r, "launch_or_result_uncertain", Value::Null, "medium"));
    assert!(matches!(r.step(), Stepped::Held(_)));
    assert!(r.sent().is_empty());
}

#[test]
fn an_idle_worker_on_the_wrong_routing_is_archived() {
    let r = run();
    r.status(idle_status(&r, IDLE, Value::Null, "high"));
    assert_eq!(r.step(), Stepped::Archived);
    assert_eq!(r.state(), State::Reserved);
}

#[test]
fn a_rollback_forgets_the_idle_spell_of_the_lost_worker() {
    let (r, mut rf) = started_run();
    idle(&r, Value::Null);
    tick_at(&r, &mut rf, now());
    let id = by_token_id(&r);
    assert!(r.t.root.read(|l| runner_status::asking(l.conn(), &id)).unwrap().is_some());
    r.fake.tokens.lock().unwrap().clear();
    assert!(matches!(r.step(), Stepped::Rollback(_)));
    assert_eq!(r.t.root.read(|l| runner_status::asking(l.conn(), &id)).unwrap(), None);
}

/// A Reserved launch idle with no session yet, whose token AgentBridge then forgets: the
/// restarted worker, first seen idle after the old deadline, gets ten minutes of its own.
#[test]
fn a_restarted_worker_does_not_inherit_the_lost_worker_s_idle_spell() {
    let r = run();
    let mut rf = refresher(Arc::new(Mutex::new(true)), Arc::new(Mutex::new(Vec::new())));
    let mut no_session = idle_status(&r, IDLE, Value::Null, "medium");
    no_session["native_session_id"] = Value::Null;
    r.status(no_session);
    tick_at(&r, &mut rf, now());
    assert_eq!(r.state(), State::Reserved);
    r.fake.tokens.lock().unwrap().clear();
    // A Reserved eval restarts once its history refresh is done.
    tick_at(&r, &mut rf, now() + Duration::minutes(11));
    rf.settle(std::time::Duration::from_secs(10), now() + Duration::minutes(11));
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(11));
    assert_eq!(t.stepped, vec![(r.token.clone(), Ok(Stepped::Started))]);
    idle(&r, Value::Null);
    let t = tick_at(&r, &mut rf, now() + Duration::minutes(12));
    assert!(!idle_blocked(&t), "{t:?}");
    assert!(r.fake.calls_to("archive").is_empty());
}

#[test]
fn a_reason_for_a_launch_that_no_longer_waits_is_forgotten() {
    let r = run();
    r.t.root
        .mutate(now(), |tx| runner_status::hold(tx.conn(), "GONE", "no free slot", now()))
        .unwrap();
    let mut rf = refresher(Arc::new(Mutex::new(true)), Arc::new(Mutex::new(Vec::new())));
    tick_at(&r, &mut rf, now());
    let held: Vec<String> =
        r.t.root.read(|l| runner_status::held(l.conn())).unwrap().into_iter().map(|h| h.launch).collect();
    assert!(!held.contains(&"GONE".to_string()), "{held:?}");
}

// --- Admission (4.2) -------------------------------------------------------------------------

const ENABLED: &str = "[runner.kinds.eval]\nenabled = true\n";

/// A fixture root whose registry also holds `runner` and a read-only pool of `capacity`
/// slots, and three of its open Store issues.
fn admitting(runner: &str, capacity: u32) -> Tracked {
    let tr = tracked();
    let repos = tr.t.path().join("repos.toml");
    let mut text = fs::read_to_string(&repos).unwrap();
    text.push_str(&format!(
        "\n{runner}\n[launch.read-only]\ncapacity = {capacity}\nworkspace = \"true\"\nrelease = \"true\"\nland = \"true\"\ngate.quick = {{ run = \"true\", minutes = 1 }}\n"
    ));
    fs::write(&repos, text).unwrap();
    tr
}

/// Requests eval launch `id` on `harness`, about `items`.
fn request(t: &TempRoot, id: &str, harness: &str, items: &[(String, String)]) {
    let mut doc = launch_of(id, LaunchKind::Eval, items);
    doc.routing.harness = Some(harness.into());
    t.root.mutate(now(), |tx| launch::request(tx, &doc, Via::Ingest)).unwrap();
}

/// One admission pass, each of `ready`'s harnesses read as ready or not.
fn admit(t: &TempRoot, ready: &[(&str, bool)]) -> Vec<String> {
    let fake = Fake::new();
    let health: HashMap<String, bool> = ready.iter().map(|(h, r)| (h.to_string(), *r)).collect();
    let files = t.path().join("bridge-files");
    let cx = Ctx {
        root: &t.root,
        bridge: &fake,
        files: &files,
        now: &now,
        part_bytes: runner::PART_BYTES,
    };
    runner::admit(&cx, &health).unwrap()
}

fn state_of(t: &TempRoot, id: &str) -> State {
    t.root.read(|l| launch::load(l.conn(), id)).unwrap().unwrap().state
}

/// Each waiting launch's recorded reason, by launch.
fn reasons(t: &TempRoot) -> Vec<(String, String)> {
    t.root
        .read(|l| runner_status::held(l.conn()))
        .unwrap()
        .into_iter()
        .map(|h| (h.launch, h.reason))
        .collect()
}

fn reason(id: &str, why: &str) -> Vec<(String, String)> {
    vec![(id.to_string(), why.to_string())]
}

#[test]
fn a_requested_launch_on_a_ready_enabled_harness_with_a_free_slot_is_reserved() {
    let tr = admitting(ENABLED, 1);
    request(&tr.t, "E1", "Codex", &[]);
    assert_eq!(admit(&tr.t, &[("Codex", true)]).len(), 1);
    assert_eq!(state_of(&tr.t, "E1"), State::Reserved);
    assert_eq!(reasons(&tr.t), vec![]);
}

/// Check 1 (no issue held) never stops an eval: a read-only kind holds no issues, so it
/// neither blocks nor is blocked (`launch::held_item`); the refusal is `reserve`'s own test.
#[test]
fn an_eval_about_an_issue_another_launch_holds_is_still_admitted() {
    let tr = admitting(ENABLED, 2);
    let item = vec![tr.items[0].clone()];
    let holder = launch_of("D1L", LaunchKind::Triage, &item);
    tr.t.root
        .mutate(now(), |tx| {
            launch::request(tx, &holder, Via::Reserve)?;
            launch::reserve(tx, "D1L", D1, &cap).map(|_| ())
        })
        .unwrap();
    request(&tr.t, "E1", "Codex", &item);
    assert_eq!(admit(&tr.t, &[("Codex", true)]).len(), 1);
    assert_eq!(state_of(&tr.t, "E1"), State::Reserved);
    assert_eq!(reasons(&tr.t), vec![]);
}

#[test]
fn a_launch_of_a_kind_the_runner_has_not_enabled_is_queued_with_that_reason() {
    let tr = admitting("[runner.kinds.eval]\nenabled = false\n", 1);
    request(&tr.t, "E1", "Codex", &[]);
    assert_eq!(admit(&tr.t, &[("Codex", true)]), Vec::<String>::new());
    assert_eq!(state_of(&tr.t, "E1"), State::Queued);
    assert_eq!(reasons(&tr.t), reason("E1", "kind disabled: [runner.kinds.eval] enabled is not true"));
}

#[test]
fn a_launch_on_a_harness_agentbridge_reports_not_ready_is_queued_with_that_reason() {
    let tr = admitting(ENABLED, 1);
    request(&tr.t, "E1", "Codex", &[]);
    assert_eq!(admit(&tr.t, &[("Codex", false)]), Vec::<String>::new());
    assert_eq!(state_of(&tr.t, "E1"), State::Queued);
    assert_eq!(reasons(&tr.t), reason("E1", "harness Codex is not ready (AgentBridge health)"));
}

#[test]
fn a_harness_at_max_active_queues_the_next_launch_until_one_ends() {
    let tr = admitting(ENABLED, 3);
    request(&tr.t, "E1", "Codex", &[]);
    request(&tr.t, "E2", "Codex", &[]);
    let tokens = admit(&tr.t, &[("Codex", true)]);
    assert_eq!(tokens.len(), 1, "max_active defaults to one");
    assert_eq!(state_of(&tr.t, "E2"), State::Queued);
    assert_eq!(reasons(&tr.t), reason("E2", "harness Codex has 1 of max_active 1 running"));
    tr.t.root
        .mutate(now(), |tx| {
            launch::started(tx, &tokens[0], "native-1")?;
            launch::ended(tx, &tokens[0], EndState::Landed)
        })
        .unwrap();
    assert_eq!(admit(&tr.t, &[("Codex", true)]).len(), 1);
    assert_eq!(state_of(&tr.t, "E2"), State::Reserved);
    assert_eq!(reasons(&tr.t), vec![], "a reserved launch's reason is cleared");
}

#[test]
fn a_full_pool_queues_the_next_launch_with_the_slot_count() {
    let tr = admitting(&format!("{ENABLED}[runner.harness.Codex]\nmax_active = 2\n"), 1);
    request(&tr.t, "E1", "Codex", &[]);
    request(&tr.t, "E2", "Codex", &[]);
    assert_eq!(admit(&tr.t, &[("Codex", true)]).len(), 1);
    assert_eq!(
        reasons(&tr.t),
        reason("E2", "no free slot in pool read-only (1 held, 0 queued ahead, capacity 1)")
    );
}

#[test]
fn a_queued_launch_the_runner_may_not_admit_does_not_block_the_one_behind_it() {
    let tr = admitting(ENABLED, 1);
    request(&tr.t, "E1", "Hermes", &[]);
    request(&tr.t, "E2", "Codex", &[]);
    assert_eq!(admit(&tr.t, &[("Hermes", false), ("Codex", true)]).len(), 1);
    assert_eq!(state_of(&tr.t, "E1"), State::Queued);
    assert_eq!(state_of(&tr.t, "E2"), State::Reserved);
    assert_eq!(reasons(&tr.t), reason("E1", "harness Hermes is not ready (AgentBridge health)"));
}
