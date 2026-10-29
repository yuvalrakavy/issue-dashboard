//! `eval refresh-history` (triage spec 3.2): the project's issues back to `eval_history_days`
//! (default 730), each with its comments' text and its label events (name, action, time,
//! actor), read through the [`GitHub`] trait into `<root>/state/evals/history/`, not the
//! ledger, so the page's `history_days` is unaffected.
//!
//! An unfiltered member is read through the repository's issue connection, oldest created
//! first and filtered by `since`, so the order is stable while it is read; a member with a
//! `filter` through search (ids only), then by node id. Both write the file after every page
//! with the cursor reached, so a run stopped by a rate limit or an error resumes where it
//! stopped (6.3: resumable by page cursor, never a shorter window).
//!
//! Open issues are kept too: `truth tag` (3.3) lists the label events of a bulk review, which
//! is mostly about open issues. Only closed issues are evaluated.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::config::ConfigSet;
use crate::github::{GitHub, Request};
use crate::read::BODY_CAP;
use crate::root::{format_time, parse_time};
use crate::{Error, Result};

/// How far back history reads, unless the project says (3.2).
pub const HISTORY_DAYS: u32 = 730;

/// Issues per request: each carries up to 100 comments with their text and 100 label events.
pub const HISTORY_PAGE: u32 = 25;

/// The most label events or comments an issue's history keeps; an issue with more label events
/// is skipped and reported (its truth could not be folded), more comments keeps the earliest.
pub const NESTED_CAP: u64 = 100;

const HISTORY_FIELDS: &str = "fragment HistoryFields on Issue {
  id number title state stateReason author { login } authorAssociation
  createdAt updatedAt closedAt body
  labels(first: 50) { totalCount nodes { name } }
  comments(first: 100) { totalCount nodes { author { login } authorAssociation createdAt body } }
  timelineItems(itemTypes: [LABELED_EVENT, UNLABELED_EVENT], first: 100) {
    totalCount
    nodes {
      __typename
      ... on LabeledEvent { id createdAt actor { login } label { name } }
      ... on UnlabeledEvent { id createdAt actor { login } label { name } }
    }
  }
  repository { nameWithOwner }
}";

fn issues_query() -> &'static str {
    static Q: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    Q.get_or_init(|| {
        Box::leak(
            format!(
                "query($owner: String!, $name: String!, $since: DateTime!, $first: Int!, $after: String) {{
  repository(owner: $owner, name: $name) {{
    issues(first: $first, after: $after, filterBy: {{since: $since}}, orderBy: {{field: CREATED_AT, direction: ASC}}) {{
      pageInfo {{ hasNextPage endCursor }}
      nodes {{ ...HistoryFields }}
    }}
  }}
}}
{HISTORY_FIELDS}"
            )
            .into_boxed_str(),
        )
    })
}

fn nodes_query() -> &'static str {
    static Q: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    Q.get_or_init(|| {
        Box::leak(
            format!(
                "query($ids: [ID!]!) {{
  nodes(ids: $ids) {{ __typename ... on Issue {{ ...HistoryFields }} }}
}}
{HISTORY_FIELDS}"
            )
            .into_boxed_str(),
        )
    })
}

/// A label set on or removed from an issue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LabelChange {
    /// The timeline event's node id: what `truth tag` names.
    pub id: String,
    pub label: String,
    pub added: bool,
    pub at: String,
    pub actor: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryComment {
    pub author: String,
    pub association: String,
    pub at: String,
    pub body: String,
}

/// One issue as history keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HistoryItem {
    pub id: String,
    pub alias: String,
    pub title: String,
    pub body: Option<String>,
    pub body_truncated: bool,
    pub author: String,
    pub author_association: String,
    pub state: String,
    pub state_reason: Option<String>,
    pub created_at: String,
    pub closed_at: Option<String>,
    /// The labels the issue carries now.
    pub labels: Vec<String>,
    /// The earliest [`NESTED_CAP`] comments.
    pub comments: Vec<HistoryComment>,
    /// Every label event, in time order.
    pub label_events: Vec<LabelChange>,
}

/// Where a read stopped: the issue connection's cursor, or how many of a search's ids are read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Cursor {
    Connection { after: Option<String> },
    Search { ids: Vec<String>, read: usize },
}

/// `state/evals/history/<owner>/<name>.json`: one repo's history, complete or as far as read.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RepoHistory {
    pub repo: String,
    pub window_days: u32,
    /// The window's start: issues updated before it are not read.
    pub since: String,
    pub started_at: String,
    pub complete: bool,
    /// Where the next read resumes; `None` once complete.
    pub cursor: Option<Cursor>,
    pub requests: u64,
    pub items: Vec<HistoryItem>,
    /// Issues not kept, each with why.
    pub skipped: Vec<Value>,
}

