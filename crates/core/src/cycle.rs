//! The cycle (10.4): ingest, observe, derive. Each step that records takes the root lock for
//! its own transaction; the GitHub reads run between them without it, and derive runs after
//! the last commit from a read transaction begun under the lock.
//!
//! 1. **ingest**: record the config, the page documents (10.5) and the app's session list
//!    (10.10).
//!    - **write** (1a): apply the owner's closes and their undos to GitHub (10.10), holding
//!      the lock across each one's intent, call and result.
//! 2. **observe**: plan from the ledger, read GitHub without the lock, record the reads under
//!    it (chapter 2); then take the cycle's outbox sequence number in the same transaction.
//! 3. **derive**: build the project's documents from the ledger as of that commit and write
//!    `<root>/outbox/<project>/<seq>.json`.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::Serialize;
use serde_json::json;

use crate::Result;
use crate::config::ConfigSet;
use crate::derive;
use crate::event::Kind;
use crate::github::GitHub;
use crate::ingest::{self, Pages};
use crate::ledger::{Dedupe, Tx};
use crate::root::{Root, parse_time};
use crate::sessions::{self, SessionList};
use crate::snapshot::{self, RepoReport};
use crate::write::{self, IssueWriter, Written};

/// What one cycle did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CycleReport {
    pub project: String,
    pub ingest: ingest::Report,
    /// What the session list recorded; empty when the cycle had none.
    pub sessions: sessions::Report,
    pub repos: Vec<RepoReport>,
    /// The GitHub writes the cycle made or settled.
    pub writes: Vec<Written>,
    pub sequence: i64,
    pub outbox: PathBuf,
}

fn begin(
    root: &Root,
    clock: &dyn Fn() -> DateTime<Utc>,
    project: &str,
    pages: &Pages,
    list: Option<&SessionList>,
) -> Result<(ConfigSet, ingest::Report, sessions::Report)> {
    root.mutate_with_clock(clock, |tx| {
        // Read under the lock (local files only), so a config recorded here is never older
        // than one another command recorded while this one waited.
        let config = ConfigSet::load(root.dir())?;
        config.record(tx)?;
        let owner = config.project(project)?.owner_id.clone();
        let report = ingest::ingest(tx, project, &owner, pages)?;
        let seen = match list {
            Some(l) => sessions::ingest(tx, l)?,
            None => sessions::Report::default(),
        };
        Ok((config, report, seen))
    })
}

fn observe_plan(
    root: &Root,
    config: &ConfigSet,
    project: &str,
    clock: &dyn Fn() -> DateTime<Utc>,
) -> Result<snapshot::Plan> {
    let plan = root.read(|l| snapshot::plan(l.conn(), config, project, clock()))?;
    snapshot::require_members(&plan)?;
    Ok(plan)
}

/// Takes the cycle's outbox sequence number: the seq of an `outbox` event, so it only grows,
/// and survives a rebuild.
pub fn take_sequence(tx: &mut Tx<'_>, project: &str) -> Result<(i64, DateTime<Utc>)> {
    let taken_at = tx.now().to_string();
    let e = tx
        .record_valid(
            Kind::Outbox,
            project,
            json!({"taken_at": taken_at}),
            json!({"project": project, "taken_at": taken_at}),
            Dedupe::IdentityOnly,
        )?
        .event()
        .clone();
    Ok((e.seq, parse_time(&taken_at)?))
}

/// One full cycle of `project`, without the write step: a close stays pending.
pub fn cycle(
    root: &Root,
    gh: &dyn GitHub,
    project: &str,
    pages: &Pages,
    clock: &dyn Fn() -> DateTime<Utc>,
    page_size: u32,
) -> Result<CycleReport> {
    cycle_with_writes(root, gh, None, project, pages, clock, page_size)
}

/// One full cycle of `project`; with a `writer`, the write step applies the owner's closes.
pub fn cycle_with_writes(
    root: &Root,
    gh: &dyn GitHub,
    writer: Option<&dyn IssueWriter>,
    project: &str,
    pages: &Pages,
    clock: &dyn Fn() -> DateTime<Utc>,
    page_size: u32,
) -> Result<CycleReport> {
    cycle_full(root, gh, writer, project, pages, None, clock, page_size)
}

/// One full cycle of `project`: [`cycle_with_writes`], also recording the app's session list
/// when there is one (10.10).
#[allow(clippy::too_many_arguments)]
pub fn cycle_full(
    root: &Root,
    gh: &dyn GitHub,
    writer: Option<&dyn IssueWriter>,
    project: &str,
    pages: &Pages,
    list: Option<&SessionList>,
    clock: &dyn Fn() -> DateTime<Utc>,
    page_size: u32,
) -> Result<CycleReport> {
    let (config, ingested, seen) = begin(root, clock, project, pages, list)?;
    let writes = match writer {
        Some(w) => root.session(clock, |s| write::step(s, w, &config, project))?,
        None => Vec::new(),
    };
    let plan = observe_plan(root, &config, project, clock)?;
    let fetches = snapshot::fetch(gh, &plan, clock, page_size);
    root.mutate_then_read(
        clock,
        |tx| {
            let repos = snapshot::record(tx, project, &fetches)?;
            let (seq, taken_at) = take_sequence(tx, project)?;
            Ok((repos, seq, taken_at))
        },
        |ledger, (repos, seq, taken_at)| {
            let out = derive::derive(ledger.conn(), &config, project, seq, taken_at)?;
            let path = derive::write(root.dir(), &out)?;
            Ok(CycleReport {
                project: project.to_string(),
                ingest: ingested,
                sessions: seen,
                repos,
                writes,
                sequence: seq,
                outbox: path,
            })
        },
    )
}

/// The observe step alone (`issue-dashboard snapshot`): records the config and the reads, and
/// writes no outbox.
pub fn snapshot(
    root: &Root,
    gh: &dyn GitHub,
    project: &str,
    clock: &dyn Fn() -> DateTime<Utc>,
    page_size: u32,
) -> Result<Vec<RepoReport>> {
    let (config, _, _) = begin(root, clock, project, &Pages::default(), None)?;
    let plan = observe_plan(root, &config, project, clock)?;
    let fetches = snapshot::fetch(gh, &plan, clock, page_size);
    root.mutate_with_clock(clock, |tx| snapshot::record(tx, project, &fetches))
}
