//! The observe step of the cycle (chapter 2, 10.4 step 2): plan from the ledger, fetch from
//! GitHub without the lock, record under it.
//!
//! Each cycle does two reads per member repo (2.2):
//!
//! 1. **membership**: the member's filtered searches, ids only, plus each `also` number. Its
//!    result is the project's set; an item that stopped matching leaves it on this cycle.
//! 2. **details**: every item of the repo updated since the repo's watermark (less five
//!    minutes), without the filter, kept when it is in this project's set or another project's;
//!    plus, read by node id, every item entering the set, and on a full read the whole set.
//!
//! The watermark is per repo, shared by every project, and advances only when the whole read
//! of the repo succeeded. A failed read records its error and changes nothing else, so the
//! repo keeps its last good items (2.5). A full read happens on a project's first read of a
//! repo, after its view or translation changes, and once a day; it also confirms which items
//! that left the set are gone (`item_gone`).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::canonical::hash_value;
use crate::config::{ConfigSet, View};
use crate::event::Kind;
use crate::github::GitHub;
use crate::ledger::{self, Dedupe, Tx};
use crate::observe::{self, ItemKind, Observation, Observed};
use crate::read::{Item, Reader, RepoInfo};
use crate::root::parse_time;
use crate::translate::Translation;
use crate::{Error, Result};

/// The incremental read overlaps the previous one by this much (2.2).
pub const OVERLAP: Duration = Duration::minutes(5);

/// A full read at least this often (2.2).
pub const FULL_EVERY: Duration = Duration::hours(24);

/// `history_days` when the project config does not set it (2.1).
pub const DEFAULT_HISTORY_DAYS: u32 = 180;

/// The `data` of a `membership` event (`schemas/event-membership.schema.json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipData {
    pub project: String,
    pub repo: String,
    pub read_id: String,
    pub members_hash: String,
    pub issues: Vec<String>,
    pub pull_requests: Vec<String>,
}

impl MembershipData {
    pub fn all(&self) -> BTreeSet<String> {
        self.issues.iter().chain(&self.pull_requests).cloned().collect()
    }
}

/// `project`'s latest recorded set in the repo with node id `repo_node`, with its sequence number.
pub fn membership(conn: &Connection, project: &str, repo_node: &str) -> Result<Option<(i64, MembershipData)>> {
    for e in ledger::events_of(conn, repo_node, Kind::Membership)?.into_iter().rev() {
        let d: MembershipData = serde_json::from_value(e.data)?;
        if d.project == project {
            return Ok(Some((e.seq, d)));
        }
    }
    Ok(None)
}

/// The node id the ledger knows for a repo name: from any project's last read of it, else from
/// the repo's latest observation under that name.
pub fn repo_node_of(conn: &Connection, repo: &str) -> Result<Option<String>> {
    let from_reads: Option<String> = conn
        .query_row(
            "SELECT repo_node FROM reads WHERE repo = ?1 COLLATE NOCASE AND repo_node IS NOT NULL
             ORDER BY good_at DESC LIMIT 1",
            [repo],
            |r| r.get(0),
        )
        .optional()?;
    if from_reads.is_some() {
        return Ok(from_reads);
    }
    Ok(conn
        .query_row(
            "SELECT subject FROM events WHERE kind = 'observed'
               AND json_extract(data, '$.item') = 'repo'
               AND json_extract(data, '$.alias') = ?1 COLLATE NOCASE
             ORDER BY seq DESC LIMIT 1",
            [repo],
            |r| r.get(0),
        )
        .optional()?)
}

/// One project's last read of one configured repo (the `reads` table).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ReadState {
    pub repo_node: Option<String>,
    pub view_hash: Option<String>,
    pub mode: Option<String>,
    pub attempt_at: Option<String>,
    pub good_at: Option<String>,
    pub full_at: Option<String>,
    pub error: Option<String>,
}

pub fn read_state(conn: &Connection, project: &str, repo: &str) -> Result<Option<ReadState>> {
    Ok(conn
        .query_row(
            "SELECT repo_node, view_hash, mode, attempt_at, good_at, full_at, error FROM reads
             WHERE project = ?1 AND repo = ?2",
            params![project, repo],
            |r| {
                Ok(ReadState {
                    repo_node: r.get(0)?,
                    view_hash: r.get(1)?,
                    mode: r.get(2)?,
                    attempt_at: r.get(3)?,
                    good_at: r.get(4)?,
                    full_at: r.get(5)?,
                    error: r.get(6)?,
                })
            },
        )
        .optional()?)
}

