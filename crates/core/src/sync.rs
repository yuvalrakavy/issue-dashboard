//! Sync requests (7.2, 10.8): asking for the next sync run now, globally or for one item.
//!
//! A request is a `request` event whose document has `kind: "sync"`, written by `sync-now` or
//! ingested from the page's `requests` collection. A global request names no item: the run it
//! wakes is an ordinary cycle. An item request names node ids, and the next read of the item's
//! repo reads each of them by node id, whatever the watermark says, as it reads an entering item.
//!
//! A request is served by the ledger's order, never by the clock. A read plan notes the last
//! event seq when it is made (`RepoPlan::requests_upto`) and reads the items of every request
//! after the seq its project's last good read of the repo served (the derived `served` table);
//! once the read is recorded good, that seq is served. So a failed read leaves its requests
//! waiting, a request recorded while a read runs waits for the next read, and any number of
//! requests before a run cost one read. Losing `served` (a rebuild) costs one read by id of
//! each item ever requested, never a missed request.

use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::{Value, json};

use crate::canonical::hash_value;
use crate::config::ConfigSet;
use crate::event::Kind;
use crate::ingest;
use crate::ledger::{self, Tx};
use crate::snapshot;
use crate::{Error, Result};

/// The author of a request `sync-now` writes: not a page user, so never the owner.
pub const AUTHOR: &str = "sync-now";

/// What `sync-now` recorded.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Requested {
    pub project: String,
    /// The request document's id.
    pub request: String,
    /// The node ids the next read reads by id; empty for a global request.
    pub items: Vec<String>,
    /// The same request was already recorded (the same request at the same instant).
    pub repeat: bool,
    /// The project's `sync_task`: the scheduled task the calling session runs now.
    pub task: Option<String>,
}

/// Records a sync request for `project`: global, or for `issue` (`owner/repo#N`), which must
/// be an item the project tracks.
pub fn request(tx: &mut Tx<'_>, config: &ConfigSet, project: &str, issue: Option<&str>) -> Result<Requested> {
    let task = config.project(project)?.sync_task.clone();
    let items = match issue {
        Some(alias) => vec![tracked(tx.conn(), config, project, alias)?],
        None => Vec::new(),
    };
    let created_at = tx.now().to_string();
    let id = format!(
        "sync-{}",
        &hash_value(&json!({"project": project, "items": items, "created_at": created_at}))[..20]
    );
    let mut doc = json!({
        "id": id,
        "project": project,
        "author": AUTHOR,
        "created_at": created_at,
        "kind": "sync",
    });
    if !items.is_empty() {
        doc["items"] = json!(items);
    }
    let repeat = !ingest::record_doc(tx, Kind::Request, project, &doc)?;
    Ok(Requested {
        project: project.to_string(),
        request: id,
        items,
        repeat,
        task,
    })
}

/// The node id of `alias` among the members of `project`'s sets. The repo part may be the
/// name the project's config uses or the repo's name at its latest read; the number is matched
/// within that repo, since an item's own alias keeps the name it was last read under.
fn tracked(conn: &Connection, config: &ConfigSet, project: &str, alias: &str) -> Result<String> {
    let shape = || Error::refused(format!("{alias}: an issue is named owner/repo#N"));
    let (repo, number) = alias.split_once('#').ok_or_else(shape)?;
    let well_formed = repo.split('/').count() == 2
        && repo.split('/').all(|p| !p.is_empty())
        && number.bytes().all(|b| b.is_ascii_digit());
    let number: u64 = match number.parse() {
        Ok(n) if well_formed => n,
        _ => return Err(shape()),
    };
    for view in config.views(project)?.into_values() {
        // After a rebuild `reads` is empty; the ledger still knows the repo (as the plan finds it).
        let node = match snapshot::read_state(conn, project, &view.repo)?.and_then(|s| s.repo_node) {
            Some(n) => Some(n),
            None => snapshot::repo_node_of(conn, &view.repo)?,
        };
        let Some(node) = node else { continue };
        let current = ledger::latest(conn, &node, Kind::Observed)?
            .and_then(|e| e.data.get("alias").and_then(Value::as_str).map(str::to_string));
        let names_it =
            repo.eq_ignore_ascii_case(&view.repo) || current.as_deref().is_some_and(|c| repo.eq_ignore_ascii_case(c));
        if !names_it {
            continue;
        }
        let Some((_, members)) = snapshot::membership(conn, project, &node)? else {
            continue;
        };
        for id in members.all() {
            let seen = ledger::latest(conn, &id, Kind::Observed)?;
            let n = seen
                .as_ref()
                .and_then(|e| e.data.get("alias"))
                .and_then(Value::as_str)
                .and_then(|a| a.rsplit_once('#'))
                .and_then(|(_, n)| n.parse::<u64>().ok());
            if n == Some(number) {
                return Ok(id);
            }
        }
    }
    Err(Error::refused(format!(
        "{alias} is not tracked by project {project}: only an item in one of its sets can be read on request"
    )))
}

/// The ledger seq up to which `project`'s good reads of the repo served its sync requests.
pub(crate) fn served(conn: &Connection, project: &str, repo_node: &str) -> Result<i64> {
    Ok(conn
        .query_row(
            "SELECT seq FROM served WHERE project = ?1 AND repo_node = ?2",
            params![project, repo_node],
            |r| r.get(0),
        )
        .optional()?
        .unwrap_or(0))
}

/// Records that a good read of the repo served `project`'s sync requests up to `upto`.
pub(crate) fn mark_served(conn: &Connection, project: &str, repo_node: &str, upto: i64) -> Result<()> {
    conn.execute(
        "INSERT INTO served (project, repo_node, seq) VALUES (?1, ?2, ?3)
         ON CONFLICT (project, repo_node) DO UPDATE SET seq = MAX(seq, excluded.seq)",
        params![project, repo_node, upto],
    )?;
    Ok(())
}

/// The items named by `project`'s sync requests recorded after seq `after`, up to `upto`.
pub(crate) fn waiting(conn: &Connection, project: &str, after: i64, upto: i64) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for e in ledger::events_of_kind_between(conn, Kind::Request, after, upto)? {
        let doc = &e.data["document"];
        if e.data["project"] != project || doc["kind"] != "sync" {
            continue;
        }
        out.extend(
            doc["items"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|i| i.as_str().map(str::to_string)),
        );
    }
    Ok(out)
}