/// The history directory of a tracker root.
pub fn dir(root: &Path) -> PathBuf {
    root.join("state").join("evals").join("history")
}

/// The file of one repo's history.
pub fn file(root: &Path, repo: &str) -> PathBuf {
    let (owner, name) = repo.split_once('/').unwrap_or(("", repo));
    dir(root).join(owner).join(format!("{name}.json"))
}

/// Reads one repo's history file, if there is one.
pub fn load(root: &Path, repo: &str) -> Result<Option<RepoHistory>> {
    let path = file(root, repo);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(Some(serde_json::from_str(&text).map_err(|e| {
            Error::refused(format!("{}: not a history file: {e}", path.display()))
        })?)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Writes `h` whole, through a temporary file, so a stopped run never leaves half a file.
pub fn save(root: &Path, h: &RepoHistory) -> Result<()> {
    let path = file(root, &h.repo);
    fs::create_dir_all(path.parent().expect("a history file has a directory"))?;
    let tmp = path.with_extension("json.tmp");
    fs::write(&tmp, serde_json::to_string(h)?)?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

fn str_at<'v>(v: &'v Value, path: &[&str]) -> Option<&'v str> {
    path.iter().try_fold(v, |v, k| v.get(k))?.as_str()
}

fn nodes<'v>(v: &'v Value, field: &str) -> impl Iterator<Item = &'v Value> {
    v.get(field)
        .and_then(|c| c.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|n| !n.is_null())
}

fn total(v: &Value, field: &str) -> u64 {
    v.get(field)
        .and_then(|c| c.get("totalCount"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
}

fn time_str(v: &Value, field: &str) -> Result<Option<String>> {
    match v.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(format_time(parse_time(s)?))),
        Some(other) => Err(Error::GitHub(format!("{field} is not a time: {other}"))),
    }
}

/// Translates one issue node; `Err(why)` for an issue history cannot keep.
pub fn item(v: &Value) -> Result<std::result::Result<HistoryItem, String>> {
    let id = v
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::GitHub(format!("a node without an id: {v}")))?
        .to_string();
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(Error::GitHub(format!(
            "node id {id} is not in the current format (was X-Github-Next-Global-ID sent?)"
        )));
    }
    let repo =
        str_at(v, &["repository", "nameWithOwner"]).ok_or_else(|| Error::GitHub(format!("{id}: no repository")))?;
    let number = v
        .get("number")
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::GitHub(format!("{id}: no number")))?;
    let alias = format!("{repo}#{number}");
    if total(v, "timelineItems") > NESTED_CAP {
        return Ok(Err(format!("{alias} has more than {NESTED_CAP} label events")));
    }
    if total(v, "labels") > 50 {
        return Ok(Err(format!("{alias} has more than 50 labels")));
    }
    let mut label_events = Vec::new();
    for n in nodes(v, "timelineItems") {
        let added = match n.get("__typename").and_then(Value::as_str) {
            Some("LabeledEvent") => true,
            Some("UnlabeledEvent") => false,
            other => return Err(Error::GitHub(format!("{alias}: unexpected timeline item {other:?}"))),
        };
        // A deleted label leaves its events without one: nothing to fold.
        let Some(label) = str_at(n, &["label", "name"]) else {
            continue;
        };
        label_events.push(LabelChange {
            id: n
                .get("id")
                .and_then(Value::as_str)
                .ok_or_else(|| Error::GitHub(format!("{alias}: a label event without an id")))?
                .to_string(),
            label: label.to_string(),
            added,
            at: time_str(n, "createdAt")?
                .ok_or_else(|| Error::GitHub(format!("{alias}: a label event without a time")))?,
            actor: str_at(n, &["actor", "login"]).unwrap_or("ghost").to_string(),
        });
    }
    // The timeline is in time order; a stable sort keeps the order of one instant.
    label_events.sort_by(|a, b| a.at.cmp(&b.at));
    let body = v.get("body").and_then(Value::as_str).unwrap_or("").trim();
    let kept: String = body.chars().take(BODY_CAP).collect();
    let lower = |field: &str| v.get(field).and_then(Value::as_str).map(str::to_lowercase);
    Ok(Ok(HistoryItem {
        alias,
        title: v.get("title").and_then(Value::as_str).unwrap_or("").to_string(),
        body_truncated: kept.len() < body.len(),
        body: (!kept.is_empty()).then_some(kept),
        author: str_at(v, &["author", "login"]).unwrap_or("ghost").to_string(),
        author_association: v
            .get("authorAssociation")
            .and_then(Value::as_str)
            .unwrap_or("NONE")
            .to_string(),
        state: lower("state").unwrap_or_else(|| "open".into()),
        state_reason: lower("stateReason"),
        created_at: time_str(v, "createdAt")?.ok_or_else(|| Error::GitHub(format!("{id}: no createdAt")))?,
        closed_at: time_str(v, "closedAt")?,
        labels: nodes(v, "labels")
            .filter_map(|l| l.get("name").and_then(Value::as_str).map(str::to_string))
            .collect(),
        comments: nodes(v, "comments")
            .map(|c| -> Result<HistoryComment> {
                Ok(HistoryComment {
                    author: str_at(c, &["author", "login"]).unwrap_or("ghost").to_string(),
                    association: c
                        .get("authorAssociation")
                        .and_then(Value::as_str)
                        .unwrap_or("NONE")
                        .to_string(),
                    at: time_str(c, "createdAt")?.unwrap_or_default(),
                    body: c
                        .get("body")
                        .and_then(Value::as_str)
                        .unwrap_or("")
                        .chars()
                        .take(BODY_CAP)
                        .collect(),
                })
            })
            .collect::<Result<_>>()?,
        label_events,
        id,
    }))
}

