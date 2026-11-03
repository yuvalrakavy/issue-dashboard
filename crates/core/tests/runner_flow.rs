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
}

fn run() -> Run {
    run_with(true)
}

/// `run`, with the Store issues only when `store`: without them only type has held-out
/// examples in the recorded history.
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
        .filter(|_| store)
        .collect();
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
    }
}

impl Run {
    fn try_step(&self) -> Result<Stepped> {
        let cx = Ctx {
            root: &self.t.root,
            bridge: &self.fake,
            files: &self.files,
            now: &now,
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

    /// The question files staged for `stage`.
    fn questions(&self, stage: &str) -> Vec<Value> {
        let dir = self.files.join("runner").join(&self.token).join(stage.replace('/', "_"));
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
        for n in 0..20 {
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
    assert_eq!(r.step(), Stepped::Sent { stage: "type".into(), round: 1 });
    let send = r.sent().pop().unwrap();
    assert_eq!(send["interaction_id"], format!("{}-type-1", r.token));
    let m = &send["message"];
    assert_eq!(m["type"], "stage_input");
    assert_eq!((m["stage"].as_str(), m["round"].as_u64()), (Some("type"), Some(1)));
    assert!(m["previous_result_event_id"].is_null());
    let attachments = m["attachments"].as_array().unwrap();
    assert!(!attachments.is_empty() && attachments.iter().all(|a| a["id"].is_string()), "{m}");
    assert!(!r.questions("type").is_empty());
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
    assert_eq!(r.step(), Stepped::Sent { stage: "type".into(), round: 1 });
    let first = r.sent()[0]["message"]["attachments"].clone();
    let wrong = json!({"backend": BACKEND, "answers": [{"question": "nope", "probabilities": {}, "reason": "x"}]});
    for round in 2..=4 {
        r.answered(wrong.clone(), &format!("ev-{round}"));
        assert_eq!(r.step(), Stepped::Sent { stage: "type".into(), round });
        let send = r.sent().pop().unwrap();
        assert_eq!(send["interaction_id"], format!("{}-type-{round}", r.token));
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
    assert_eq!(r.drive_to_finish(), STAGES);
    assert_eq!(r.evaluated(), 3, "type, area and priority; priority-first is not scored");
    let sent = r.sent();
    assert_eq!(sent.len(), 5);
    assert_eq!(sent[4]["interaction_id"], format!("{}-finish", r.token));
    // Each stage after the first names the result it follows.
    assert_eq!(sent[3]["message"]["stage"], "priority");
    assert_eq!(sent[3]["message"]["previous_result_event_id"], "ev-2");
    assert!(!r.questions("priority").is_empty());
}

#[test]
fn a_step_repeated_after_a_score_scores_nothing_twice_and_resends_the_same_input() {
    let r = run();
    r.running(Value::Null);
    r.step();
    r.step();
    let a = r.answers("type");
    r.answered(a, "ev-1");
    let next = || Stepped::Sent {
        stage: "area".into(),
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

// --- The loop's tick (4.3) -------------------------------------------------------------------

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
fn a_reason_for_a_launch_that_no_longer_waits_is_forgotten() {
    let r = run();
    r.t.root
        .mutate(now(), |tx| runner_status::hold(tx.conn(), "GONE", "no free slot", now()))
        .unwrap();
    let mut rf = refresher(Arc::new(Mutex::new(true)), Arc::new(Mutex::new(Vec::new())));
    tick_at(&r, &mut rf, now());
    assert_eq!(launch_reasons(&r), Vec::<String>::new());
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
