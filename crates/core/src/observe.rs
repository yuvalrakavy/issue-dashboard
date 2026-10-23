//! Recording what a read of GitHub returned (10.3 `observed` and `item_gone`; 10.4 step 2).
//!
//! An observation is recorded only when its content differs from the item's latest one, and
//! a stale fetch never overwrites a fresher one: an observation older than the item's latest
//! recorded one (by GitHub's `updatedAt`, then by read time) is discarded at commit.

use chrono::{DateTime, SecondsFormat, Utc};
use rusqlite::{OptionalExtension, params};
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

/// An observation's times, stored at full precision so the stale check compares what was read.
fn precise(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::AutoSi, true)
}

/// The freshest (updatedAt, read time) the item has been read at: the latest observation, or
/// a fresher unchanged read kept in the `freshness` table.
fn freshest(tx: &Tx<'_>, node: &str, latest: Option<&ObservedData>) -> Result<Option<(DateTime<Utc>, DateTime<Utc>)>> {
    let mut best = match latest {
        Some(p) => Some((parse_time(&p.updated_at)?, parse_time(&p.read_at)?)),
        None => None,
    };
    let row: Option<(String, String)> = tx
        .conn()
        .query_row(
            "SELECT updated_at, read_at FROM freshness WHERE node = ?1",
            [node],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .optional()?;
    if let Some((u, r)) = row {
        let kept = (parse_time(&u)?, parse_time(&r)?);
        if best.is_none_or(|b| kept > b) {
            best = Some(kept);
        }
    }
    Ok(best)
}

/// Records an observation under the rules above. An unchanged read that is fresher than the
/// item's latest still moves its freshness forward, so an older read arriving later is stale.
pub fn record(tx: &mut Tx<'_>, obs: &Observation) -> Result<Observed> {
    let content_hash = hash_value(&obs.content);
    let latest = ledger::latest(tx.conn(), &obs.node_id, Kind::Observed)?;
    let prev: Option<ObservedData> = latest
        .as_ref()
        .map(|e| serde_json::from_value(e.data.clone()))
        .transpose()?;
    let here = (obs.updated_at, obs.read_at);
    if let (Some(fresh), Some(latest)) = (freshest(tx, &obs.node_id, prev.as_ref())?, &latest)
        && here < fresh
    {
        return Ok(Observed::Stale { latest_seq: latest.seq });
    }
    tx.conn().execute(
        "INSERT INTO freshness (node, updated_at, read_at) VALUES (?1, ?2, ?3)
         ON CONFLICT (node) DO UPDATE SET updated_at = excluded.updated_at, read_at = excluded.read_at",
        params![obs.node_id, precise(obs.updated_at), precise(obs.read_at)],
    )?;
    if let (Some(p), Some(latest)) = (&prev, &latest)
        && p.content_hash == content_hash
        && p.alias == obs.alias
    {
        return Ok(Observed::Unchanged(latest.seq));
    }
    let data = ObservedData {
        item: obs.item,
        alias: obs.alias.clone(),
        updated_at: precise(obs.updated_at),
        read_at: precise(obs.read_at),
        read_id: obs.read_id.clone(),
        content_hash: content_hash.clone(),
        content: obs.content.clone(),
    };
    let r = tx.record_valid(
        Kind::Observed,
        &obs.node_id,
        json!({"content_hash": content_hash, "alias": obs.alias}),
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
    tx.record_valid(
        Kind::ItemGone,
        node_id,
        json!({"read_id": read_id}),
        json!({"alias": alias, "read_id": read_id, "read_at": format_time(read_at)}),
        Dedupe::Any,
    )
}
