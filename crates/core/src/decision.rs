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

use crate::docs::{Action, Decision, DecisionValue, Priority};
use crate::event::{Event, Kind};
use crate::ledger::{self, Dedupe, Recorded, Tx};
use crate::{Error, Result};

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

/// Records a page decision as a `decided` event, per ingesting project. It is accepted only if
/// its author is `owner_id` and it belongs to `project`; otherwise it is recorded as refused.
///
/// - Ingesting the same document again with the same verdict is a no-op.
/// - A refusal does not consume the document: a later ingest that accepts it (the right project,
///   or `owner_id` corrected in the config) is recorded, and the decision takes effect at that
///   event's place in the sequence.
/// - Once accepted, a decision stays accepted: a later ingest is a repeat whatever it would say.
/// - Page documents are never changed, so a different document under an ingested id is refused.
pub fn ingest(tx: &mut Tx<'_>, project: &str, owner_id: &str, doc: &Decision) -> Result<Recorded> {
    let earlier: Vec<(Event, DecidedData)> = ledger::events_of(tx.conn(), &doc.id, Kind::Decided)?
        .into_iter()
        .map(|e| Ok((e.clone(), serde_json::from_value::<DecidedData>(e.data)?)))
        .collect::<Result<_>>()?;
    if let Some((e, _)) = earlier.iter().find(|(_, d)| &d.document != doc) {
        return Err(Error::refused(format!(
            "decision {} was already ingested (seq {}) with a different document; page documents are never changed",
            doc.id, e.seq
        )));
    }
    if let Some((e, _)) = earlier.iter().find(|(_, d)| d.project == project && d.accepted) {
        return Ok(Recorded::Repeat(e.clone()));
    }
    let refusal = if doc.project != project {
        Some(format!(
            "decision {} belongs to project {}, not {project}",
            doc.id, doc.project
        ))
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
    let distinct = json!({"project": project, "accepted": data.accepted});
    // The repeat is judged against this project's own latest verdict, not the document's
    // latest event overall, which may be another project's.
    if let Some((e, _)) = earlier.iter().rev().find(|(_, d)| d.project == project)
        && e.distinct == distinct
    {
        return Ok(Recorded::Repeat(e.clone()));
    }
    tx.record_valid(
        Kind::Decided,
        &doc.id,
        distinct,
        serde_json::to_value(data)?,
        Dedupe::Latest,
    )
}

/// What became of one ingested decision.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum Status {
    Applied,
    Refused { reason: String },
    Cancelled { by: String },
}

/// The ingested decisions of `project`, one per document, in sequence order with the seq of
/// the chosen event: the accepting event if there is one, else the latest refusal.
fn decided(conn: &Connection, project: &str) -> Result<Vec<(i64, DecidedData)>> {
    let mut chosen: HashMap<String, (i64, DecidedData)> = HashMap::new();
    for e in ledger::events_by_kind(conn, Kind::Decided)? {
        let d: DecidedData = serde_json::from_value(e.data)?;
        if d.project != project {
            continue;
        }
        let keep_earlier = chosen.get(&e.subject).is_some_and(|(_, prev)| prev.accepted);
        if !keep_earlier {
            chosen.insert(e.subject.clone(), (e.seq, d));
        }
    }
    let mut out: Vec<(i64, DecidedData)> = chosen.into_values().collect();
    out.sort_by_key(|(seq, _)| *seq);
    Ok(out)
}

