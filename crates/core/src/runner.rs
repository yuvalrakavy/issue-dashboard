//! The bridge runner's step (runner spec 2.1, 3.1; as-built D1-D5): what to do next for one
//! held runner launch, derived each time from the ledger and AgentBridge's `status(token)`
//! alone, so nothing is held only in memory and re-running a step never doubles an effect
//! (3.3). The service loop calls [`step`] for a token on each tick and on each `wait` event
//! that names it.
//!
//! An eval is staged (D2): one stage per replay key, `<decision>[-first][@<repo>]`. Each stage's
//! accepted result is checked and scored before the next is sent (D3): a refusal is a
//! correction round, at most [`MAX_CORRECTIONS`] of them. After the last stage the worker is
//! asked to finish (D5).

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::bridge::{self, Bridge, Status, TokenStatus};
use crate::config::{ConfigSet, RUNNER_MAX_ACTIVE};
use crate::docs::LaunchKind;
use crate::eval::{self, EvalAnswers, EvalStage, Replay};
use crate::launch::{self, EndState, Launch, Reservation, State};
use crate::{Error, Result, Root, runner_status};

/// Corrections after a stage's first round (2.5); the next refusal ends the launch failed.
pub const MAX_CORRECTIONS: u32 = 3;

/// How long a worker may wait on one question (a permission prompt arrives as one) before
/// the runner archives it: an unanswered prompt otherwise holds its pool slot forever.
pub const QUESTION_BOUND_MINUTES: i64 = 10;

/// How long one step waits for a staged file to be ready before it leaves it to the next.
const STAGING_WAIT: Duration = Duration::from_secs(30);

/// What a step works with.
pub struct Ctx<'a> {
    pub root: &'a Root,
    pub bridge: &'a dyn Bridge,
    /// The connector's `state_dir/files`: AgentBridge stages only files under it.
    pub files: &'a Path,
    pub now: &'a dyn Fn() -> DateTime<Utc>,
}

/// What one step did.
#[derive(Debug, Clone, PartialEq)]
pub enum Stepped {
    /// The start was sent (3.1 row 2).
    Started,
    /// `started` was recorded (row 8).
    RecordedStarted,
    /// A stage round was sent.
    Sent { stage: String, round: u32 },
    /// The worker was asked to finish (D5).
    Finish,
    /// The worker was archived; the next step ends the launch.
    Archived,
    /// The worker sat on one question past [`QUESTION_BOUND_MINUTES`] and was archived; the
    /// next step ends the launch failed, freeing its slot.
    Blocked(String),
    Ended(EndState),
    /// Released as never started (row 6).
    ReleasedNeverStarted,
    /// Nothing to do until AgentBridge answers differently.
    Waiting(String),
    /// Held for the owner: only Cancel acts (row 11).
    Held(String),
    /// AgentBridge no longer knows a started token: stop the runner (3.4).
    Rollback(String),
}

/// AgentBridge's active stage round (`dashboard/stages.rs` `Round`).
#[derive(Debug, Clone, Deserialize)]
struct Round {
    input: RoundInput,
    #[serde(default)]
    result: Option<RoundResult>,
    #[serde(default)]
    result_event_id: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
struct RoundInput {
    stage: String,
    round: u32,
}

#[derive(Debug, Clone, Deserialize)]
struct RoundResult {
    content: Value,
}

/// One stage of an eval (D2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Stage {
    pub name: String,
    pub replay: Replay,
    pub stage: EvalStage,
}

/// The stages of an eval of `project` under `backend`, in order.
pub fn eval_stages(root: &Root, config: &ConfigSet, project: &str, backend: &str) -> Result<Vec<Stage>> {
    let replays = root.read(|l| eval::replays(l.conn(), root.dir(), config, project, backend))?;
    let mut out = Vec::new();
    for r in replays {
        let at = r.repo.as_ref().map(|repo| format!("@{repo}")).unwrap_or_default();
        if eval::two_stage(&r.decision)? {
            out.push(Stage {
                name: format!("{}-first{at}", r.decision),
                replay: r.clone(),
                stage: EvalStage::First,
            });
        }
        out.push(Stage {
            name: format!("{}{at}", r.decision),
            replay: r,
            stage: EvalStage::Scored,
        });
    }
    Ok(out)
}

