//! Ingest (10.4 step 1, 10.5): the page's documents become events.
//!
//! The pages file is what the sync job fetched from the page's collections:
//! `{"decisions": [...], "launches": [...], "views": [...], "requests": [...]}`. Every document
//! is checked against its schema; one that fails, or that a check refuses, is skipped and
//! reported, and never fails the cycle: the rest are still ingested.
//!
//! - Decisions go through [`decision::ingest`], which records a non-owner's as refused.
//! - Launch requests, saved views and requests count only from the owner, for this project;
//!   any other is skipped (there is no refused form of those events).
//! - After the decisions, [`apply_cancels`] withdraws every launch an effective
//!   `cancel_launch` names, so a cancel that arrived before its launch takes effect when the
//!   launch arrives.

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::decision;
use crate::docs::{Action, Decision, LaunchRequest};
use crate::event::Kind;
use crate::launch::{self, State, Via};
use crate::ledger::{self, Dedupe, Tx};
use crate::schema;
use crate::{Error, Result};

/// The page documents one cycle ingests.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Pages {
    #[serde(default)]
    pub decisions: Vec<Value>,
    #[serde(default)]
    pub launches: Vec<Value>,
    #[serde(default)]
    pub views: Vec<Value>,
    #[serde(default)]
    pub requests: Vec<Value>,
}

/// A document ingest did not record.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Skipped {
    pub collection: &'static str,
    pub id: Option<String>,
    pub reason: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Report {
    /// New `decided` events, accepted or refused.
    pub decided: usize,
    pub refused: usize,
    pub launches: usize,
    pub views: usize,
    pub requests: usize,
    /// Launches withdrawn by a `cancel_launch` this ingest.
    pub withdrawn: Vec<String>,
    pub skipped: Vec<Skipped>,
}

fn id_of(doc: &Value) -> Option<String> {
    doc.get("id").and_then(|i| i.as_str()).map(str::to_string)
}

/// Runs `f`; a refusal skips the document instead of failing the cycle. A refusal is always
/// raised before anything is inserted, so the transaction is still clean.
fn guarded(
    skipped: &mut Vec<Skipped>,
    collection: &'static str,
    doc: &Value,
    f: impl FnOnce() -> Result<()>,
) -> Result<()> {
    match f() {
        Err(Error::Refused(reason)) => {
            skipped.push(Skipped {
                collection,
                id: id_of(doc),
                reason,
            });
            Ok(())
        }
        other => other,
    }
}

fn owned(doc: &Value, project: &str, owner_id: &str) -> Result<()> {
    let p = doc.get("project").and_then(|p| p.as_str()).unwrap_or_default();
    let a = doc.get("author").and_then(|a| a.as_str()).unwrap_or_default();
    if p != project {
        return Err(Error::refused(format!("belongs to project {p}, not {project}")));
    }
    if a != owner_id {
        return Err(Error::refused(format!("author {a} is not the project's owner_id")));
    }
    Ok(())
}

/// Records a saved view or a request: subject the document id, one event per document. The
/// same document again is a repeat; a different one under the same id is refused.
pub(crate) fn record_doc(tx: &mut Tx<'_>, kind: Kind, project: &str, doc: &Value) -> Result<bool> {
    let id = id_of(doc).unwrap_or_default();
    if let Some(e) = ledger::latest(tx.conn(), &id, kind)? {
        if e.data.get("document") == Some(doc) {
            return Ok(false);
        }
        return Err(Error::refused(format!(
            "{} {id} was already ingested (seq {}) with a different document; page documents are never changed",
            kind.as_str(),
            e.seq
        )));
    }
    let r = tx.record_valid(
        kind,
        &id,
        json!({}),
        json!({"project": project, "document": doc}),
        Dedupe::Any,
    )?;
    Ok(r.is_new())
}

/// Ingests one cycle's page documents for `project`.
pub fn ingest(tx: &mut Tx<'_>, project: &str, owner_id: &str, pages: &Pages) -> Result<Report> {
    let mut report = Report::default();
    let mut skipped = Vec::new();

    // Launches first, so a cancel in the same fetch finds its launch.
    for doc in &pages.launches {
        guarded(&mut skipped, "launches", doc, || {
            schema::validate("page-launch", "launch document", doc)?;
            if doc.get("not_sent").is_some() || doc.get("thread").is_some() {
                return Err(Error::refused(
                    "not_sent records and thread records are not ingested in phase 1",
                ));
            }
            owned(doc, project, owner_id)?;
            let req: LaunchRequest = serde_json::from_value(doc.clone())?;
            if !launch::request(tx, &req, Via::Ingest)?.repeat {
                report.launches += 1;
            }
            Ok(())
        })?;
    }
    for doc in &pages.decisions {
        guarded(&mut skipped, "decisions", doc, || {
            schema::validate("page-decision", "decision document", doc)?;
            let d: Decision = serde_json::from_value(doc.clone())?;
            let r = decision::ingest(tx, project, owner_id, &d)?;
            if r.is_new() {
                report.decided += 1;
                if r.event().data.get("accepted") == Some(&Value::Bool(false)) {
                    report.refused += 1;
                }
            }
            Ok(())
        })?;
    }
    report.withdrawn = apply_cancels(tx, project)?;
    for (collection, kind, stem, list) in [
        ("views", Kind::ViewSaved, "page-view", &pages.views),
        ("requests", Kind::Request, "page-request", &pages.requests),
    ] {
        for doc in list {
            guarded(&mut skipped, collection, doc, || {
                schema::validate(stem, collection, doc)?;
                owned(doc, project, owner_id)?;
                if record_doc(tx, kind, project, doc)? {
                    match kind {
                        Kind::ViewSaved => report.views += 1,
                        _ => report.requests += 1,
                    }
                }
                Ok(())
            })?;
        }
    }
    report.skipped = skipped;
    Ok(report)
}

/// Withdraws every requested or queued launch of `project` that an effective `cancel_launch`
/// names. Idempotent; run on every ingest and after `reserve --request`, so the order in which
/// a cancel and its launch arrive does not matter. A launch already reserved is past
/// cancelling: the decision's ack reports it refused.
pub fn apply_cancels(tx: &mut Tx<'_>, project: &str) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for d in decision::effective(tx.conn(), project)? {
        if d.action != Action::CancelLaunch {
            continue;
        }
        let Some(id) = d.target.launch.as_deref() else { continue };
        let Some(l) = launch::load(tx.conn(), id)? else {
            continue;
        };
        if l.request.project != project {
            continue;
        }
        if matches!(l.state, State::Requested | State::Queued) {
            launch::withdraw(tx, id, &d.id)?;
            out.push(id.to_string());
        }
    }
    Ok(out)
}