/// The start of the last complete read of the repo with this node id.
pub fn watermark(conn: &Connection, repo_node: &str) -> Result<Option<DateTime<Utc>>> {
    let w: Option<String> = conn
        .query_row("SELECT started_at FROM watermarks WHERE repo = ?1", [repo_node], |r| {
            r.get(0)
        })
        .optional()?;
    w.map(|w| parse_time(&w)).transpose()
}

/// What a project's view of a repo reads: a change of any of it forces a full read (2.2).
pub fn view_hash(config: &ConfigSet, view: &View) -> String {
    hash_value(&json!({
        "filter": view.filter,
        "also": view.also,
        "translation": Translation::for_repo(config, &view.repo),
    }))
}

/// What the fetch of one repo needs from the ledger.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoPlan {
    pub view: View,
    pub view_hash: String,
    /// The repo's node id as the ledger knows it, if it does.
    pub repo_node: Option<String>,
    /// Why this read is full; `None` for an incremental read.
    pub full_reason: Option<String>,
    pub watermark: Option<DateTime<Utc>>,
    /// This project's latest recorded set in the repo.
    pub previous: BTreeSet<String>,
    /// Every other project's latest recorded set in the repo: their items' details are kept
    /// too, since the watermark this read advances is theirs as well.
    pub others: BTreeSet<String>,
    /// Every item in this project's set at any time since its last full read of the repo: a
    /// full read checks each one that is no longer in the set, including those that left at an
    /// incremental read since.
    pub since_full: BTreeSet<String>,
    /// The latest alias of each item in `since_full`, for `item_gone`.
    pub aliases: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Plan {
    pub project: String,
    pub history_days: u32,
    pub repos: Vec<RepoPlan>,
}

/// Plans `project`'s reads from the ledger (a read; no lock is needed).
pub fn plan(conn: &Connection, config: &ConfigSet, project: &str, now: DateTime<Utc>) -> Result<Plan> {
    let p = config.project(project)?;
    let mut repos = Vec::new();
    for view in config.views(project)?.into_values() {
        let vh = view_hash(config, &view);
        let state = read_state(conn, project, &view.repo)?;
        let repo_node = match state.as_ref().and_then(|s| s.repo_node.clone()) {
            Some(n) => Some(n),
            None => repo_node_of(conn, &view.repo)?,
        };
        let watermark = match &repo_node {
            Some(n) => watermark(conn, n)?,
            None => None,
        };
        let full_at = state
            .as_ref()
            .and_then(|s| s.full_at.as_deref())
            .map(parse_time)
            .transpose()?;
        let full_reason = match (&state, &repo_node, watermark, full_at) {
            (None, _, _, _) | (Some(ReadState { good_at: None, .. }), _, _, _) => Some("first read"),
            (_, None, _, _) | (_, _, None, _) => Some("no watermark"),
            (Some(s), _, _, _) if s.view_hash.as_deref() != Some(vh.as_str()) => Some("view or translation changed"),
            (_, _, _, None) => Some("no full read recorded"),
            (_, _, _, Some(f)) if now - f >= FULL_EVERY => Some("daily"),
            _ => None,
        }
        .map(str::to_string);
        let mut previous = BTreeSet::new();
        let mut since_full = BTreeSet::new();
        let mut others = BTreeSet::new();
        let mut aliases = BTreeMap::new();
        if let Some(node) = &repo_node {
            if let Some((_, m)) = membership(conn, project, node)? {
                previous = m.all();
            }
            since_full = sets_since(conn, project, node, full_at)?;
            for other in config.projects.keys().filter(|o| o.as_str() != project) {
                let tracks = config
                    .views(other)
                    .map(|v| v.keys().any(|r| r.eq_ignore_ascii_case(&view.repo)))
                    .unwrap_or(false);
                if tracks && let Some((_, m)) = membership(conn, other, node)? {
                    others.extend(m.all());
                }
            }
            for id in &since_full {
                if let Some(e) = ledger::latest(conn, id, Kind::Observed)?
                    && let Some(a) = e.data.get("alias").and_then(|a| a.as_str())
                {
                    aliases.insert(id.clone(), a.to_string());
                }
            }
        }
        repos.push(RepoPlan {
            view,
            view_hash: vh,
            repo_node,
            full_reason,
            watermark,
            previous,
            others,
            since_full,
            aliases,
        });
    }
    Ok(Plan {
        project: project.to_string(),
        history_days: p.history_days.unwrap_or(DEFAULT_HISTORY_DAYS),
        repos,
    })
}