/// The decisions that already had an irreversible effect, with the seq of that effect: a
/// `cancel_launch` that withdrew its launch. An undo accepted after the effect cannot take it
/// back (10.5).
fn effects(conn: &Connection) -> Result<HashMap<String, i64>> {
    let mut out = HashMap::new();
    for e in ledger::events_by_kind(conn, Kind::Launch)? {
        if e.data.get("transition").and_then(|t| t.as_str()) == Some("withdrawn")
            && let Some(d) = e.data.get("decision").and_then(|d| d.as_str())
        {
            out.entry(d.to_string()).or_insert(e.seq);
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

fn resolve(decided: &[(i64, DecidedData)], effects: &HashMap<String, i64>) -> Vec<(Decision, Status)> {
    let mut undos: HashMap<String, Vec<String>> = HashMap::new();
    let mut too_late: HashMap<String, String> = HashMap::new();
    for (seq, d) in decided
        .iter()
        .filter(|(_, d)| d.accepted && d.document.action == Action::Undo)
    {
        if let Some(target) = &d.document.target.decision {
            // Too late when it would cancel, directly or along a chain of undos, a decision whose
            // irreversible effect came before it. Undos are taken in sequence order, each against
            // the ones before it.
            let mut trial = undos.clone();
            trial.entry(target.clone()).or_default().push(d.document.id.clone());
            let reversed = effects
                .iter()
                .filter(|(_, at)| **at < *seq)
                .filter(|(id, _)| {
                    cancelled_by(id, &undos, &mut HashSet::new()).is_none()
                        && cancelled_by(id, &trial, &mut HashSet::new()).is_some()
                })
                .min_by_key(|(_, at)| **at);
            match reversed {
                Some((id, at)) => {
                    too_late.insert(
                        d.document.id.clone(),
                        format!("too late: decision {id} already took effect (seq {at})"),
                    );
                }
                None => undos = trial,
            }
        }
    }
    decided
        .iter()
        .map(|(_, d)| {
            let status = if !d.accepted {
                Status::Refused {
                    reason: d.refusal.clone().unwrap_or_default(),
                }
            } else if let Some(reason) = too_late.get(&d.document.id) {
                Status::Refused { reason: reason.clone() }
            } else if let Some(by) = cancelled_by(&d.document.id, &undos, &mut HashSet::new()) {
                Status::Cancelled { by }
            } else {
                Status::Applied
            };
            (d.document.clone(), status)
        })
        .collect()
}

/// Every ingested decision of `project` with the seq of its chosen event and its status, in
/// sequence order (what the acks of 10.5 report).
pub fn resolved(conn: &Connection, project: &str) -> Result<Vec<(i64, Decision, Status)>> {
    let decided = decided(conn, project)?;
    let seqs: Vec<i64> = decided.iter().map(|(s, _)| *s).collect();
    Ok(seqs
        .into_iter()
        .zip(resolve(&decided, &effects(conn)?))
        .map(|(s, (d, st))| (s, d, st))
        .collect())
}

/// The status of every ingested decision of `project`, by decision id.
pub fn statuses(conn: &Connection, project: &str) -> Result<BTreeMap<String, Status>> {
    Ok(resolved(conn, project)?
        .into_iter()
        .map(|(_, d, s)| (d.id, s))
        .collect())
}

/// The decisions of `project` that take effect, in sequence order: accepted, not cancelled,
/// and not themselves undos.
pub fn effective(conn: &Connection, project: &str) -> Result<Vec<Decision>> {
    Ok(resolved(conn, project)?
        .into_iter()
        .filter(|(_, d, s)| *s == Status::Applied && d.action != Action::Undo)
        .map(|(_, d, _)| d)
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

/// The overrides (4.3) each item has from the effective decisions: `set_override` replaces the
/// item's override, `clear_override` removes it; the later decision wins. Each override comes
/// with the decision's reason.
pub fn overrides(conn: &Connection, project: &str) -> Result<BTreeMap<String, (DecisionValue, Option<String>)>> {
    let mut out = BTreeMap::new();
    for d in effective(conn, project)? {
        let Some(item) = d.target.item.clone() else { continue };
        match d.action {
            Action::SetOverride => {
                if let Some(v) = d.value.clone() {
                    out.insert(item, (v, d.reason.clone()));
                }
            }
            Action::ClearOverride => {
                out.remove(&item);
            }
            _ => {}
        }
    }
    Ok(out)
}
