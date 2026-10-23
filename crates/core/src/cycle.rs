//! The cycle (10.4): ingest, observe, derive. Each step that records takes the root lock for
//! its own transaction; the GitHub reads run between them without it, and derive runs after
//! the last commit from a read transaction begun under the lock.
//!
//! 1. **ingest**: record the config, then the page documents (10.5).
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
use crate::snapshot::{self, RepoReport};

/// What one cycle did.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct CycleReport {
    pub project: String,
    pub ingest: ingest::Report,
    pub repos: Vec<RepoReport>,
    pub sequence: i64,
    pub outbox: PathBuf,
}

fn begin(
    root: &Root,
    clock: &dyn Fn() -> DateTime<Utc>,
    project: &str,
    pages: &Pages,
) -> Result<(ConfigSet, ingest::Report)> {
    root.mutate_with_clock(clock, |tx| {
        // Read under the lock (local files only), so a config recorded here is never older
        // than one another command recorded while this one waited.
        let config = ConfigSet::load(root.dir())?;
        config.record(tx)?;
        let owner = config.project(project)?.owner_id.clone();
        let report = ingest::ingest(tx, project, &owner, pages)?;
        Ok((config, report))
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

/// One full cycle of `project`.
pub fn cycle(
    root: &Root,
    gh: &dyn GitHub,
    project: &str,
    pages: &Pages,
    clock: &dyn Fn() -> DateTime<Utc>,
    page_size: u32,
) -> Result<CycleReport> {
    let (config, ingested) = begin(root, clock, project, pages)?;
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
                repos,
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
    let (config, _) = begin(root, clock, project, &Pages::default())?;
    let plan = observe_plan(root, &config, project, clock)?;
    let fetches = snapshot::fetch(gh, &plan, clock, page_size);
    root.mutate_with_clock(clock, |tx| snapshot::record(tx, project, &fetches))
}