/// The union of `project`'s sets in the repo from the one current at `full_at` on; every set
/// it ever had when there was no full read.
fn sets_since(
    conn: &Connection,
    project: &str,
    repo_node: &str,
    full_at: Option<DateTime<Utc>>,
) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for e in ledger::events_of(conn, repo_node, Kind::Membership)?.into_iter().rev() {
        let d: MembershipData = serde_json::from_value(e.data)?;
        if d.project != project {
            continue;
        }
        out.extend(d.all());
        if full_at.is_some_and(|f| parse_time(&e.time).is_ok_and(|t| t <= f)) {
            break;
        }
    }
    Ok(out)
}

/// What a complete read of one repo returned.
#[derive(Debug, Clone, PartialEq)]
pub struct Fetched {
    pub info: RepoInfo,
    pub full: bool,
    pub issues: BTreeSet<String>,
    pub pull_requests: BTreeSet<String>,
    pub items: Vec<Item>,
    /// Items that left the set and that GitHub no longer resolves, with their last alias.
    pub gone: Vec<(String, String)>,
    pub entered: usize,
    pub left: usize,
    /// `also` numbers that are neither an issue nor a pull request (2.2).
    pub skipped_also: Vec<u64>,
}

/// One repo's fetch: complete, or failed with the reason.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoFetch {
    pub plan: RepoPlan,
    pub started_at: DateTime<Utc>,
    pub read_id: String,
    pub outcome: std::result::Result<Fetched, String>,
}

/// Reads every planned repo from GitHub. No lock is held; a failure fails only its repo.
pub fn fetch(gh: &dyn GitHub, plan: &Plan, clock: &dyn Fn() -> DateTime<Utc>, page: u32) -> Vec<RepoFetch> {
    plan.repos
        .iter()
        .map(|rp| {
            let started_at = clock();
            let read_id = format!(
                "read-{}-{}-{}",
                plan.project,
                rp.view.repo.replace('/', "-"),
                started_at.format("%Y%m%dT%H%M%S%.3fZ")
            );
            let outcome =
                fetch_repo(&Reader::new(gh, page), rp, plan.history_days, started_at).map_err(|e| e.to_string());
            RepoFetch {
                plan: rp.clone(),
                started_at,
                read_id,
                outcome,
            }
        })
        .collect()
}

