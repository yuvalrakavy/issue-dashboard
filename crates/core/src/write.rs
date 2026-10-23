//! Closing an issue from the page: phase 1's one GitHub write (10.0's exception, 10.10
//! *Closing from the page*). Deterministic, with no model: a `close` decision the owner made
//! becomes `gh issue close`, and its undo `gh issue reopen`.
//!
//! The step runs in the cycle between ingest and observe, holding the root lock across each
//! operation (10.9): the intent is committed first, then GitHub is called, then the result is
//! committed. A crash in between leaves an intent with no result, which the next step settles
//! from a fresh read before anything else.

use std::collections::{BTreeMap, HashMap};
use std::process::{Command, Stdio};
use std::time::Duration;

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::backup::Failpoint;
use crate::config::ConfigSet;
use crate::decision::{self, Status};
use crate::docs::{Action, CloseReason, CloseValue, Decision};
use crate::event::{Event, Kind};
use crate::ledger::{self, Dedupe};
use crate::observe::{ItemKind, ObservedData};
use crate::root::{Session, parse_time};
use crate::{Error, Result, derive};

/// An issue as GitHub has it right now (`gh issue view --json state,closedAt,comments`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssueNow {
    /// `OPEN` or `CLOSED`.
    pub state: String,
    pub closed_at: Option<String>,
    pub comments: Vec<CommentNow>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommentNow {
    pub author: String,
    pub at: String,
    pub body: String,
}

/// The GitHub writes of phase 1. The real one is [`GhIssues`]; tests use a fake.
pub trait IssueWriter {
    /// A fresh read of one issue.
    fn issue_now(&self, repo: &str, number: u64) -> Result<IssueNow>;
    /// Closes the issue, posting `comment` first when there is one.
    fn close(
        &self,
        repo: &str,
        number: u64,
        reason: CloseReason,
        duplicate_of: Option<u64>,
        comment: Option<&str>,
    ) -> Result<()>;
    fn reopen(&self, repo: &str, number: u64) -> Result<()>;
    /// The pause between reads in [`READ_BACK_MS`].
    fn wait(&self, d: Duration) {
        std::thread::sleep(d);
    }
}

/// The pauses before each read that follows a write: GitHub's read can lag its own write, so a
/// read that does not show it yet is tried again, about 7 s in all.
pub const READ_BACK_MS: [u64; 4] = [0, 1000, 2000, 4000];

/// The real writes, through the `gh` CLI and its stored authentication.
#[derive(Debug, Clone, Default)]
pub struct GhIssues;

fn gh(args: &[&str]) -> Result<Vec<u8>> {
    let out = Command::new("gh")
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| Error::GitHub(format!("cannot run gh: {e}")))?;
    if !out.status.success() {
        return Err(Error::GitHub(format!(
            "gh {}: exited with {}: {}",
            args.first().copied().unwrap_or(""),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        )));
    }
    Ok(out.stdout)
}

impl IssueWriter for GhIssues {
    fn issue_now(&self, repo: &str, number: u64) -> Result<IssueNow> {
        let n = number.to_string();
        let out = gh(&["issue", "view", &n, "-R", repo, "--json", "state,closedAt,comments"])?;
        let v: Value = serde_json::from_slice(&out)
            .map_err(|e| Error::GitHub(format!("gh issue view {repo}#{number}: not JSON: {e}")))?;
        let text = |v: &Value, f: &str| v.get(f).and_then(Value::as_str).unwrap_or("").to_string();
        Ok(IssueNow {
            state: text(&v, "state"),
            // gh prints a zero time for an open issue.
            closed_at: v
                .get("closedAt")
                .and_then(Value::as_str)
                .filter(|s| !s.is_empty() && !s.starts_with("0001-"))
                .map(str::to_string),
            comments: v
                .get("comments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|c| CommentNow {
                    author: c
                        .pointer("/author/login")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .to_string(),
                    at: text(c, "createdAt"),
                    body: text(c, "body"),
                })
                .collect(),
        })
    }

    fn close(
        &self,
        repo: &str,
        number: u64,
        reason: CloseReason,
        duplicate_of: Option<u64>,
        comment: Option<&str>,
    ) -> Result<()> {
        let n = number.to_string();
        let dup = duplicate_of.map(|d| d.to_string());
        let mut args = vec!["issue", "close", &n, "-R", repo, "--reason", reason.gh()];
        if let Some(d) = &dup {
            args.extend(["--duplicate-of", d]);
        }
        // An argument of its own, never through a shell.
        if let Some(c) = comment {
            args.extend(["--comment", c]);
        }
        gh(&args).map(|_| ())
    }

    fn reopen(&self, repo: &str, number: u64) -> Result<()> {
        let n = number.to_string();
        gh(&["issue", "reopen", &n, "-R", repo]).map(|_| ())
    }
}