/// The runner's admission checks 3-5 (4.2), in order, over `health` read live and `all`, the
/// launches as they stand: the kind is enabled, the harness is healthy, and the harness's
/// `max_active` is not reached.
pub fn admission<'a>(
    config: &'a ConfigSet,
    health: &'a HashMap<String, bool>,
    all: &'a [Launch],
) -> impl Fn(&Launch) -> Option<String> + 'a {
    let runner = config.registry.runner.clone().unwrap_or_default();
    move |l: &Launch| {
        let kind = serde_json::to_value(l.request.kind).ok()?.as_str()?.to_string();
        if !runner.kinds.get(&kind).is_some_and(|k| k.enabled) {
            return Some(format!("kind disabled: [runner.kinds.{kind}] enabled is not true"));
        }
        let harness = l.request.routing.harness.as_deref()?;
        if health.get(harness) != Some(&true) {
            return Some(format!("harness {harness} is not ready (AgentBridge health)"));
        }
        let max = runner
            .harness
            .get(harness)
            .and_then(|h| h.max_active)
            .unwrap_or(RUNNER_MAX_ACTIVE);
        let active = all
            .iter()
            .filter(|o| o.state.holds() && o.request.routing.harness.as_deref() == Some(harness))
            .count();
        (active >= max as usize).then(|| format!("harness {harness} has {active} of max_active {max} running"))
    }
}

/// The harnesses the runner has to read the health of: every runner launch's, waiting or held.
pub fn harnesses(root: &Root) -> Result<Vec<String>> {
    let mut out: Vec<String> = root
        .read(|l| launch::all(l.conn()))?
        .into_iter()
        .filter(|l| l.state.holds() || matches!(l.state, State::Requested | State::Queued))
        .filter_map(|l| l.request.routing.harness)
        .collect();
    out.sort();
    out.dedup();
    Ok(out)
}

/// One admission pass (4.2) over `health`, read live by the caller: every requested runner
/// launch is reserved or queued, then the queue is reserved from until nothing more can be.
/// Each launch that was waiting has its reason recorded for the page, or cleared once it is
/// reserved. Returns the tokens reserved.
pub fn admit(cx: &Ctx<'_>, health: &HashMap<String, bool>) -> Result<Vec<String>> {
    let config = ConfigSet::load(cx.root.dir())?;
    let all = cx.root.read(|l| launch::all(l.conn()))?;
    let waiting: Vec<String> = all
        .iter()
        .filter(|l| l.request.routing.harness.is_some() && matches!(l.state, State::Requested | State::Queued))
        .map(|l| l.id.clone())
        .collect();
    if waiting.is_empty() {
        return Ok(Vec::new());
    }
    let capacity = |recipe: &str| config.capacity(recipe);
    let mut tokens = Vec::new();
    let mut reasons: Vec<(String, String)> = Vec::new();
    let requested: Vec<String> = all
        .iter()
        .filter(|l| l.request.routing.harness.is_some() && l.state == State::Requested)
        .map(|l| l.id.clone())
        .collect();
    for id in requested {
        let all = cx.root.read(|l| launch::all(l.conn()))?;
        let ok = admission(&config, health, &all);
        let r = cx
            .root
            .mutate((cx.now)(), |tx| launch::reserve_admitted(tx, &id, launch::RUNNER, &capacity, &ok));
        match r {
            Ok((Reservation::Reserved { token, .. }, _)) => tokens.push(token),
            Ok((_, reason)) => reasons.push((id, reason.unwrap_or_else(|| "queued".into()))),
            Err(Error::Refused(reason)) => reasons.push((id, reason)),
            Err(e) => return Err(e),
        }
    }
    loop {
        let all = cx.root.read(|l| launch::all(l.conn()))?;
        let ok = admission(&config, health, &all);
        let next = cx
            .root
            .mutate((cx.now)(), |tx| launch::reserve_next_admitted(tx, launch::RUNNER, &capacity, &ok))?;
        if let Some(Reservation::Reserved { token, .. }) = next.reserved {
            tokens.push(token);
            continue;
        }
        reasons.extend(next.waiting);
        break;
    }
    let now = (cx.now)();
    cx.root.mutate(now, |tx| {
        for o in launch::all(tx.conn())?.iter().filter(|o| waiting.contains(&o.id)) {
            match reasons.iter().rev().find(|(id, _)| id == &o.id) {
                Some((_, reason)) if matches!(o.state, State::Queued | State::Requested) => {
                    runner_status::hold(tx.conn(), &o.id, reason, now)?
                }
                _ => runner_status::unhold(tx.conn(), &o.id)?,
            }
        }
        Ok(())
    })?;
    Ok(tokens)
}