/// Fails on any error in the document, or a document without data.
fn check(name: &str, resp: &Value) -> Result<()> {
    if let Some(errors) = resp.get("errors").and_then(Value::as_array)
        && !errors.is_empty()
    {
        let msgs: Vec<&str> = errors
            .iter()
            .map(|e| {
                e.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("an error without a message")
            })
            .collect();
        return Err(Error::GitHub(format!("{name}: {}", msgs.join("; "))));
    }
    match resp.get("data") {
        Some(Value::Object(_)) => Ok(()),
        _ => Err(Error::GitHub(format!("{name}: the response has no data"))),
    }
}

/// What one repo's refresh did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RepoReport {
    pub repo: String,
    pub complete: bool,
    pub resumed: bool,
    pub items: usize,
    pub closed_in_window: usize,
    pub requests: u64,
    pub skipped: usize,
    pub seconds: f64,
    pub error: Option<String>,
}

/// Keeps `it` in `h`, replacing an earlier read of the same issue.
fn keep(h: &mut RepoHistory, kept: std::result::Result<HistoryItem, String>, ids: &mut BTreeMap<String, usize>) {
    match kept {
        Ok(it) => match ids.get(&it.id) {
            Some(&i) => h.items[i] = it,
            None => {
                ids.insert(it.id.clone(), h.items.len());
                h.items.push(it);
            }
        },
        Err(why) => h.skipped.push(json!({"why": why})),
    }
}