fn fetch_repo(r: &Reader<'_>, rp: &RepoPlan, history_days: u32, started_at: DateTime<Utc>) -> Result<Fetched> {
    let view = &rp.view;
    let info = r.repo(&view.repo)?;
    // A name that now resolves to another repo (deleted and recreated) shares nothing with
    // what the ledger holds under it.
    let same = rp.repo_node.as_deref() == Some(info.node_id.as_str());
    let empty = BTreeSet::new();
    let (previous, others, since_full, watermark) = if same {
        (&rp.previous, &rp.others, &rp.since_full, rp.watermark)
    } else {
        (&empty, &empty, &empty, None)
    };
    // The repo's current name: GitHub's search does not follow a rename.
    let name = info.name_with_owner.as_str();
    let full = rp.full_reason.is_some() || !same;

    // Read 1: membership, ids only.
    let window = started_at - Duration::days(history_days as i64);
    let filter = view.filter.as_deref().map(|f| format!(" {f}")).unwrap_or_default();
    let mut issues = BTreeSet::new();
    let mut prs = BTreeSet::new();
    for (qualifier, set) in [("is:issue", &mut issues), ("is:pr", &mut prs)] {
        let open = r.search(&format!("repo:{name} {qualifier} is:open{filter}"), name)?;
        let closed = r.search(
            &format!(
                "repo:{name} {qualifier} is:closed closed:>={}{filter}",
                window.format("%Y-%m-%d")
            ),
            name,
        )?;
        for x in open
            .into_iter()
            .chain(closed.into_iter().filter(|x| x.closed_at.is_none_or(|c| c >= window)))
        {
            set.insert(x.node_id);
        }
    }
    let mut skipped_also = Vec::new();
    for n in &view.also {
        match r.by_number(name, *n)? {
            Some(x) if x.kind == ItemKind::PullRequest => {
                prs.insert(x.node_id);
            }
            Some(x) => {
                issues.insert(x.node_id);
            }
            None => skipped_also.push(*n),
        }
    }
    let set: BTreeSet<String> = issues.iter().chain(&prs).cloned().collect();
    let entering: BTreeSet<String> = if full {
        set.clone()
    } else {
        set.difference(previous).cloned().collect()
    };
    let leaving: BTreeSet<String> = previous.difference(&set).cloned().collect();
    // A full read checks everything that left since the last full one.
    let unchecked: BTreeSet<String> = if full {
        since_full
            .union(previous)
            .filter(|id| !set.contains(*id))
            .cloned()
            .collect()
    } else {
        BTreeSet::new()
    };

    // Read 2: details. Every item updated since the watermark, unfiltered, kept when some
    // project tracks it; then by node id whatever is still needed.
    let mut items: BTreeMap<String, Item> = BTreeMap::new();
    if let Some(w) = watermark {
        let since = w - OVERLAP;
        for it in r
            .issues_since(name, since)?
            .into_iter()
            .chain(r.prs_since(name, since)?)
        {
            if set.contains(&it.node_id) || others.contains(&it.node_id) {
                items.insert(it.node_id.clone(), it);
            }
        }
    }
    let mut need: Vec<String> = entering.iter().cloned().collect();
    need.extend(unchecked.iter().cloned());
    need.retain(|id| !items.contains_key(id));
    let mut gone = Vec::new();
    for (id, found) in r.nodes(&need)? {
        match found {
            Some(it) => {
                items.insert(id, it);
            }
            None if unchecked.contains(&id) => {
                let alias = rp.aliases.get(&id).cloned().unwrap_or_else(|| format!("{name}#0"));
                gone.push((id, alias));
            }
            None => {}
        }
    }
    Ok(Fetched {
        info,
        full,
        issues,
        pull_requests: prs,
        items: items.into_values().collect(),
        gone,
        entered: entering.len(),
        left: leaving.len(),
        skipped_also,
    })
}

/// What recording one repo's fetch did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct RepoReport {
    pub repo: String,
    pub mode: Option<String>,
    pub full_reason: Option<String>,
    pub error: Option<String>,
    pub recorded: usize,
    pub unchanged: usize,
    pub stale: usize,
    pub entered: usize,
    pub left: usize,
    pub gone: usize,
    pub skipped_also: Vec<u64>,
    /// A newer read of this repo for this project was recorded first; only its observations
    /// were offered (each checked for staleness), not its set or its read times.
    pub superseded: bool,
}

/// Records the fetches under the lock (10.4 step 2).
pub fn record(tx: &mut Tx<'_>, project: &str, fetches: &[RepoFetch]) -> Result<Vec<RepoReport>> {
    fetches.iter().map(|f| record_repo(tx, project, f)).collect()
}

fn precise(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true)
}