fn by_token(root: &Root, token: &str) -> Result<Launch> {
    root.read(|l| launch::all(l.conn()))?
        .into_iter()
        .find(|l| l.token.as_deref() == Some(token))
        .ok_or_else(|| Error::refused(format!("no launch holds token {token}")))
}

/// One step for the runner launch holding `token` (3.1's rows, in order). A failed call to
/// AgentBridge is returned as the error: the loop holds the launch and retries (row 1).
pub fn step(cx: &Ctx<'_>, token: &str) -> Result<Stepped> {
    let l = by_token(cx.root, token)?;
    if l.request.routing.harness.is_none() {
        return Err(Error::refused(format!("launch {} is not the runner's", l.id)));
    }
    if !l.state.holds() {
        return Ok(Stepped::Waiting(format!("launch {} is {}", l.id, l.state)));
    }
    let s = match bridge::status(cx.bridge, token)? {
        TokenStatus::Unknown if l.state == State::Reserved => return start(cx, &l, token),
        TokenStatus::Unknown => {
            // A rolled-back launch's next worker must not inherit this one's question.
            cx.root.mutate((cx.now)(), |tx| runner_status::not_asking(tx.conn(), &l.id))?;
            return Ok(Stepped::Rollback(format!(
                "AgentBridge no longer knows token {token}, which launch {} recorded as {}",
                l.id, l.state
            )));
        }
        TokenStatus::Known(s) => s,
    };
    let asking = cx.root.read(|led| runner_status::asking(led.conn(), &l.id))?;
    if s.state != "waiting_user" && asking.is_some() {
        cx.root.mutate((cx.now)(), |tx| runner_status::not_asking(tx.conn(), &l.id))?;
    }
    if s.result.is_some() {
        return finished(cx, &l, token, &s);
    }
    if s.state == "archived" {
        return end(cx, token, EndState::Failed, &s);
    }
    if s.never_started {
        if l.state != State::Reserved {
            return Ok(Stepped::Held(format!(
                "AgentBridge reports token {token} never started, but launch {} is {}",
                l.id, l.state
            )));
        }
        cx.root.mutate((cx.now)(), |tx| launch::release_never_started(tx, token, &s.body))?;
        return Ok(Stepped::ReleasedNeverStarted);
    }
    match s.state.as_str() {
        "queued" => Ok(Stepped::Waiting("queued: no native session yet".into())),
        "started" | "awaiting_caller" => {
            let r = &l.request.routing;
            match (s.native_session_id.as_deref(), s.routing_matches(&r.model, &r.effort)) {
                (_, Some(false)) => archive(cx, token),
                (Some(session), Some(true)) => {
                    if l.state == State::Reserved {
                        cx.root.mutate((cx.now)(), |tx| launch::started(tx, token, session))?;
                        return Ok(Stepped::RecordedStarted);
                    }
                    match l.request.kind {
                        LaunchKind::Eval => eval_step(cx, &l, token, &s),
                        other => Ok(Stepped::Held(format!("the runner does not run {other:?} launches yet"))),
                    }
                }
                _ => Ok(Stepped::Waiting("routing not verified yet".into())),
            }
        }
        "waiting_user" => {
            // AgentBridge leaves `reason` null here and puts the question in `question`.
            let q = &s.body["question"];
            // An OpenCode permission prompt is an object: shown as its JSON, cut short.
            let shown = match [&s.reason, &q["question"], &q["detail"]].into_iter().find(|v| !v.is_null()) {
                Some(Value::String(text)) => Some(text.clone()),
                Some(v) => Some(v.to_string().chars().take(200).collect()),
                None => None,
            };
            let why = match shown {
                Some(text) => format!("the worker asked a question: {text}"),
                None => "the worker asked a question".to_string(),
            };
            // The bound runs from when THIS question was first seen: its identity is the question
            // without the stamp of when it was observed, which changes on every observation.
            let mut identity = q.clone();
            if let Some(o) = identity.as_object_mut() {
                o.remove("observed_at_ms");
            }
            let identity = if identity.is_null() { s.reason.to_string() } else { identity.to_string() };
            let now = (cx.now)();
            // Most ticks see the question already recorded; only a new one pays a commit.
            let since = match asking {
                Some((recorded, since)) if recorded == identity => since,
                _ => cx.root.mutate(now, |tx| runner_status::asked(tx.conn(), &l.id, &identity, now))?,
            };
            if now - since <= chrono::Duration::minutes(QUESTION_BOUND_MINUTES) {
                return Ok(Stepped::Waiting(why));
            }
            // Retried each tick until AgentBridge reports the worker archived: the same id and
            // payload, which AgentBridge accepts again.
            bridge::archive(cx.bridge, token, &format!("{token}-blocked"))?;
            Ok(Stepped::Blocked(format!(
                "the worker was blocked on a prompt for over {QUESTION_BOUND_MINUTES} minutes and was archived ({why})"
            )))
        }
        other => Ok(Stepped::Held(format!("AgentBridge reports {other} ({})", s.reason))),
    }
}

