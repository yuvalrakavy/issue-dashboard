//! Derive (10.4 step 3, 10.6): the tracker documents of one project, built from one read
//! transaction of the ledger, and written as `<root>/outbox/<project>/<seq>.json`.
//!
//! Phase 1 derives `meta`, `issues`, `acks` and `launch_status`; `suggestions`, `plans`,
//! `forks` and `metrics` are written empty until the steps that produce them exist. Every
//! document is checked against its schema before the file is written.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::{ConfigSet, View};
use crate::decision::{self, Status};
use crate::docs::{Action, DecisionValue, Priority};
use crate::event::Kind;
use crate::launch::{self, State};
use crate::ledger;
use crate::observe::ObservedData;
use crate::root::{format_time, parse_time};
use crate::schema;
use crate::snapshot::{self, DEFAULT_HISTORY_DAYS};
use crate::translate::Translation;
use crate::{Error, Result};

/// `sync_interval_minutes` when the project config does not set it (7.2).
pub const DEFAULT_SYNC_MINUTES: u32 = 5;

/// Acks are carried for this long after the cycle that consumed the decision (10.6).
pub const ACK_DAYS: i64 = 30;

/// Inside authors by association (4.1).
const INSIDE: [&str; 3] = ["OWNER", "MEMBER", "COLLABORATOR"];

/// One cycle's tracker documents, by collection.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Outbox {
    pub project: String,
    pub sequence: i64,
    pub taken_at: String,
    pub meta: Value,
    pub issues: Vec<Value>,
    pub acks: Vec<Value>,
    pub launch_status: Vec<Value>,
    pub suggestions: Vec<Value>,
    pub plans: Vec<Value>,
    pub forks: Vec<Value>,
    pub metrics: Vec<Value>,
}

fn whole_seconds(s: &str) -> Result<String> {
    Ok(format_time(parse_time(s)?))
}

fn observed(conn: &Connection, id: &str) -> Result<Option<ObservedData>> {
    ledger::latest(conn, id, Kind::Observed)?
        .map(|e| Ok(serde_json::from_value(e.data)?))
        .transpose()
}

fn strings(v: &Value, field: &str) -> Vec<String> {
    v.get(field)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|s| s.as_str().map(str::to_string))
        .collect()
}

/// A pull request's one status, the first of 4.4's list that holds.
fn pr_status(pr: &Value) -> &'static str {
    let s = |f: &str| pr.get(f).and_then(Value::as_str).unwrap_or("");
    if pr.get("draft").and_then(Value::as_bool).unwrap_or(false) {
        "draft"
    } else if s("mergeable") == "CONFLICTING" {
        "conflicting"
    } else if matches!(s("checks"), "FAILURE" | "ERROR") {
        "checks-failing"
    } else if s("review_decision") == "CHANGES_REQUESTED" && changes_requested_on_head(pr) {
        "waiting-on-author"
    } else if matches!(s("checks"), "PENDING" | "EXPECTED") {
        "checks-running"
    } else if s("review_decision") == "APPROVED" {
        "approved"
    } else {
        "needs-review"
    }
}

/// Whether the changes requested are on the latest push (4.4: "since the last push"): a
/// change-requesting review of the head commit. A read without the reviews (an older
/// observation) keeps GitHub's aggregate decision.
fn changes_requested_on_head(pr: &Value) -> bool {
    match pr.get("changes_requested_on").and_then(Value::as_array) {
        Some(oids) => pr.get("head").is_some_and(|h| !h.is_null() && oids.contains(h)),
        None => true,
    }
}

/// What derive needs to know about one item besides its own observation.
struct Context<'a> {
    conn: &'a Connection,
    translation: Translation,
    maintainers: &'a [String],
    limits: BTreeMap<Priority, u32>,
    project_priorities: &'a BTreeMap<String, Priority>,
    overrides: &'a BTreeMap<String, (DecisionValue, Option<String>)>,
    taken_at: DateTime<Utc>,
    stale: bool,
}