fn record_repo(tx: &mut Tx<'_>, project: &str, f: &RepoFetch) -> Result<RepoReport> {
    let repo = &f.plan.view.repo;
    let started = precise(f.started_at);
    let mut report = RepoReport {
        repo: repo.clone(),
        mode: None,
        full_reason: f.plan.full_reason.clone(),
        error: None,
        recorded: 0,
        unchanged: 0,
        stale: 0,
        entered: 0,
        left: 0,
        gone: 0,
        skipped_also: Vec::new(),
        superseded: false,
    };
    let prior = read_state(tx.conn(), project, repo)?;
    let fetched = match &f.outcome {
        Err(e) => {
            // A failed read changes nothing but the error: the repo keeps its last good items.
            tx.conn().execute(
                "INSERT INTO reads (project, repo, attempt_at, error) VALUES (?1, ?2, ?3, ?4)
                 ON CONFLICT (project, repo) DO UPDATE SET attempt_at = excluded.attempt_at, error = excluded.error",
                params![project, repo, started, e],
            )?;
            report.error = Some(e.clone());
            return Ok(report);
        }
        Ok(x) => x,
    };
    let superseded = match prior.as_ref().and_then(|p| p.good_at.as_deref()) {
        Some(g) => parse_time(g)? > f.started_at,
        None => false,
    };
    report.mode = Some(if fetched.full { "full" } else { "incremental" }.into());
    report.superseded = superseded;
    report.skipped_also = fetched.skipped_also.clone();
    report.entered = fetched.entered;
    report.left = fetched.left;

    let info = &fetched.info;
    let mut offer = |tx: &mut Tx<'_>, o: Observation| -> Result<()> {
        match observe::record(tx, &o)? {
            Observed::Recorded(_) => report.recorded += 1,
            Observed::Unchanged(_) => report.unchanged += 1,
            Observed::Stale { .. } => report.stale += 1,
        }
        Ok(())
    };
    offer(
        tx,
        Observation {
            node_id: info.node_id.clone(),
            item: ItemKind::Repo,
            alias: info.name_with_owner.clone(),
            updated_at: f.started_at,
            read_at: f.started_at,
            read_id: f.read_id.clone(),
            content: info.content(),
        },
    )?;
    for it in &fetched.items {
        offer(
            tx,
            Observation {
                node_id: it.node_id.clone(),
                item: it.kind,
                alias: it.alias.clone(),
                updated_at: it.updated_at,
                read_at: f.started_at,
                read_id: f.read_id.clone(),
                content: it.content.clone(),
            },
        )?;
    }
    if superseded {
        return Ok(report);
    }

    let issues: Vec<String> = fetched.issues.iter().cloned().collect();
    let prs: Vec<String> = fetched.pull_requests.iter().cloned().collect();
    let members_hash = hash_value(&json!({"issues": issues, "pull_requests": prs}));
    let same = membership(tx.conn(), project, &info.node_id)?
        .is_some_and(|(_, m)| m.members_hash == members_hash && m.repo == info.name_with_owner);
    if !same {
        let data = MembershipData {
            project: project.to_string(),
            repo: info.name_with_owner.clone(),
            read_id: f.read_id.clone(),
            members_hash: members_hash.clone(),
            issues,
            pull_requests: prs,
        };
        tx.record_valid(
            Kind::Membership,
            &info.node_id,
            json!({"project": project, "repo": info.name_with_owner, "members_hash": members_hash}),
            serde_json::to_value(data)?,
            Dedupe::IdentityOnly,
        )?;
    }
    for (id, alias) in &fetched.gone {
        if observe::item_gone(tx, id, alias, &f.read_id, f.started_at)?.is_new() {
            report.gone += 1;
        }
    }
    let current = watermark(tx.conn(), &info.node_id)?;
    if current.is_none_or(|w| w < f.started_at) {
        tx.conn().execute(
            "INSERT INTO watermarks (repo, started_at) VALUES (?1, ?2)
             ON CONFLICT (repo) DO UPDATE SET started_at = excluded.started_at",
            params![info.node_id, started],
        )?;
    }
    let full_at = if fetched.full {
        Some(started.clone())
    } else {
        prior.as_ref().and_then(|p| p.full_at.clone())
    };
    tx.conn().execute(
        "INSERT INTO reads (project, repo, repo_node, view_hash, mode, attempt_at, good_at, full_at, error)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?6, ?7, NULL)
         ON CONFLICT (project, repo) DO UPDATE SET repo_node = excluded.repo_node,
           view_hash = excluded.view_hash, mode = excluded.mode, attempt_at = excluded.attempt_at,
           good_at = excluded.good_at, full_at = excluded.full_at, error = NULL",
        params![
            project,
            repo,
            info.node_id,
            f.plan.view_hash,
            report.mode,
            started,
            full_at
        ],
    )?;
    Ok(report)
}

/// Refuses a project that tracks no repo, so a typo in `--project` is not a silent no-op.
pub fn require_members(plan: &Plan) -> Result<()> {
    if plan.repos.is_empty() {
        return Err(Error::refused(format!("project {} tracks no repo", plan.project)));
    }
    Ok(())
}
