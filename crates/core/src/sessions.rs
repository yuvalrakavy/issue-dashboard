//! Session tracking (10.10): the app's session list, which the sync job reads with
//! `list_sessions` and hands to `cycle --sessions`, recorded as `session_seen` and
//! `session_gone`; and the matching of each live session to the items it works on, in derive.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::canonical::hash_value;
use crate::config::{ConfigSet, expand_home};
use crate::event::{Event, Kind};
use crate::ingest::Skipped;
use crate::launch;
use crate::ledger::{self, Dedupe, Tx};
use crate::observe::ItemKind;
use crate::root::parse_time;
use crate::schema;
use crate::{Error, Result};

/// The file `cycle --sessions` reads: the rows as `list_sessions` returned them.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionList {
    pub listed_at: String,
    /// The limit `list_sessions` was called with: a list of that many rows may be cut.
    pub limit: u32,
    pub sessions: Vec<Value>,
}

/// What one ingest of a session list recorded.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    pub seen: usize,
    pub gone: usize,
    pub skipped: Vec<Skipped>,
}

/// A row reduced to what the ledger keeps (5 in 10.10's list; never a transcript).
struct Row {
    id: String,
    archived: bool,
    last_active_at: String,
    data: Value,
}

fn row(v: &Value, listed_at: &str) -> Result<Row> {
    schema::validate("session-row", "session row", v)?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
    let last = s("lastActivityAt").unwrap_or_default();
    parse_time(&last)?;
    Ok(Row {
        id: s("sessionId").unwrap_or_default(),
        archived: v["isArchived"].as_bool().unwrap_or(false),
        last_active_at: last.clone(),
        data: json!({
            "title": s("title").unwrap_or_default(),
            "cwd": s("cwd").unwrap_or_default(),
            "branch": s("branch"),
            "pr_number": v.get("prNumber").and_then(Value::as_u64),
            "running": v["isRunning"].as_bool().unwrap_or(false),
            "last_active_at": last,
            "link": s("link"),
            "listed_at": listed_at,
        }),
    })
}

/// The hash a `session_seen` is told apart by: the content, with the last activity taken to
/// the hour so a running session records about once an hour, not once a sync.
fn content(data: &Value) -> String {
    let hour = data["last_active_at"]
        .as_str()
        .unwrap_or("")
        .get(..13)
        .unwrap_or("")
        .to_string();
    hash_value(&json!({
        "title": data["title"],
        "cwd": data["cwd"],
        "branch": data["branch"],
        "pr_number": data["pr_number"],
        "running": data["running"],
        "last_active_hour": hour,
    }))
}

/// The session's latest `session_seen` or `session_gone`, whichever is later.
fn latest(conn: &Connection, id: &str) -> Result<Option<Event>> {
    let seen = ledger::latest(conn, id, Kind::SessionSeen)?;
    let gone = ledger::latest(conn, id, Kind::SessionGone)?;
    Ok(match (seen, gone) {
        (Some(s), Some(g)) => Some(if s.seq > g.seq { s } else { g }),
        (s, g) => s.or(g),
    })
}

fn gone(tx: &mut Tx<'_>, seen: &Event, why: &str, listed_at: &str) -> Result<()> {
    tx.record_valid(
        Kind::SessionGone,
        &seen.subject,
        json!({"seen": seen.seq}),
        json!({"why": why, "listed_at": listed_at}),
        Dedupe::IdentityOnly,
    )?;
    Ok(())
}