impl Context<'_> {
    /// The current alias and state of a linked item: the ledger's latest observation of it,
    /// else what the linking item's read said.
    fn linked(&self, id: &str, content: &Value) -> Result<Option<(String, String)>> {
        if let Some(o) = observed(self.conn, id)? {
            let state = o
                .content
                .get("state")
                .and_then(Value::as_str)
                .unwrap_or("open")
                .to_string();
            return Ok(Some((o.alias, state)));
        }
        Ok(content.get("linked").and_then(|l| l.get(id)).and_then(|l| {
            Some((
                l.get("alias")?.as_str()?.to_string(),
                l.get("state")?.as_str()?.to_string(),
            ))
        }))
    }

    fn aliases(&self, ids: &[String], content: &Value) -> Result<Vec<String>> {
        let mut out = Vec::new();
        for id in ids {
            if let Some((alias, _)) = self.linked(id, content)? {
                out.push(alias);
            }
        }
        Ok(out)
    }

    fn issue(&self, id: &str, o: &ObservedData) -> Result<Value> {
        let c = &o.content;
        let labels = strings(c, "labels");
        let canon = self.translation.translate(&labels);
        let over = self.overrides.get(id);
        let type_ = over.and_then(|(v, _)| v.type_.clone()).or(canon.type_.clone());
        let areas = over.and_then(|(v, _)| v.areas.clone()).unwrap_or(canon.areas.clone());
        let override_priority = over.and_then(|(v, reason)| {
            v.priority
                .map(|p| json!({"value": p, "reason": reason.clone().unwrap_or_else(|| "set on the dashboard".into())}))
        });
        let project = self.project_priorities.get(id).copied();
        let (value, source) = match (over.and_then(|(v, _)| v.priority), project, canon.priority) {
            (Some(p), _, _) => (Some(p), Some("override")),
            (None, Some(p), _) => (Some(p), Some("project")),
            (None, None, Some(p)) => (Some(p), Some("label")),
            _ => (None, None),
        };

        let association = c.get("author_association").and_then(Value::as_str).unwrap_or("NONE");
        let author = c.get("author").and_then(Value::as_str).unwrap_or("ghost").to_string();
        let outside = !INSIDE.contains(&association) && !self.maintainers.contains(&author);
        let state = c.get("state").and_then(Value::as_str).unwrap_or("open").to_string();
        let links = c.get("links").cloned().unwrap_or(Value::Null);
        let blocked_by = strings(&links, "blocked_by");
        let modifiers = &canon.modifiers;
        let has = |m: &str| modifiers.iter().any(|x| x == m);

        let status = if o.item == crate::observe::ItemKind::PullRequest {
            pr_status(c.get("pull_request").unwrap_or(&Value::Null))
        } else if has("umbrella") {
            "umbrella"
        } else if outside && state == "open" && !self.answered_inside(c, &labels) {
            "new"
        } else if type_.is_none() || value.is_none() || (self.translation.declares_areas() && areas.is_empty()) {
            "untriaged"
        } else if has("needs_decision") {
            "needs-decision"
        } else if has("in_progress") {
            "in-progress"
        } else if self.any_open(&blocked_by, c)? {
            "blocked"
        } else {
            "ready"
        };

        let updated = parse_time(&o.updated_at)?;
        let idle_days = (self.taken_at - updated).num_days().max(0);
        let limit_breached = state == "open"
            && value
                .and_then(|p| self.limits.get(&p))
                .is_some_and(|days| idle_days > *days as i64);
        let parent = match links.get("parent").and_then(Value::as_str) {
            Some(p) => self.linked(p, c)?.map(|(a, _)| a),
            None => None,
        };
        let created = c.get("created_at").and_then(Value::as_str).unwrap_or(&o.updated_at);
        Ok(json!({
            "id": id,
            "alias": o.alias,
            "kind": if o.item == crate::observe::ItemKind::PullRequest { "pull_request" } else { "issue" },
            "title": c.get("title").and_then(Value::as_str).unwrap_or(""),
            "state": state,
            "type": type_,
            "areas": areas,
            "modifiers": canon.modifiers,
            "priority": {
                "value": value,
                "source": source,
                "label": canon.priority,
                "project": project,
                "override": override_priority,
            },
            "status": status,
            "created_at": whole_seconds(created)?,
            "updated_at": whole_seconds(&o.updated_at)?,
            "idle_days": idle_days,
            "limit_breached": limit_breached,
            "links": {
                "blocked_by": self.aliases(&blocked_by, c)?,
                "blocks": self.aliases(&strings(&links, "blocking"), c)?,
                "parent": parent,
                "children": self.aliases(&strings(&links, "sub_issues"), c)?,
            },
            "commits": [],
            "author": author,
            "outside": outside,
            "rank": null,
            "score": null,
            "untranslated_labels": canon.untranslated,
            "labels": labels,
            "body": c.get("body").cloned().unwrap_or(Value::Null),
            "body_truncated": c.get("body_truncated").and_then(Value::as_bool).unwrap_or(false),
            "stale": self.stale,
        }))
    }

    /// Whether an inside author has acted on an outside issue (4.3): commented, or (as far as
    /// the read shows) labelled it.
    fn answered_inside(&self, c: &Value, labels: &[String]) -> bool {
        let commented = c
            .get("comments")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|cm| {
                let a = cm.get("association").and_then(Value::as_str).unwrap_or("NONE");
                let who = cm.get("author").and_then(Value::as_str).unwrap_or("");
                INSIDE.contains(&a) || self.maintainers.iter().any(|m| m == who)
            });
        // Only the latest comments are read: when there are more, an earlier inside comment may
        // be unseen, so the thread is not taken as unanswered.
        let read = c.get("comments").and_then(Value::as_array).map_or(0, Vec::len) as u64;
        let unseen = c.get("comment_count").and_then(Value::as_u64).unwrap_or(0) > read;
        commented || unseen || !labels.is_empty()
    }

    fn any_open(&self, ids: &[String], content: &Value) -> Result<bool> {
        for id in ids {
            if let Some((_, state)) = self.linked(id, content)?
                && state == "open"
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
}

fn limits(config: &ConfigSet, project: &str) -> Result<BTreeMap<Priority, u32>> {
    let mut out = BTreeMap::new();
    if let Some(t) = config.project(project)?.time_limits_days.as_ref() {
        for (p, d) in [
            (Priority::P0, t.p0),
            (Priority::P1, t.p1),
            (Priority::P2, t.p2),
            (Priority::P3, t.p3),
        ] {
            if let Some(d) = d {
                out.insert(p, d);
            }
        }
    }
    Ok(out)
}

/// The read state of one configured repo, for `meta` (2.5).
fn repo_meta(
    conn: &Connection,
    project: &str,
    view: &View,
    sync_minutes: u32,
    taken_at: DateTime<Utc>,
) -> Result<(Value, Option<String>, bool)> {
    let state = snapshot::read_state(conn, project, &view.repo)?;
    let node = match state.as_ref().and_then(|s| s.repo_node.clone()) {
        Some(n) => Some(n),
        None => snapshot::repo_node_of(conn, &view.repo)?,
    };
    let good_at = state.as_ref().and_then(|s| s.good_at.clone());
    let error = state.as_ref().and_then(|s| s.error.clone());
    let stale = error.is_some()
        || match &good_at {
            None => true,
            Some(g) => taken_at - parse_time(g)? > Duration::minutes(2 * sync_minutes as i64),
        };
    let permission = match &node {
        Some(n) => observed(conn, n)?.and_then(|o| {
            o.content
                .get("viewer_permission")
                .and_then(Value::as_str)
                .map(str::to_string)
        }),
        None => None,
    };
    let read_only = if view.read_only {
        json!({"value": true, "why": "declared"})
    } else if matches!(permission.as_deref(), Some("READ" | "TRIAGE" | "NONE")) {
        json!({"value": true, "why": "no_write_access"})
    } else {
        json!({"value": false, "why": null})
    };
    let mut m = json!({
        "repo": view.repo,
        "read_at": good_at.as_deref().map(whole_seconds).transpose()?,
        "read": state.as_ref().and_then(|s| s.mode.clone()).unwrap_or_else(|| "full".into()),
        "error": error,
        "stale": stale,
        "read_only": read_only,
    });
    if let Some(n) = &node {
        m["id"] = json!(n);
    }
    Ok((m, node, stale))
}

fn acks(conn: &Connection, project: &str, taken_at: DateTime<Utc>) -> Result<Vec<Value>> {
    let outboxes: Vec<(i64, DateTime<Utc>)> = ledger::events_by_kind(conn, Kind::Outbox)?
        .into_iter()
        .filter(|e| e.subject == project)
        .map(|e| Ok((e.seq, parse_time(&e.time)?)))
        .collect::<Result<_>>()?;
    let launches: BTreeMap<String, launch::Launch> =
        launch::all(conn)?.into_iter().map(|l| (l.id.clone(), l)).collect();
    let withdrawn_by = |launch_id: &str| -> Result<Option<String>> {
        Ok(ledger::events_of(conn, launch_id, Kind::Launch)?
            .into_iter()
            .find(|e| e.data.get("transition").and_then(Value::as_str) == Some("withdrawn"))
            .and_then(|e| e.data.get("decision").and_then(Value::as_str).map(str::to_string)))
    };
    let mut out = Vec::new();
    for (seq, d, status) in decision::resolved(conn, project)? {
        // The cycle that consumed it: the first outbox of the project after it was recorded.
        let Some((cycle, at)) = outboxes.iter().find(|(s, _)| *s > seq) else {
            continue;
        };
        if taken_at - *at > Duration::days(ACK_DAYS) {
            continue;
        }
        let (result, reason) = match status {
            Status::Applied if d.action == Action::CancelLaunch => {
                let target = d.target.launch.clone().unwrap_or_default();
                match launches.get(&target) {
                    Some(l) if withdrawn_by(&target)?.as_deref() != Some(d.id.as_str()) => (
                        "refused",
                        Some(match l.state {
                            State::Withdrawn => format!("launch {target} was already withdrawn by another decision"),
                            other => format!(
                                "launch {target} is {other}; only a requested or queued launch can be cancelled"
                            ),
                        }),
                    ),
                    _ => ("applied", None),
                }
            }
            Status::Applied => ("applied", None),
            Status::Refused { reason } => ("refused", Some(reason)),
            Status::Cancelled { by } => ("cancelled", Some(format!("undone by {by}"))),
        };
        let mut ack = json!({"id": d.id, "decision": d.id, "cycle": cycle, "result": result});
        if let Some(r) = reason.filter(|r| !r.is_empty()) {
            ack["reason"] = json!(r);
        }
        out.push(ack);
    }
    Ok(out)
}

fn launch_status(conn: &Connection, project: &str) -> Result<Vec<Value>> {
    let mut out = Vec::new();
    for l in launch::all(conn)? {
        if l.request.project != project {
            continue;
        }
        let updated = ledger::event_at(conn, l.last_seq)?.map(|e| e.time).unwrap_or_default();
        out.push(json!({
            "id": l.id,
            "launch": l.id,
            "state": l.state,
            "token": l.token,
            "session": l.session,
            "dispatcher": l.dispatcher,
            "end_state": l.end_state,
            "updated_at": updated,
            "reserved_at": l.reserved_at,
            "orphans": l.orphans,
            "needs_attention": null,
        }));
    }
    Ok(out)
}

/// Each group a member of `project` names, with the tracked repos it holds (the page's scope
/// pills and "by group" panel).
fn groups(config: &ConfigSet, project: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let p = config.project(project)?;
    let views = config.views(project)?;
    let mut out = BTreeMap::new();
    for group in p.member.iter().filter_map(|m| m.group.as_deref()) {
        let repos = p.groups.get(group).or_else(|| config.registry.groups.get(group));
        let tracked: Vec<String> = repos
            .into_iter()
            .flatten()
            .filter(|r| views.contains_key(*r))
            .cloned()
            .collect();
        out.insert(group.to_string(), tracked);
    }
    Ok(out)
}

/// Builds the outbox of `project` for the cycle whose `outbox` event has sequence `seq`.
pub fn derive(
    conn: &Connection,
    config: &ConfigSet,
    project: &str,
    seq: i64,
    taken_at: DateTime<Utc>,
) -> Result<Outbox> {
    let p = config.project(project)?;
    let history_days = p.history_days.unwrap_or(DEFAULT_HISTORY_DAYS);
    let sync_minutes = p.sync_interval_minutes.unwrap_or(DEFAULT_SYNC_MINUTES);
    let maintainers = p.maintainers.clone().unwrap_or_default();
    let limits = limits(config, project)?;
    let project_priorities = decision::project_priorities(conn, project)?;
    let overrides: BTreeMap<String, (DecisionValue, Option<String>)> = decision::overrides(conn, project)?;
    let window = taken_at - Duration::days(history_days as i64);

    let mut repos = Vec::new();
    let mut issues = Vec::new();
    let mut seen = BTreeSet::new();
    for view in config.views(project)?.into_values() {
        let (meta, node, stale) = repo_meta(conn, project, &view, sync_minutes, taken_at)?;
        repos.push(meta);
        let Some(node) = node else { continue };
        let Some((_, members)) = snapshot::membership(conn, project, &node)? else {
            continue;
        };
        let ctx = Context {
            conn,
            translation: Translation::for_repo(config, &view.repo),
            maintainers: &maintainers,
            limits: limits.clone(),
            project_priorities: &project_priorities,
            overrides: &overrides,
            taken_at,
            stale,
        };
        for id in members.all() {
            let Some(o) = observed(conn, &id)? else { continue };
            // A closed item leaves the dashboard once it is older than the history window.
            if let Some(closed) = o.content.get("closed_at").and_then(Value::as_str)
                && o.content.get("state").and_then(Value::as_str) != Some("open")
                && parse_time(closed)? < window
            {
                continue;
            }
            if seen.insert(id.clone()) {
                issues.push(ctx.issue(&id, &o)?);
            }
        }
    }
    issues.sort_by(|a, b| a["alias"].as_str().cmp(&b["alias"].as_str()));

    let open: Vec<&Value> = issues.iter().filter(|i| i["state"] == "open").collect();
    let count = |f: &dyn Fn(&Value) -> bool| open.iter().filter(|i| f(i)).count();
    let summary = json!({
        "p0_open": count(&|i| i["priority"]["value"] == "p0"),
        "waiting_on_you": count(&|i| i["status"] == "needs-decision" || (i["status"] == "needs-review" && i["outside"] == true)),
        "untriaged": count(&|i| i["status"] == "untriaged" || i["status"] == "new"),
        "over_time_limit": count(&|i| i["limit_breached"] == true),
        "open": open.len(),
    });
    let taken = format_time(taken_at);
    let meta = json!({
        "schema": 1,
        "project": project,
        "name": p.name,
        "generation": format!("gen-{seq}"),
        "sequence": seq,
        "taken_at": taken,
        "summary": summary,
        "repos": repos,
        "dispatcher_seen_at": null,
        "default_view": p.default_view,
        "groups": groups(config, project)?,
        "sync_interval_minutes": sync_minutes,
    });
    let out = Outbox {
        project: project.to_string(),
        sequence: seq,
        taken_at: taken,
        meta,
        issues,
        acks: acks(conn, project, taken_at)?,
        launch_status: launch_status(conn, project)?,
        suggestions: Vec::new(),
        plans: Vec::new(),
        forks: Vec::new(),
        metrics: Vec::new(),
    };
    validate(&out)?;
    Ok(out)
}

/// Checks every document against its schema; a failure is a bug in derive, not bad input.
pub fn validate(out: &Outbox) -> Result<()> {
    schema::validate("doc-meta", "meta document", &out.meta)?;
    for (stem, docs) in [
        ("doc-issue", &out.issues),
        ("doc-acks", &out.acks),
        ("doc-launch-status", &out.launch_status),
    ] {
        for d in docs {
            schema::validate(stem, &format!("{stem} {}", d["id"]), d)?;
        }
    }
    Ok(())
}

/// `<root>/outbox/<project>/<seq>.json`.
pub fn outbox_path(root: &Path, project: &str, seq: i64) -> PathBuf {
    root.join("outbox").join(project).join(format!("{seq}.json"))
}

/// Writes the outbox atomically: a temporary file, synced, renamed into place.
pub fn write(root: &Path, out: &Outbox) -> Result<PathBuf> {
    let path = outbox_path(root, &out.project, out.sequence);
    let dir = path
        .parent()
        .ok_or_else(|| Error::refused("an outbox path has no directory"))?;
    fs::create_dir_all(dir)?;
    let tmp = dir.join(format!(".{}.json.tmp", out.sequence));
    {
        let mut f = fs::File::create(&tmp)?;
        f.write_all(serde_json::to_string_pretty(out)?.as_bytes())?;
        f.write_all(b"\n")?;
        f.sync_all()?;
    }
    fs::rename(&tmp, &path)?;
    fs::File::open(dir)?.sync_all()?;
    Ok(path)
}