/// The two operations a close decision can need.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Close,
    Reopen,
}

impl Op {
    fn as_str(self) -> &'static str {
        match self {
            Op::Close => "close",
            Op::Reopen => "reopen",
        }
    }
}

/// Where one operation of one decision stands.
#[derive(Debug, Clone, PartialEq)]
pub enum OpState {
    None,
    /// The intent, and no result: a crash between them.
    Intended(Event),
    Applied(Value),
    Failed(Value),
}

/// Where a close decision's writes stand.
#[derive(Debug, Clone, PartialEq)]
pub struct Progress {
    pub close: OpState,
    pub reopen: OpState,
    /// The close's intent, which a reopen reads the repo and number from.
    pub close_intent: Option<Event>,
}

impl Progress {
    /// The close's result when it was applied: `closed` or `already_closed`.
    pub fn closed(&self) -> Option<&Value> {
        match &self.close {
            OpState::Applied(d) => Some(d),
            _ => None,
        }
    }

    pub fn reopened(&self) -> bool {
        matches!(self.reopen, OpState::Applied(_))
    }
}

/// The write events of `decision`.
pub fn progress(conn: &Connection, decision: &str) -> Result<Progress> {
    let mut events: Vec<Event> = Vec::new();
    for k in [Kind::WriteIntended, Kind::WriteApplied, Kind::WriteFailed] {
        events.extend(ledger::events_of(conn, decision, k)?);
    }
    events.sort_by_key(|e| e.seq);
    let state = |op: Op| {
        let mut s = OpState::None;
        for e in events.iter().filter(|e| e.distinct["op"] == op.as_str()) {
            s = match e.kind {
                Kind::WriteIntended => OpState::Intended(e.clone()),
                Kind::WriteApplied => OpState::Applied(e.data.clone()),
                _ => OpState::Failed(e.data.clone()),
            };
        }
        s
    };
    Ok(Progress {
        close: state(Op::Close),
        reopen: state(Op::Reopen),
        close_intent: events
            .iter()
            .find(|e| e.kind == Kind::WriteIntended && e.distinct["op"] == "close")
            .cloned(),
    })
}

/// What the step did to one decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Written {
    pub decision: String,
    pub op: Op,
    /// `closed`, `already_closed`, `reopened`, `failed`, or `deferred` (GitHub could not be
    /// read to settle a crash; the next cycle tries again).
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub why: Option<String>,
}

/// The issue a close acts on, from the item's latest observation.
struct Issue {
    item: String,
    repo: String,
    number: u64,
}

fn observed(conn: &Connection, id: &str) -> Result<Option<ObservedData>> {
    ledger::latest(conn, id, Kind::Observed)?
        .map(|e| Ok(serde_json::from_value(e.data)?))
        .transpose()
}

/// The preconditions of a close (10.10). `Err` in the inner result is the refusal.
fn close_target(
    conn: &Connection,
    config: &ConfigSet,
    project: &str,
    d: &Decision,
    now: DateTime<Utc>,
) -> Result<std::result::Result<(Issue, CloseValue), String>> {
    let Some(v) = d.value.as_ref().and_then(|v| v.close.clone()) else {
        return Ok(Err("a close needs value.close".into()));
    };
    // Past the ack window the page no longer shows the close as pending (5.3), so it is not
    // made: the owner closes again if they still want it.
    if now - parse_time(&d.created_at)? > chrono::Duration::days(derive::ACK_DAYS) {
        return Ok(Err(format!(
            "the decision is older than {} days, so the page no longer showed it as pending",
            derive::ACK_DAYS
        )));
    }
    let Some(item) = d.target.item.clone() else {
        return Ok(Err("a close needs target.item".into()));
    };
    let Some(o) = observed(conn, &item)? else {
        return Ok(Err(format!("{item} has not been read from GitHub")));
    };
    if o.item != ItemKind::Issue {
        return Ok(Err(format!("{} is not an issue; only an issue can be closed", o.alias)));
    }
    let Some((repo, number)) = o
        .alias
        .split_once('#')
        .and_then(|(r, n)| Some((r.to_string(), n.parse::<u64>().ok()?)))
    else {
        return Ok(Err(format!("{} is not an owner/repo#number alias", o.alias)));
    };
    let views = config.views(project)?;
    let Some(view) = views.get(&repo) else {
        return Ok(Err(format!("{repo} is not one of {project}'s repos")));
    };
    if let Some(why) = derive::read_only(conn, project, view)? {
        let why = if why == "declared" {
            "declared"
        } else {
            "no write access"
        };
        return Ok(Err(format!("{repo} is read-only ({why})")));
    }
    match (v.reason, v.duplicate_of) {
        (CloseReason::Duplicate, None) => return Ok(Err("a duplicate needs the issue it duplicates".into())),
        (CloseReason::Duplicate, Some(n)) if n == number => {
            return Ok(Err(format!("{repo}#{number} cannot duplicate itself")));
        }
        _ => {}
    }
    Ok(Ok((Issue { item, repo, number }, v)))
}

