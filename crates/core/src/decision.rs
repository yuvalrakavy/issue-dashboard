//! Page decisions: authorship, ordering and undo (10.5).
//!
//! - Only the owner's decisions count: ingest records any other as refused.
//! - Undo is a decision naming another. Whichever arrives first, the named decision is
//!   cancelled and never has an effect: cancellation is derived from every ingested undo
//!   whenever decisions are read, not applied once at arrival.
//! - Decisions take effect in sequence order; two about the same item resolve to the later.

use std::collections::{BTreeMap, HashMap, HashSet};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::docs::{Action, Decision, Priority};
use crate::event::Kind;
use crate::ledger::{self, Dedupe, Recorded, Tx};
use crate::Result;

/// The `data` of a `decided` event. Its schema is `schemas/event-decided.schema.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecidedData {
    pub project: String,
    pub author: String,
    pub accepted: bool,
    pub refusal: Option<String>,
    pub document: Decision,
}

/// Records a page decision as a `decided` event. It is accepted only if its author is
/// `owner_id` and it belongs to `project`; otherwise it is recorded as refused. Ingesting
/// the same document again is a no-op.
pub fn ingest(tx: &mut Tx<'_>, project: &str, owner_id: &str, doc: &Decision) -> Result<Recorded> {
    let refusal = if doc.project != project {
        Some(format!("decision {} belongs to project {}, not {project}", doc.id, doc.project))
    } else if doc.author != owner_id {
        Some(format!("author {} is not the project's owner_id", doc.author))
    } else {
        None
    };
    let data = DecidedData {
        project: project.to_string(),
        author: doc.author.clone(),
        accepted: refusal.is_none(),
        refusal,
        document: doc.clone(),
    };
    tx.record(Kind::Decided, &doc.id, json!({}), serde_json::to_value(data)?, Dedupe::Any)
}

/// What became of one ingested decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Applied,
    Refused { reason: String },
    Cancelled { by: String },
}

/// The ingested decisions of `project`, in sequence order.
fn decided(conn: &Connection, project: &str) -> Result<Vec<DecidedData>> {
    let mut out = Vec::new();
    for e in ledger::events_by_kind(conn, Kind::Decided)? {
        let d: DecidedData = serde_json::from_value(e.data)?;
        if d.project == project {
            out.push(d);
        }
    }
    Ok(out)
}

/// The accepted undo that cancels `id`, if any. An undo cancels its target only while it is
/// not itself cancelled; a cycle of undos (which the page cannot produce) cancels nothing.
fn cancelled_by(id: &str, undos: &HashMap<String, Vec<String>>, visiting: &mut HashSet<String>) -> Option<String> {
    if !visiting.insert(id.to_string()) {
        return None;
    }
    let by = undos
        .get(id)
        .into_iter()
        .flatten()
        .find(|u| cancelled_by(u, undos, visiting).is_none())
        .cloned();
    visiting.remove(id);
    by
}

fn resolve(decided: &[DecidedData]) -> Vec<(Decision, Status)> {
    let mut undos: HashMap<String, Vec<String>> = HashMap::new();
    for d in decided.iter().filter(|d| d.accepted && d.document.action == Action::Undo) {
        if let Some(target) = &d.document.target.decision {
            undos.entry(target.clone()).or_default().push(d.document.id.clone());
        }
    }
    decided
        .iter()
        .map(|d| {
            let status = if !d.accepted {
                Status::Refused { reason: d.refusal.clone().unwrap_or_default() }
            } else if let Some(by) = cancelled_by(&d.document.id, &undos, &mut HashSet::new()) {
                Status::Cancelled { by }
            } else {
                Status::Applied
            };
            (d.document.clone(), status)
        })
        .collect()
}

/// The status of every ingested decision of `project`, by decision id.
pub fn statuses(conn: &Connection, project: &str) -> Result<BTreeMap<String, Status>> {
    Ok(resolve(&decided(conn, project)?).into_iter().map(|(d, s)| (d.id, s)).collect())
}

/// The decisions of `project` that take effect, in sequence order: accepted, not cancelled,
/// and not themselves undos.
pub fn effective(conn: &Connection, project: &str) -> Result<Vec<Decision>> {
    Ok(resolve(&decided(conn, project)?)
        .into_iter()
        .filter(|(d, s)| *s == Status::Applied && d.action != Action::Undo)
        .map(|(d, _)| d)
        .collect())
}

/// The project priority each item has from the effective decisions: the later of two
/// decisions about one item wins; a cleared priority is absent.
pub fn project_priorities(conn: &Connection, project: &str) -> Result<BTreeMap<String, Priority>> {
    let mut out = BTreeMap::new();
    for d in effective(conn, project)? {
        let Some(item) = d.target.item.clone() else { continue };
        match d.action {
            Action::SetPriority => {
                if let Some(p) = d.value.as_ref().and_then(|v| v.priority) {
                    out.insert(item, p);
                }
            }
            Action::ClearPriority => {
                out.remove(&item);
            }
            _ => {}
        }
    }
    Ok(out)
}