/// The strict start (2.2), identical every time it is sent for a token.
fn start(cx: &Ctx<'_>, l: &Launch, token: &str) -> Result<Stepped> {
    let config = ConfigSet::load(cx.root.dir())?;
    let brief = cx.root.read(|led| crate::brief::brief_with(led.conn(), &config, &l.id, "issue-dashboard"))?;
    let r = &l.request.routing;
    let s = bridge::Start {
        operation_id: token.to_string(),
        launch_id: l.id.clone(),
        issue_id: l.request.items.first().cloned().unwrap_or_else(|| l.id.clone()),
        issue_revision: l.request.created_at.clone(),
        profile: r.harness.clone().expect("a runner launch names its harness"),
        model: r.model.clone(),
        effort: r.effort.clone(),
        brief: brief.text,
        cwd: None,
        access: Some("read-only".into()),
        prepared: Value::Null,
    };
    bridge::start(cx.bridge, &s)?;
    Ok(Stepped::Started)
}

fn archive(cx: &Ctx<'_>, token: &str) -> Result<Stepped> {
    bridge::archive(cx.bridge, token, &format!("{token}-archive"))?;
    Ok(Stepped::Archived)
}

fn end(cx: &Ctx<'_>, token: &str, end_state: EndState, s: &Status) -> Result<Stepped> {
    cx.root.mutate((cx.now)(), |tx| launch::runner_ended(tx, token, end_state, &s.body))?;
    Ok(Stepped::Ended(end_state))
}

/// Row 4: a `launch_result` is in. Archive, then end with its outcome only if every stage was
/// applied, else failed (D5).
fn finished(cx: &Ctx<'_>, l: &Launch, token: &str, s: &Status) -> Result<Stepped> {
    if s.state != "archived" {
        return archive(cx, token);
    }
    let outcome = s.result.as_ref().map(|r| &r["report"]["outcome"]);
    let reported = match outcome.and_then(Value::as_str) {
        Some("landed") => EndState::Landed,
        Some("stopped") => EndState::Stopped,
        _ => EndState::Failed,
    };
    let all_applied = l.request.kind == LaunchKind::Eval && every_stage_applied(cx, l)?;
    end(cx, token, if all_applied { reported } else { EndState::Failed }, s)
}

fn backend(l: &Launch) -> String {
    crate::triage::backend(&l.request.routing)
}