/// Whole seconds, so a GitHub time (seconds) compares with a ledger time (milliseconds).
fn secs(s: &str) -> Result<i64> {
    Ok(parse_time(s)?.timestamp())
}

fn same_time(a: Option<&str>, b: Option<&str>) -> Result<bool> {
    Ok(match (a, b) {
        (Some(a), Some(b)) => secs(a)? == secs(b)?,
        (None, None) => true,
        _ => false,
    })
}

/// Reads the issue until `settled` holds, pausing between reads ([`READ_BACK_MS`]). Returns the
/// last read.
fn read_back(
    writer: &dyn IssueWriter,
    repo: &str,
    number: u64,
    settled: impl Fn(&IssueNow) -> bool,
) -> Result<IssueNow> {
    let mut last = Err(Error::GitHub("not read".into()));
    for ms in READ_BACK_MS {
        if ms > 0 {
            writer.wait(Duration::from_millis(ms));
        }
        last = writer.issue_now(repo, number);
        if matches!(&last, Ok(n) if settled(n)) {
            break;
        }
    }
    last
}

const SOMEONE_ELSES: &str =
    "the issue was already closed when the dashboard came to close it, so Undo would reopen someone else's close";

/// Why a reopen would undo more than the dashboard's own close, if it would (10.10).
fn reopen_refusal(now: &IssueNow, closed: &Value, own_comment: &str) -> Result<Option<String>> {
    if closed["result"] != "closed" {
        return Ok(Some(SOMEONE_ELSES.into()));
    }
    if now.state != "CLOSED" {
        return Ok(Some("the issue was reopened on GitHub since".into()));
    }
    let at = closed["closed_at"].as_str();
    if !same_time(now.closed_at.as_deref(), at)? {
        return Ok(Some("the issue was reopened and closed again on GitHub since".into()));
    }
    if let Some(at) = at {
        let since = secs(at)?;
        for c in &now.comments {
            if secs(&c.at)? > since && !(c.body == own_comment && !own_comment.is_empty()) {
                return Ok(Some(format!("{} commented after the close", c.author)));
            }
        }
    }
    Ok(None)
}

