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

use crate::config::ConfigSet;
use crate::docs::{Action, Decision, DecisionValue, Priority};
use crate::event::{Event, Kind};
use crate::layers;
use crate::ledger::{self, Dedupe, Recorded, Tx};
use crate::root::{format_time, parse_time};
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
    ingest_with(tx, None, project, owner_id, doc)
}

/// [`ingest`], with the checks that read the item's values through the project's labels
/// (triage spec 4.2, 4.3): a stale accept, and an apply-all accept a card may not take.
pub fn ingest_checked(
    tx: &mut Tx<'_>,
    config: &ConfigSet,
    project: &str,
    owner_id: &str,
    doc: &Decision,
) -> Result<Recorded> {
    ingest_with(tx, Some(config), project, owner_id, doc)
}

fn ingest_with(
    tx: &mut Tx<'_>,
    config: Option<&ConfigSet>,
    project: &str,
    owner_id: &str,
    doc: &Decision,
) -> Result<Recorded> {
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
    } else if doc.action == Action::DismissClosed {
        dismiss_refusal(tx.conn(), doc)?
    } else if let (Action::AcceptSuggestion | Action::EditSuggestion | Action::DismissSuggestion, Some(config)) =
        (doc.action, config)
    {
        suggestion_refusal(tx.conn(), config, project, doc)?
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

/// Why a `dismiss_closed` cannot take effect: its item is not closed, or not by the close it
/// names (reopened and closed again since the page showed it).
fn dismiss_refusal(conn: &Connection, doc: &Decision) -> Result<Option<String>> {
    let Some(item) = doc.target.item.as_deref() else {
        return Ok(Some("dismiss_closed names no item".into()));
    };
    let Some(asked) = doc.value.as_ref().and_then(|v| v.closed_at.as_deref()) else {
        return Ok(Some("dismiss_closed names no close (value.closed_at)".into()));
    };
    let Some(seen) = ledger::latest(conn, item, Kind::Observed)? else {
        return Ok(Some(format!("{item} was never read")));
    };
    let content = &seen.data["content"];
    if content["state"] == "open" {
        return Ok(Some(format!("{item} is open: only a closed item can be dismissed")));
    }
    let closed = content["closed_at"].as_str().map(parse_time).transpose()?;
    if closed != Some(parse_time(asked)?) {
        return Ok(Some(format!(
            "{item} was closed at {}, not at {asked}: the close the page showed is gone",
            closed.map_or("an unknown time".into(), format_time)
        )));
    }
    Ok(None)
}

/// Why an `accept_suggestion`, `edit_suggestion` or `dismiss_suggestion` cannot take effect
/// (triage spec 4.2, 4.3):
///
/// - an accept or edit names no recorded suggestion, or one recorded after the owner acted (a
///   later answer must not inherit the verdict); a Details edit with no suggestion names its item;
/// - an edit of a label card carries no value for it;
/// - a label card's item no longer has the value the suggestion saw (`current`): the owner
///   acted on a card about facts that changed (stale); an edit is as an accept, and dismissing a
///   second opinion confirms the current label, so it must still be current;
/// - apply all (`value.bulk`) on anything but an accept of an untriaged item's type, area or
///   priority card, on a second opinion, or on a key that does not pass Bulk or is drifting now.
fn suggestion_refusal(conn: &Connection, config: &ConfigSet, project: &str, doc: &Decision) -> Result<Option<String>> {
    let value = doc.value.clone().unwrap_or_default();
    let bulk = value.bulk == Some(true);
    let dismiss = doc.action == Action::DismissSuggestion;
    let edit = doc.action == Action::EditSuggestion;
    if bulk && doc.action != Action::AcceptSuggestion {
        return Ok(Some("apply all only accepts".to_string()));
    }
    let details = value.severity.is_some() || value.reach.is_some() || value.detectability.is_some();
    let found = match doc.target.suggestion.as_deref() {
        Some(id) => crate::truth::suggestions(conn, project)?.remove(id),
        None => None,
    };
    let Some(s) = found else {
        if dismiss {
            // Phase-1 dismissals name closure suggestions a later run may have replaced.
            return Ok(None);
        }
        if bulk {
            return Ok(Some("apply all names no recorded suggestion".to_string()));
        }
        if edit && details && doc.target.suggestion.is_none() && doc.target.item.is_some() {
            return Ok(None);
        }
        return Ok(Some(format!(
            "{} names no recorded suggestion",
            match doc.action {
                Action::EditSuggestion => "the edit",
                _ => "the accept",
            }
        )));
    };
    let label = layers::LABEL_DECISIONS.contains(&s.decision.as_str());
    let alias = s.body["alias"].as_str().unwrap_or(&s.item).to_string();
    if !dismiss && s.at > crate::root::parse_time(&doc.created_at)? {
        return Ok(Some(format!(
            "{alias}'s {} suggestion was recorded after this decision was made; the card it was made on is gone",
            s.decision
        )));
    }
    if edit && label {
        let has = match s.decision.as_str() {
            "type" => value.type_.is_some(),
            "area" => value.areas.as_ref().is_some_and(|a| !a.is_empty()),
            _ => value.priority.is_some(),
        };
        if !has {
            return Ok(Some(format!(
                "an edit of {alias}'s {} card carries no {}",
                s.decision, s.decision
            )));
        }
    }
    if dismiss && !(label && s.body["second_opinion"].is_object()) {
        return Ok(None);
    }
    if bulk {
        if !label {
            return Ok(Some(format!(
                "apply all takes only type, area and priority cards; {} is not one",
                s.decision
            )));
        }
        if s.body["second_opinion"].is_object() {
            return Ok(Some(format!(
                "apply all never takes a second opinion ({alias}'s {})",
                s.decision
            )));
        }
    }
    if !label {
        return Ok(None);
    }
    let Some(recorded) = s.body.get("current") else {
        if dismiss {
            return Ok(None);
        }
        return Ok(Some(format!(
            "{alias}'s {} suggestion was recorded before cards existed; it has no card to accept",
            s.decision
        )));
    };
    let l = layers::layers(conn, project)?;
    let now = layers::effective_of(conn, config, &l, &s.item)?.map(|e| e.current(&s.decision));
    if now.as_ref() != Some(recorded) {
        let shown = |v: Option<&serde_json::Value>| match v {
            None => "unknown".to_string(),
            Some(v) if layers::is_unset(v) => "unset".to_string(),
            Some(v) => v.to_string(),
        };
        return Ok(Some(format!(
            "stale: {alias}'s {} is now {}, not the {} the suggestion saw",
            s.decision,
            shown(now.as_ref()),
            shown(Some(recorded))
        )));
    }
    if bulk {
        if !layers::is_unset(recorded) {
            return Ok(Some(format!(
                "apply all takes only an untriaged item's card; {alias} has a {}",
                s.decision
            )));
        }
        let (badge, ok) = crate::suggest::Badges::load(conn, project)?.of(&s.body)?;
        if !ok {
            return Ok(Some(format!(
                "apply all: {alias}'s {} key does not pass Bulk or is drifting now (badge {badge})",
                s.decision
            )));
        }
    }
    Ok(None)
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
/// decisions about one item wins (`set_priority`, or an accepted or edited priority suggestion,
/// triage spec 4.2); a cleared priority is absent.
pub fn project_priorities(conn: &Connection, project: &str) -> Result<BTreeMap<String, Priority>> {
    Ok(crate::layers::layers(conn, project)?.priorities)
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

/// The close each item's effective `dismiss_closed` dismissed, with when it was dismissed (the
/// document's `created_at`); the later decision about one item wins.
pub fn dismissals(conn: &Connection, project: &str) -> Result<BTreeMap<String, (String, String)>> {
    let mut out = BTreeMap::new();
    for d in effective(conn, project)? {
        if d.action != Action::DismissClosed {
            continue;
        }
        if let (Some(item), Some(closed)) = (
            d.target.item.clone(),
            d.value.as_ref().and_then(|v| v.closed_at.clone()),
        ) {
            out.insert(item, (closed, d.created_at.clone()));
        }
    }
    Ok(out)
}
