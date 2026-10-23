//! Recording what a read of GitHub returned (10.3 `observed` and `item_gone`; 10.4 step 2).
//!
//! An observation is recorded only when its content differs from the item's latest one, and
//! a stale fetch never overwrites a fresher one: an observation older than the item's latest
//! recorded one (by GitHub's `updatedAt`, then by read time) is discarded at commit.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::Result;
use crate::canonical::hash_value;
use crate::event::Kind;
use crate::ledger::{self, Dedupe, Recorded, Tx};
use crate::root::{format_time, parse_time};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Issue,
    PullRequest,
    Repo,
}

/// One item as a read returned it.
#[derive(Debug, Clone, PartialEq)]
pub struct Observation {
    pub node_id: String,
    pub item: ItemKind,
    /// owner/repo#N, or owner/name for a repo.
    pub alias: String,
    pub updated_at: DateTime<Utc>,
    pub read_at: DateTime<Utc>,
    pub read_id: String,
    /// What was observed: labels, state, assignees, links, timeline ids (10.3).
    pub content: Value,
}

/// The `data` of an `observed` event. Its schema is `schemas/event-observed.schema.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ObservedData {
    pub item: ItemKind,
    pub alias: String,
    pub updated_at: String,
    pub read_at: String,
    pub read_id: String,
    pub content_hash: String,
    pub content: Value,
}

/// What [`record`] did.
#[derive(Debug, Clone, PartialEq)]
pub enum Observed {
    Recorded(i64),
    /// The content equals the item's latest observation.
    Unchanged(i64),
    /// Older than the item's latest observation; not recorded.
    Stale {
        latest_seq: i64,
    },
}

/// Records an observation under the rules above.
pub fn record(tx: &mut Tx<'_>, obs: &Observation) -> Result<Observed> {
    let content_hash = hash_value(&obs.content);
    if let Some(latest) = ledger::latest(tx.conn(), &obs.node_id, Kind::Observed)? {
        let prev: ObservedData = serde_json::from_value(latest.data.clone())?;
        let prev_at = (parse_time(&prev.updated_at)?, parse_time(&prev.read_at)?);
        if (obs.updated_at, obs.read_at) < prev_at {
            return Ok(Observed::Stale { latest_seq: latest.seq });
        }
        if prev.content_hash == content_hash {
            return Ok(Observed::Unchanged(latest.seq));
        }
    }
    let data = ObservedData {
        item: obs.item,
        alias: obs.alias.clone(),
        updated_at: format_time(obs.updated_at),
        read_at: format_time(obs.read_at),
        read_id: obs.read_id.clone(),
        content_hash: content_hash.clone(),
        content: obs.content.clone(),
    };
    let r = tx.record(
        Kind::Observed,
        &obs.node_id,
        json!({"content_hash": content_hash}),
        serde_json::to_value(data)?,
        Dedupe::Latest,
    )?;
    Ok(match r {
        Recorded::New(e) => Observed::Recorded(e.seq),
        Recorded::Repeat(e) => Observed::Unchanged(e.seq),
    })
}

/// Records that the daily full read confirmed the item is gone (10.2).
pub fn item_gone(
    tx: &mut Tx<'_>,
    node_id: &str,
    alias: &str,
    read_id: &str,
    read_at: DateTime<Utc>,
) -> Result<Recorded> {
    tx.record(
        Kind::ItemGone,
        node_id,
        json!({"read_id": read_id}),
        json!({"alias": alias, "read_id": read_id, "read_at": format_time(read_at)}),
        Dedupe::Any,
    )
}