/// Records the list (10.10): each unarchived session whose content is new or which was gone
/// is seen; a seen session that is archived, or missing from a list that would show it, is
/// gone. A repeat of the same list records nothing.
pub fn ingest(tx: &mut Tx<'_>, list: &SessionList) -> Result<Report> {
    schema::validate("session-list", "session list", &serde_json::to_value(list)?)?;
    let mut report = Report::default();
    let mut listed = BTreeSet::new();
    let mut oldest: Option<i64> = None;
    for v in &list.sessions {
        let id = v.get("sessionId").and_then(Value::as_str).map(str::to_string);
        let r = match row(v, &list.listed_at) {
            Ok(r) => r,
            Err(Error::Refused(reason)) => {
                report.skipped.push(Skipped {
                    collection: "sessions",
                    id,
                    reason,
                });
                continue;
            }
            Err(e) => return Err(e),
        };
        let at = parse_time(&r.last_active_at)?.timestamp();
        oldest = Some(oldest.map_or(at, |o| o.min(at)));
        if !listed.insert(r.id.clone()) {
            continue;
        }
        let last = latest(tx.conn(), &r.id)?;
        let was_seen = last.as_ref().filter(|e| e.kind == Kind::SessionSeen);
        if r.archived {
            if let Some(e) = was_seen {
                gone(tx, e, "archived", &list.listed_at)?;
                report.gone += 1;
            }
            continue;
        }
        let c = content(&r.data);
        if was_seen.is_some_and(|e| e.distinct["content"] == c) {
            continue;
        }
        tx.record_valid(
            Kind::SessionSeen,
            &r.id,
            json!({"content": c}),
            r.data,
            Dedupe::IdentityOnly,
        )?;
        report.seen += 1;
    }
    // Sorted by activity and cut at the limit: a missing session is gone only if the list is
    // complete, or it would have been listed (its activity, which only grows, is no older than
    // the oldest row listed).
    let complete = (list.sessions.len() as u64) < u64::from(list.limit);
    let mut subjects = BTreeSet::new();
    for e in ledger::events_by_kind(tx.conn(), Kind::SessionSeen)? {
        subjects.insert(e.subject);
    }
    for id in subjects.difference(&listed) {
        let Some(e) = latest(tx.conn(), id)?.filter(|e| e.kind == Kind::SessionSeen) else {
            continue;
        };
        let at = parse_time(e.data["last_active_at"].as_str().unwrap_or(""))?.timestamp();
        if complete || oldest.is_some_and(|o| at >= o) {
            gone(tx, &e, "absent", &list.listed_at)?;
            report.gone += 1;
        }
    }
    Ok(report)
}

/// A session whose latest event is a `session_seen`: listed, and not archived.
#[derive(Debug, Clone)]
pub struct Live {
    pub id: String,
    pub data: Value,
}

pub fn live(conn: &Connection) -> Result<Vec<Live>> {
    let mut ids = BTreeSet::new();
    for e in ledger::events_by_kind(conn, Kind::SessionSeen)? {
        ids.insert(e.subject);
    }
    let mut out = Vec::new();
    for id in ids {
        if let Some(e) = latest(conn, &id)?.filter(|e| e.kind == Kind::SessionSeen) {
            out.push(Live { id, data: e.data });
        }
    }
    Ok(out)
}

/// An item of the project, by its current alias.
pub struct Tracked {
    pub id: String,
    pub kind: ItemKind,
    /// For a pull request: the node ids of the issues it closes.
    pub closes: Vec<String>,
}

/// A reference in a title or branch: a number, with the repo it named if it named one.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct Ref {
    repo: Option<String>,
    number: u64,
}

fn word(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn digits_at(s: &str, i: usize) -> Option<(u64, usize)> {
    let d: String = s[i..].chars().take_while(char::is_ascii_digit).collect();
    if d.is_empty() {
        return None;
    }
    Some((d.parse().ok()?, i + d.len()))
}

/// `owner/repo#N`, `#N` (so `gh #N` too) and `issue N` / `issue #N` in a title.
fn title_refs(title: &str) -> Vec<Ref> {
    let mut out = Vec::new();
    let chars: Vec<(usize, char)> = title.char_indices().collect();
    for (k, &(i, c)) in chars.iter().enumerate() {
        if c != '#' {
            continue;
        }
        let Some((n, end)) = digits_at(title, i + 1) else {
            continue;
        };
        if title[end..].chars().next().is_some_and(word) {
            continue;
        }
        // The run of repo-name characters before the `#`.
        let start = chars[..k]
            .iter()
            .rev()
            .take_while(|(_, c)| word(*c) || matches!(c, '/' | '.' | '-'))
            .last()
            .map_or(i, |(j, _)| *j);
        let before = &title[start..i];
        if before.is_empty() {
            out.push(Ref { repo: None, number: n });
        } else if let Some((owner, repo)) = before.split_once('/')
            && !owner.is_empty()
            && !repo.is_empty()
            && !repo.contains('/')
        {
            out.push(Ref {
                repo: Some(before.to_string()),
                number: n,
            });
        }
        // Anything else (`abc#5`) is not a reference.
    }
    let lower = title.to_lowercase();
    let mut from = 0;
    while let Some(p) = lower[from..].find("issue") {
        let at = from + p;
        from = at + 5;
        if lower[..at].chars().next_back().is_some_and(word) {
            continue;
        }
        let rest = &lower[at + 5..];
        let gap = rest.len() - rest.trim_start().len();
        if gap == 0 {
            continue;
        }
        let i = at + 5 + gap;
        let i = if lower[i..].starts_with('#') { i + 1 } else { i };
        if let Some((n, end)) = digits_at(&lower, i)
            && !lower[end..].chars().next().is_some_and(word)
        {
            out.push(Ref { repo: None, number: n });
        }
    }
    out
}

/// `issue-N` or `gh-N` as a whole part of a branch name (parts split at `/`, `_`, `-`).
fn branch_refs(branch: &str) -> Vec<Ref> {
    let lower = branch.to_lowercase();
    let sep = |c: char| matches!(c, '/' | '_' | '-');
    let mut out = Vec::new();
    for prefix in ["issue-", "gh-"] {
        let mut from = 0;
        while let Some(p) = lower[from..].find(prefix) {
            let at = from + p;
            from = at + prefix.len();
            if lower[..at].chars().next_back().is_some_and(|c| !sep(c)) {
                continue;
            }
            if let Some((n, end)) = digits_at(&lower, at + prefix.len())
                && lower[end..].chars().next().is_none_or(sep)
            {
                out.push(Ref { repo: None, number: n });
            }
        }
    }
    out
}

/// The member repo whose checkout holds `cwd` (the longest such checkout).
fn repo_of(cwd: &str, checkouts: &[(String, PathBuf)]) -> Option<String> {
    let cwd = Path::new(cwd);
    checkouts
        .iter()
        .filter(|(_, path)| cwd.starts_with(path))
        .max_by_key(|(_, path)| path.components().count())
        .map(|(repo, _)| repo.clone())
}

/// The one item `refs` name, if they name exactly one the project tracks.
fn resolve<'a>(refs: &[Ref], repo: Option<&str>, tracked: &'a BTreeMap<String, Tracked>) -> Option<&'a Tracked> {
    let mut names = BTreeSet::new();
    for r in refs {
        let repo = r.repo.as_deref().or(repo)?;
        names.insert(format!("{repo}#{}", r.number));
    }
    if names.len() != 1 {
        return None;
    }
    tracked.get(names.first()?)
}

