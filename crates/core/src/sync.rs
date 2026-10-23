//! Sync requests (7.2, 10.8): asking for the next sync run now, globally or for one item.
//!
//! A request is a `request` event whose document has `kind: "sync"`, written by `sync-now` or
//! ingested from the page's `requests` collection. A global request names no item: the run it
//! wakes is an ordinary cycle. An item request names node ids, and the next read of the item's
//! repo reads each of them by node id, whatever the watermark says, as it reads an entering item.
//!
//! Nothing marks a request served. It is waiting while the project's last good read of the repo
//! started before it was recorded, so a failed read leaves it waiting, a read that started
//! before it does not serve it, and any number of requests before a run cost one read.

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{Value, json};

use crate::canonical::hash_value;
use crate::config::ConfigSet;
use crate::event::Kind;
use crate::ingest;
use crate::ledger::{self, Tx};
use crate::root::parse_time;
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

/// The node id of `alias` among the members of `project`'s set, as the ledger last saw them.
fn tracked(conn: &Connection, config: &ConfigSet, project: &str, alias: &str) -> Result<String> {
    let shape = || Error::refused(format!("{alias}: an issue is named owner/repo#N"));
    let (repo, number) = alias.split_once('#').ok_or_else(shape)?;
    let well_formed = repo.split('/').count() == 2
        && repo.split('/').all(|p| !p.is_empty())
        && !number.is_empty()
        && number.bytes().all(|b| b.is_ascii_digit());
    if !well_formed {
        return Err(shape());
    }
    for view in config.views(project)?.into_values() {
        let node = match snapshot::read_state(conn, project, &view.repo)?.and_then(|s| s.repo_node) {
            Some(n) => n,
            None => continue,
        };
        let Some((_, members)) = snapshot::membership(conn, project, &node)? else {
            continue;
        };
        for id in members.all() {
            // The alias carries the repo's current name, so a renamed repo answers to it.
            let seen = ledger::latest(conn, &id, Kind::Observed)?;
            if seen
                .as_ref()
                .and_then(|e| e.data.get("alias"))
                .and_then(Value::as_str)
                .is_some_and(|a| a.eq_ignore_ascii_case(alias))
            {
                return Ok(id);
            }
        }
    }
    Err(Error::refused(format!(
        "{alias} is not tracked by project {project}: only an item in one of its sets can be read on request"
    )))
}

/// Every item request of `project`: when it was recorded, and the node ids it names.
pub(crate) fn item_requests(conn: &Connection, project: &str) -> Result<Vec<(DateTime<Utc>, Vec<String>)>> {
    let mut out = Vec::new();
    for e in ledger::events_by_kind(conn, Kind::Request)? {
        let doc = &e.data["document"];
        if e.data["project"] != project || doc["kind"] != "sync" {
            continue;
        }
        let items: Vec<String> = doc["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|i| i.as_str().map(str::to_string))
            .collect();
        if !items.is_empty() {
            out.push((parse_time(&e.time)?, items));
        }
    }
    Ok(out)
}