/// Launch `l`'s eval plan (2.5): recorded before its first stage is sent and read back on every
/// later step, so a history or config change during the launch never adds, drops or reorders
/// a stage. `None` when none is recorded and `record` is false.
fn plan(cx: &Ctx<'_>, config: &ConfigSet, l: &Launch, record: bool) -> Result<Option<Vec<Stage>>> {
    let dir = eval::launch_dir(cx.root.dir(), &l.request.project, &backend(l), &l.id);
    let path = dir.join("plan.json");
    match fs::read_to_string(&path) {
        Ok(text) => return Ok(Some(serde_json::from_str(&text)?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(e.into()),
    }
    if !record {
        return Ok(None);
    }
    let stages = eval_stages(cx.root, config, &l.request.project, &backend(l))?;
    // Nothing to evaluate yet is not a plan: a later step looks again.
    if stages.is_empty() {
        return Ok(Some(stages));
    }
    fs::create_dir_all(&dir)?;
    let tmp = dir.join("plan.json.tmp");
    fs::write(&tmp, serde_json::to_string_pretty(&stages)? + "\n")?;
    fs::rename(&tmp, &path)?;
    Ok(Some(stages))
}

/// Whether every scored stage of `l`'s recorded plan has its receipt: never for a launch with
/// no plan, or one with no scored stage.
fn every_stage_applied(cx: &Ctx<'_>, l: &Launch) -> Result<bool> {
    let config = ConfigSet::load(cx.root.dir())?;
    let stages = match plan(cx, &config, l, false)? {
        Some(stages) if stages.iter().any(|st| st.stage == EvalStage::Scored) => stages,
        _ => return Ok(false),
    };
    for st in stages.iter().filter(|st| st.stage == EvalStage::Scored) {
        if !cx.root.read(|led| eval::applied(led.conn(), &l.id, &st.replay))? {
            return Ok(false);
        }
    }
    Ok(true)
}

/// The eval rows (D3, D4): no round yet → the first stage; a round awaiting its result →
/// nothing; an accepted result → check it, then score it or prepare the next stage from it,
/// and send the next stage, a correction, or the finish.
fn eval_step(cx: &Ctx<'_>, l: &Launch, token: &str, s: &Status) -> Result<Stepped> {
    let config = ConfigSet::load(cx.root.dir())?;
    let active = s.stage.clone().filter(|v| !v.is_null());
    let Some(stages) = plan(cx, &config, l, active.is_none())? else {
        return Ok(Stepped::Held(format!(
            "launch {} has an active stage but no recorded eval plan",
            l.id
        )));
    };
    let Some(round) = active else {
        return match stages.first() {
            Some(first) => send_stage(cx, &config, l, token, first, 1, &[], None, None),
            None => Ok(Stepped::Held(format!(
                "project {} has no replay to evaluate",
                l.request.project
            ))),
        };
    };
    let round: Round = serde_json::from_value(round)?;
    let (Some(result), Some(event)) = (round.result, round.result_event_id) else {
        return Ok(Stepped::Waiting(format!(
            "{} round {} awaits its result",
            round.input.stage, round.input.round
        )));
    };
    let Some(i) = stages.iter().position(|st| st.name == round.input.stage) else {
        return Ok(Stepped::Held(format!(
            "AgentBridge's active stage {} is not in this launch's recorded eval plan",
            round.input.stage
        )));
    };
    let st = &stages[i];
    let done = st.stage == EvalStage::Scored && cx.root.read(|led| eval::applied(led.conn(), &l.id, &st.replay))?;
    let answers = if done {
        None
    } else {
        let errors = match serde_json::from_value::<EvalAnswers>(result.content) {
            Err(e) => (vec![format!("the content is not {{backend, answers}}: {e}")], None),
            Ok(a) => (
                eval::answer_errors(cx.root.dir(), &config, &st.replay, st.stage, std::slice::from_ref(&a), &l.id)?,
                Some(a),
            ),
        };
        match errors {
            (errors, _) if !errors.is_empty() => {
                if round.input.round > MAX_CORRECTIONS {
                    return archive(cx, token);
                }
                let content = json!({"refused": errors});
                return send_stage(cx, &config, l, token, st, round.input.round + 1, &[], Some(content), Some(&event));
            }
            (_, a) => a,
        }
    };
    if let (EvalStage::Scored, Some(a)) = (st.stage, &answers) {
        cx.root.mutate((cx.now)(), |tx| {
            eval::score_for_launch(tx, cx.root.dir(), &config, &st.replay, std::slice::from_ref(a), &l.id)
        })?;
    }
    match stages.get(i + 1) {
        // Priority's scored stage is prepared from its first stage's answers.
        Some(next) if st.stage == EvalStage::First => {
            let first = answers.map(|a| vec![a]).unwrap_or_default();
            send_stage(cx, &config, l, token, next, 1, &first, None, Some(&event))
        }
        Some(next) => send_stage(cx, &config, l, token, next, 1, &[], None, Some(&event)),
        None => {
            // AgentBridge takes only an object; `text` is the field it reads a plain one by.
            let message = json!({
                "text": "Every stage is answered and accepted. Finish now: reply with a launch_result, outcome `landed`."
            });
            bridge::send(cx.bridge, token, &format!("{token}-finish"), message)?;
            Ok(Stepped::Finish)
        }
    }
}

/// The directory a stage's files are prepared into, under the connector's files.
fn stage_dir(cx: &Ctx<'_>, token: &str, st: &Stage) -> PathBuf {
    cx.files.join("runner").join(token).join(st.name.replace('/', "_"))
}

/// Sends `st`'s round `round`: its files (prepared now for round 1, the same files again for a
/// correction) and `content` (the refusals, for a correction).
#[allow(clippy::too_many_arguments)]
fn send_stage(
    cx: &Ctx<'_>,
    config: &ConfigSet,
    l: &Launch,
    token: &str,
    st: &Stage,
    round: u32,
    first: &[EvalAnswers],
    content: Option<Value>,
    previous: Option<&str>,
) -> Result<Stepped> {
    let dir = stage_dir(cx, token, st);
    if round == 1 {
        cx.root.read(|led| eval::prepare(led.conn(), cx.root.dir(), config, &st.replay, st.stage, first, &dir, Some(&l.id)))?;
    }
    let mut attachments = Vec::new();
    let mut names: Vec<PathBuf> = fs::read_dir(&dir)?.map(|e| Ok(e?.path())).collect::<Result<_>>()?;
    names.sort();
    for path in names {
        match staged(cx, token, st, &path)? {
            Some(manifest) => attachments.push(manifest),
            None => return Ok(Stepped::Waiting(format!("staging {}", path.display()))),
        }
    }
    let content = content.unwrap_or_else(|| {
        json!({"decision": st.replay.decision, "repo": st.replay.repo, "stage": st.stage})
    });
    let message = json!({
        "type": "stage_input",
        "schema_version": 1,
        "stage": st.name,
        "round": round,
        "content": content,
        "previous_result_event_id": previous,
        "attachments": attachments,
    });
    bridge::send(cx.bridge, token, &format!("{token}-{}-{round}", st.name), message)?;
    Ok(Stepped::Sent {
        stage: st.name.clone(),
        round,
    })
}

/// A staged file's transfer key, `{token}-{stage}-{file}`: refused past the connector's 128 bytes.
pub fn transfer_key(token: &str, stage: &str, file: &str) -> Result<String> {
    let key = format!("{token}-{stage}-{file}");
    if key.len() > 128 {
        return Err(Error::refused(format!("transfer key {key} is over 128 bytes")));
    }
    Ok(key)
}

/// `path` staged through AgentBridge's transfer, keyed by token, stage and file so a retry
/// reuses it: its ready manifest, or `None` while it is still being prepared.
fn staged(cx: &Ctx<'_>, token: &str, st: &Stage, path: &Path) -> Result<Option<Value>> {
    let name = path
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or_else(|| Error::refused(format!("{} has no file name", path.display())))?;
    let key = transfer_key(token, &st.name, name)?;
    let prepared = bridge::transfer(
        cx.bridge,
        json!({"action": "prepare", "key": key, "path": path, "name": name, "media_type": "application/json", "keep": false}),
    )?;
    let id = prepared["transfer_id"].clone();
    let started = Instant::now();
    loop {
        let s = bridge::transfer(cx.bridge, json!({"action": "status", "transfer_id": id, "wait_ms": 5000}))?;
        let state = s["preparation"]["state"].as_str().unwrap_or("ready");
        if state != "preparing" && !s["manifest"].is_null() {
            return Ok(Some(s["manifest"].clone()));
        }
        if state != "preparing" {
            return Err(Error::refused(format!(
                "staging {} failed: {}",
                path.display(),
                s["preparation"]["error"]
            )));
        }
        if started.elapsed() >= STAGING_WAIT {
            return Ok(None);
        }
    }
}