/// Refreshes one repo: resumes an incomplete file, else reads afresh. Each page is written as
/// soon as it is read; an error leaves the file at the last page and is returned.
pub fn refresh_repo(
    root: &Path,
    gh: &dyn GitHub,
    repo: &str,
    filter: Option<&str>,
    window_days: u32,
    now: DateTime<Utc>,
    page: u32,
) -> Result<RepoReport> {
    let started = std::time::Instant::now();
    let previous = load(root, repo)?;
    let resumed = previous
        .as_ref()
        .is_some_and(|h| !h.complete && h.window_days == window_days);
    let mut h = match previous {
        Some(h) if resumed => h,
        _ => RepoHistory {
            repo: repo.to_string(),
            window_days,
            since: format_time(now - Duration::days(i64::from(window_days))),
            started_at: format_time(now),
            complete: false,
            cursor: None,
            requests: 0,
            items: Vec::new(),
            skipped: Vec::new(),
        },
    };
    let (owner, name) = repo
        .split_once('/')
        .ok_or_else(|| Error::GitHub(format!("{repo} is not owner/name")))?;
    let mut ids: BTreeMap<String, usize> = h.items.iter().enumerate().map(|(i, it)| (it.id.clone(), i)).collect();
    let page = page.max(1);
    let result = (|| -> Result<()> {
        match filter {
            None => {
                let mut after = match &h.cursor {
                    Some(Cursor::Connection { after }) => after.clone(),
                    _ => None,
                };
                loop {
                    let req = Request {
                        name: "history_issues",
                        query: issues_query(),
                        variables: json!({"owner": owner, "name": name, "since": h.since, "first": page, "after": after}),
                    };
                    let resp = gh.graphql(&req)?;
                    h.requests += 1;
                    check("history_issues", &resp)?;
                    let c = &resp["data"]["repository"]["issues"];
                    if resp["data"]["repository"].is_null() {
                        return Err(Error::GitHub(format!("{repo}: no such repository")));
                    }
                    for n in c["nodes"].as_array().into_iter().flatten().filter(|n| !n.is_null()) {
                        let kept = item(n)?;
                        // GitHub filters by `since` too; this keeps a recording valid for any window.
                        if let Ok(it) = &kept
                            && it.state == "closed"
                            && it.closed_at.as_deref().is_some_and(|c| c < h.since.as_str())
                        {
                            continue;
                        }
                        keep(&mut h, kept, &mut ids);
                    }
                    let more = c["pageInfo"]["hasNextPage"].as_bool() == Some(true);
                    after = c["pageInfo"]["endCursor"].as_str().map(str::to_string);
                    h.cursor = Some(Cursor::Connection { after: after.clone() });
                    if !more {
                        return Ok(());
                    }
                    save(root, &h)?;
                }
            }
            Some(f) => {
                let wanted = match &h.cursor {
                    Some(Cursor::Search { ids, read }) => (ids.clone(), *read),
                    _ => {
                        let refs =
                            crate::read::Reader::new(gh, page).search(&format!("repo:{repo} is:issue {f}"), repo)?;
                        h.requests += 1;
                        (refs.into_iter().map(|r| r.node_id).collect(), 0)
                    }
                };
                let (all, mut read) = wanted;
                h.cursor = Some(Cursor::Search { ids: all.clone(), read });
                save(root, &h)?;
                while read < all.len() {
                    let chunk: Vec<&String> = all[read..].iter().take(page as usize).collect();
                    let req = Request {
                        name: "history_nodes",
                        query: nodes_query(),
                        variables: json!({"ids": chunk}),
                    };
                    let resp = gh.graphql(&req)?;
                    h.requests += 1;
                    check("history_nodes", &resp)?;
                    for n in resp["data"]["nodes"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter(|n| !n.is_null())
                    {
                        let kept = item(n)?;
                        if let Ok(it) = &kept
                            && it.created_at < h.since
                            && it.closed_at.as_deref().is_none_or(|c| c < h.since.as_str())
                        {
                            continue;
                        }
                        keep(&mut h, kept, &mut ids);
                    }
                    read += chunk.len();
                    h.cursor = Some(Cursor::Search { ids: all.clone(), read });
                    save(root, &h)?;
                }
                Ok(())
            }
        }
    })();
    let error = match result {
        Ok(()) => {
            h.complete = true;
            h.cursor = None;
            None
        }
        Err(e) => Some(e.to_string()),
    };
    save(root, &h)?;
    let since = h.since.clone();
    Ok(RepoReport {
        repo: repo.to_string(),
        complete: h.complete,
        resumed,
        items: h.items.len(),
        closed_in_window: h
            .items
            .iter()
            .filter(|i| i.state == "closed" && i.closed_at.as_deref().is_some_and(|c| c >= since.as_str()))
            .count(),
        requests: h.requests,
        skipped: h.skipped.len(),
        seconds: started.elapsed().as_secs_f64(),
        error,
    })
}

/// `eval refresh-history` for every repo of `project`.
pub fn refresh(
    root: &Path,
    config: &ConfigSet,
    gh: &dyn GitHub,
    project: &str,
    now: DateTime<Utc>,
    page: u32,
) -> Result<Vec<RepoReport>> {
    let days = config.project(project)?.eval_history_days.unwrap_or(HISTORY_DAYS);
    let mut out = Vec::new();
    for (repo, view) in config.views(project)? {
        out.push(refresh_repo(root, gh, &repo, view.filter.as_deref(), days, now, page)?);
    }
    Ok(out)
}

/// Every complete history file of `project`'s repos; a repo whose history is missing or
/// incomplete is refused, so an evaluation never runs on part of the window.
pub fn project_history(root: &Path, config: &ConfigSet, project: &str) -> Result<Vec<RepoHistory>> {
    let mut out = Vec::new();
    for repo in config.views(project)?.keys() {
        match load(root, repo)? {
            Some(h) if h.complete => out.push(h),
            Some(_) => {
                return Err(Error::refused(format!(
                    "the history of {repo} is incomplete: run eval refresh-history again, it resumes"
                )));
            }
            None => {
                return Err(Error::refused(format!(
                    "no history of {repo}: run eval refresh-history first"
                )));
            }
        }
    }
    Ok(out)
}