/// The write step of one cycle of `project`.
pub fn step(
    session: &mut Session<'_>,
    writer: &dyn IssueWriter,
    config: &ConfigSet,
    project: &str,
) -> Result<Vec<Written>> {
    let closes: Vec<(Decision, Status)> = decision::resolved(session.ledger().conn(), project)?
        .into_iter()
        .filter(|(_, d, _)| d.action == Action::Close)
        .map(|(_, d, s)| (d, s))
        .collect();
    let mut out = Vec::new();
    for (d, status) in closes {
        if matches!(status, Status::Refused { .. }) {
            continue;
        }
        let mut w = Step {
            session: &mut *session,
            writer,
            project,
            decision: &d.id,
            out: &mut out,
        };
        let p = progress(w.session.ledger().conn(), &d.id)?;
        // A crash's intent is settled first, whatever the decision's status now.
        if let OpState::Intended(e) = &p.close
            && !w.reconcile_close(e, &status, &d, config)?
        {
            continue;
        }
        if let OpState::Intended(e) = &p.reopen
            && !w.reconcile_reopen(e)?
        {
            continue;
        }
        let p = progress(w.session.ledger().conn(), &d.id)?;
        match status {
            Status::Applied if p.close == OpState::None => {
                let now = w.session.now();
                match close_target(w.session.ledger().conn(), config, project, &d, now)? {
                    Ok((issue, v)) => w.close(&issue, &v)?,
                    Err(why) => w.failed(Op::Close, "precondition", &why)?,
                }
            }
            Status::Cancelled { .. } if p.reopen == OpState::None => {
                if let (Some(closed), Some(intent)) = (p.closed(), &p.close_intent) {
                    w.reopen(intent, closed)?;
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

struct Step<'s, 'a, 'o> {
    session: &'s mut Session<'a>,
    writer: &'s dyn IssueWriter,
    project: &'s str,
    decision: &'s str,
    out: &'o mut Vec<Written>,
}

impl Step<'_, '_, '_> {
    fn record(&mut self, kind: Kind, op: Op, data: Value) -> Result<()> {
        let (decision, project) = (self.decision.to_string(), self.project.to_string());
        let mut data = data;
        data["project"] = json!(project);
        data["op"] = json!(op.as_str());
        self.session.commit(|tx| {
            tx.record_valid(kind, &decision, json!({"op": op.as_str()}), data, Dedupe::Any)
                .map(|_| ())
        })
    }

    fn note(&mut self, op: Op, outcome: &str, why: Option<String>) {
        self.out.push(Written {
            decision: self.decision.to_string(),
            op,
            outcome: outcome.to_string(),
            why,
        });
    }

    fn failed(&mut self, op: Op, stage: &str, why: &str) -> Result<()> {
        self.record(Kind::WriteFailed, op, json!({"stage": stage, "why": why}))?;
        self.note(op, "failed", Some(why.to_string()));
        Ok(())
    }

    fn applied(&mut self, op: Op, result: &str, closed_at: Option<String>, reconciled: bool) -> Result<()> {
        let mut data = json!({"result": result, "reconciled": reconciled});
        if op == Op::Close {
            data["closed_at"] = json!(closed_at);
        }
        self.record(Kind::WriteApplied, op, data)?;
        self.note(op, result, None);
        Ok(())
    }

    /// Closes the issue after a fresh read, and records the result. A closed issue gets no
    /// comment and is recorded `already_closed`.
    fn close(&mut self, issue: &Issue, v: &CloseValue) -> Result<()> {
        let mut intent = json!({
            "item": issue.item,
            "repo": issue.repo,
            "number": issue.number,
            "reason": v.reason,
            "comment": v.comment,
        });
        if let Some(d) = v.duplicate_of {
            intent["duplicate_of"] = json!(d);
        }
        self.record(Kind::WriteIntended, Op::Close, intent)?;
        self.session.failpoint(Failpoint::AfterWriteIntent)?;
        let now = match self.writer.issue_now(&issue.repo, issue.number) {
            Ok(n) => n,
            Err(e) => return self.failed(Op::Close, "apply", &e.to_string()),
        };
        if now.state != "OPEN" {
            return self.applied(Op::Close, "already_closed", now.closed_at, false);
        }
        let comment = Some(v.comment.as_str()).filter(|c| !c.is_empty());
        self.call_close(issue, v, comment, false)
    }

    fn call_close(&mut self, issue: &Issue, v: &CloseValue, comment: Option<&str>, reconciled: bool) -> Result<()> {
        if let Err(e) = self
            .writer
            .close(&issue.repo, issue.number, v.reason, v.duplicate_of, comment)
        {
            return self.failed(Op::Close, "apply", &e.to_string());
        }
        self.session.failpoint(Failpoint::AfterWriteCall)?;
        // The time it closed, for Undo; a read that never shows the close leaves it unknown.
        let closed_at = read_back(self.writer, &issue.repo, issue.number, |n| {
            n.state == "CLOSED" && n.closed_at.is_some()
        })
        .ok()
        .filter(|n| n.state == "CLOSED")
        .and_then(|n| n.closed_at);
        self.applied(Op::Close, "closed", closed_at, reconciled)
    }

    /// Settles a close whose intent has no result. Returns false when GitHub could not be read.
    /// An issue still open is closed only if the decision still stands and its preconditions
    /// still hold: an undo, or a repo made read-only, since the crash means no close.
    fn reconcile_close(
        &mut self,
        intent: &Event,
        status: &Status,
        decision: &Decision,
        config: &ConfigSet,
    ) -> Result<bool> {
        let d = &intent.data;
        let issue = Issue {
            item: d["item"].as_str().unwrap_or("").to_string(),
            repo: d["repo"].as_str().unwrap_or("").to_string(),
            number: d["number"].as_u64().unwrap_or(0),
        };
        let v: CloseValue = CloseValue {
            reason: serde_json::from_value(d["reason"].clone())?,
            duplicate_of: d["duplicate_of"].as_u64(),
            comment: d["comment"].as_str().unwrap_or("").to_string(),
        };
        let now = match self.writer.issue_now(&issue.repo, issue.number) {
            Ok(n) => n,
            Err(e) => {
                self.note(Op::Close, "deferred", Some(e.to_string()));
                return Ok(false);
            }
        };
        let since = secs(&intent.time)?;
        if now.state != "OPEN" {
            let ours = match &now.closed_at {
                Some(at) => secs(at)? >= since,
                None => true,
            };
            let result = if ours { "closed" } else { "already_closed" };
            self.applied(Op::Close, result, now.closed_at, true)?;
            return Ok(true);
        }
        if !matches!(status, Status::Applied) {
            self.failed(Op::Close, "precondition", "undone before the close was made")?;
            return Ok(true);
        }
        let now_t = self.session.now();
        if let Err(why) = close_target(self.session.ledger().conn(), config, self.project, decision, now_t)? {
            self.failed(Op::Close, "precondition", &why)?;
            return Ok(true);
        }
        // gh posts the comment before it closes: a comment of the intended text since the
        // intent means only the close is missing.
        let posted = !v.comment.is_empty()
            && now
                .comments
                .iter()
                .any(|c| c.body == v.comment && secs(&c.at).is_ok_and(|t| t >= since));
        let comment = Some(v.comment.as_str()).filter(|c| !c.is_empty() && !posted);
        self.call_close(&issue, &v, comment, true)?;
        Ok(true)
    }

    /// Reopens the dashboard's own close, if nobody has acted on the issue since (10.10). The
    /// check is a fresh read before the intent.
    fn reopen(&mut self, close_intent: &Event, closed: &Value) -> Result<()> {
        let (repo, number) = (
            close_intent.data["repo"].as_str().unwrap_or("").to_string(),
            close_intent.data["number"].as_u64().unwrap_or(0),
        );
        let own = close_intent.data["comment"].as_str().unwrap_or("").to_string();
        // A refusal is final, so one seen on a read that may still lag the close is read again.
        let now = match read_back(self.writer, &repo, number, |n| {
            matches!(reopen_refusal(n, closed, &own), Ok(None))
        }) {
            Ok(n) => n,
            Err(e) => {
                self.note(Op::Reopen, "deferred", Some(e.to_string()));
                return Ok(());
            }
        };
        if let Some(why) = reopen_refusal(&now, closed, &own)? {
            return self.failed(Op::Reopen, "precondition", &why);
        }
        self.record(
            Kind::WriteIntended,
            Op::Reopen,
            json!({
                "item": close_intent.data["item"],
                "repo": repo,
                "number": number,
                "closed_at": closed["closed_at"],
            }),
        )?;
        self.session.failpoint(Failpoint::AfterWriteIntent)?;
        if let Err(e) = self.writer.reopen(&repo, number) {
            return self.failed(Op::Reopen, "apply", &e.to_string());
        }
        self.session.failpoint(Failpoint::AfterWriteCall)?;
        self.applied(Op::Reopen, "reopened", None, false)
    }

    /// Settles a reopen whose intent has no result. Returns false when GitHub could not be read.
    fn reconcile_reopen(&mut self, intent: &Event) -> Result<bool> {
        let (repo, number) = (
            intent.data["repo"].as_str().unwrap_or("").to_string(),
            intent.data["number"].as_u64().unwrap_or(0),
        );
        let now = match self.writer.issue_now(&repo, number) {
            Ok(n) => n,
            Err(e) => {
                self.note(Op::Reopen, "deferred", Some(e.to_string()));
                return Ok(false);
            }
        };
        if now.state == "OPEN" {
            self.applied(Op::Reopen, "reopened", None, true)?;
        } else if same_time(now.closed_at.as_deref(), intent.data["closed_at"].as_str())? {
            match self.writer.reopen(&repo, number) {
                Ok(()) => self.applied(Op::Reopen, "reopened", None, true)?,
                Err(e) => self.failed(Op::Reopen, "apply", &e.to_string())?,
            }
        } else {
            self.failed(Op::Reopen, "apply", "the issue was closed again on GitHub since")?;
        }
        Ok(true)
    }
}

/// Each item's standing dashboard close, for its issue document (10.10): the latest close
/// decision of `project` that was applied and not reopened.
pub fn standing_closes(conn: &Connection, project: &str) -> Result<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    for (_, d, status) in decision::resolved(conn, project)? {
        if d.action != Action::Close || matches!(status, Status::Refused { .. }) {
            continue;
        }
        let p = progress(conn, &d.id)?;
        let (Some(closed), Some(intent)) = (p.closed(), &p.close_intent) else {
            continue;
        };
        if p.reopened() {
            continue;
        }
        let item = intent.data["item"].as_str().unwrap_or("").to_string();
        let applied_at = ledger::events_of(conn, &d.id, Kind::WriteApplied)?
            .into_iter()
            .find(|e| e.distinct["op"] == "close")
            .map(|e| e.time)
            .unwrap_or_default();
        // A later close that found the issue already closed (two tabs, one issue) does not hide
        // the close that closed it.
        if closed["result"] != "closed" && out.get(&item).is_some_and(|s: &Value| s["result"] == "closed") {
            continue;
        }
        let undo_open = matches!(status, Status::Applied) && p.reopen == OpState::None && closed["result"] == "closed";
        let undo_allowed = undo_open && observed_still_ours(conn, &item, closed)?;
        out.insert(
            item,
            json!({
                "decision": d.id,
                "reason": intent.data["reason"],
                "at": applied_at,
                "result": closed["result"],
                "undo_allowed": undo_allowed,
            }),
        );
    }
    Ok(out)
}

/// Whether the item's latest observation still shows the dashboard's close: closed at that
/// time, with no comment since but the close's own.
fn observed_still_ours(conn: &Connection, item: &str, closed: &Value) -> Result<bool> {
    let Some(o) = observed(conn, item)? else {
        return Ok(false);
    };
    let c = &o.content;
    if c.get("state").and_then(Value::as_str) == Some("open") {
        return Ok(false);
    }
    let now = IssueNow {
        state: "CLOSED".into(),
        closed_at: c.get("closed_at").and_then(Value::as_str).map(str::to_string),
        comments: c
            .get("comments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|cm| CommentNow {
                author: cm.get("author").and_then(Value::as_str).unwrap_or("").to_string(),
                at: cm.get("at").and_then(Value::as_str).unwrap_or("").to_string(),
                // The observation keeps no bodies: a comment after the close is never ours.
                body: String::new(),
            })
            .collect(),
    };
    Ok(reopen_refusal(&now, closed, "")?.is_none())
}

/// An ack's result and its reason.
pub type AckResult = (&'static str, Option<String>);

/// The ack of a close decision, or of an undo of one, from its writes (10.5, 10.10): `None`
/// when the decision is not a close's and its ack is the usual one; `Some(None)` while the
/// write is still pending (no ack yet); `Some(Some((result, reason)))` otherwise.
pub fn ack(
    conn: &Connection,
    d: &Decision,
    status: &Status,
    targets: &HashMap<String, Decision>,
) -> Result<Option<Option<AckResult>>> {
    match d.action {
        Action::Close => {
            let p = progress(conn, &d.id)?;
            Ok(Some(match (status, &p.close) {
                (Status::Refused { reason }, _) => Some(("refused", Some(reason.clone()))),
                (Status::Applied, OpState::Applied(v)) => Some((
                    "applied",
                    (v["result"] == "already_closed").then(|| "the issue was already closed on GitHub".to_string()),
                )),
                (Status::Applied, OpState::Failed(v)) => Some(("refused", v["why"].as_str().map(str::to_string))),
                (Status::Applied, _) => None,
                (Status::Cancelled { by }, OpState::None | OpState::Failed(_)) => {
                    Some(("cancelled", Some(format!("undone by {by}"))))
                }
                (Status::Cancelled { by }, OpState::Applied(_)) => match &p.reopen {
                    OpState::Applied(_) => Some(("cancelled", Some(format!("undone by {by}: reopened")))),
                    OpState::Failed(_) => Some(("applied", None)),
                    _ => None,
                },
                (Status::Cancelled { .. }, OpState::Intended(_)) => None,
            }))
        }
        Action::Undo => {
            let Some(target) = d.target.decision.as_ref().and_then(|t| targets.get(t)) else {
                return Ok(None);
            };
            if target.action != Action::Close || *status != Status::Applied {
                return Ok(None);
            }
            let p = progress(conn, &target.id)?;
            Ok(Some(match (&p.close, &p.reopen) {
                (OpState::Applied(_), OpState::Applied(_)) => Some(("applied", None)),
                (OpState::Applied(_), OpState::Failed(v)) => Some(("refused", v["why"].as_str().map(str::to_string))),
                (OpState::Applied(_), _) | (OpState::Intended(_), _) => None,
                _ => Some(("applied", None)),
            }))
        }
        _ => Ok(None),
    }
}