/// Each item's matched live sessions, as its document shows them (10.10), newest first.
pub fn matches(
    conn: &Connection,
    config: &ConfigSet,
    project: &str,
    tracked: &BTreeMap<String, Tracked>,
) -> Result<BTreeMap<String, Vec<Value>>> {
    let mut checkouts = Vec::new();
    for repo in config.views(project)?.into_keys() {
        if let Some(local) = config.registry.repo.get(&repo).and_then(|e| e.local_path.as_deref()) {
            checkouts.push((repo, expand_home(local)));
        }
    }
    let launches: Vec<launch::Launch> = launch::all(conn)?
        .into_iter()
        .filter(|l| l.request.project == project)
        .collect();
    let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for s in live(conn)? {
        let d = &s.data;
        let title = d["title"].as_str().unwrap_or("");
        let repo = repo_of(d["cwd"].as_str().unwrap_or(""), &checkouts);
        let known: BTreeSet<&str> = tracked.values().map(|t| t.id.as_str()).collect();
        let mut found: Vec<(String, &str, &str)> = Vec::new();
        for l in &launches {
            let by_session = l.session.as_deref() == Some(s.id.as_str());
            let by_token = l.token.as_deref().is_some_and(|t| title.contains(t));
            if by_session || by_token {
                for item in l.request.items.iter().filter(|i| known.contains(i.as_str())) {
                    found.push((item.clone(), "exact", "launch"));
                }
            }
        }
        if let (Some(repo), Some(n)) = (&repo, d["pr_number"].as_u64())
            && let Some(pr) = tracked
                .get(&format!("{repo}#{n}"))
                .filter(|t| t.kind == ItemKind::PullRequest)
        {
            found.push((pr.id.clone(), "exact", "pull_request"));
            for issue in pr.closes.iter().filter(|i| known.contains(i.as_str())) {
                found.push((issue.clone(), "exact", "pull_request"));
            }
        }
        if found.is_empty() {
            let from_title = title_refs(title);
            let from_branch = d["branch"].as_str().map(branch_refs).unwrap_or_default();
            let mut all = from_title.clone();
            all.extend(from_branch.iter().cloned());
            if let Some(t) = resolve(&all, repo.as_deref(), tracked) {
                let via = if from_title.is_empty() { "branch" } else { "title" };
                found.push((t.id.clone(), "probable", via));
            }
        }
        let mut once = BTreeSet::new();
        for (item, kind, via) in found {
            if once.insert(item.clone()) {
                out.entry(item).or_default().push(json!({
                    "id": s.id,
                    "title": title,
                    "running": d["running"],
                    "last_active_at": d["last_active_at"],
                    "link": d["link"],
                    "match": {"kind": kind, "via": via},
                }));
            }
        }
    }
    for v in out.values_mut() {
        v.sort_by(|a, b| {
            let t = |x: &Value| {
                parse_time(x["last_active_at"].as_str().unwrap_or(""))
                    .map(|t| t.timestamp_millis())
                    .unwrap_or(0)
            };
            t(b).cmp(&t(a)).then_with(|| a["id"].as_str().cmp(&b["id"].as_str()))
        });
    }
    Ok(out)
}
