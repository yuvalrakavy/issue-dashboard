//! Ground truth (triage spec 3.1, 3.3): the records an (item, decision) has, each with its
//! provenance, and the truth folded from them.
//!
//! | Provenance | From |
//! |---|---|
//! | `filed` | a label added within [`FILED_WITHIN_MINUTES`] of the issue's creation |
//! | `github_later` | any other label event |
//! | `bulk_review:<name>` | a label event a `truth_tag` event names (3.3) |
//! | `owner_accept` | `accept_suggestion` |
//! | `owner_edit` | `edit_suggestion`, `set_priority`, `set_override` with type or areas, a dismissed second opinion |
//!
//! **Folding:** the records are replayed in time order; a label adds to or removes from the
//! decision's set, an owner record replaces it. Type and priority have a truth when the set
//! holds exactly one value, area always (an empty set is left out of area's eval). The truth's
//! provenance is the latest record's. An evaluation drops the records of an excluded provenance
//! before folding. An undone decision is not effective (`decision::effective`), so it leaves no
//! record; an apply-all accept (`value.bulk`, `owner_bulk`) is never one.
//!
//! **Closure truth** is bound to its own suggestion document: accept, or the owner's close of
//! the item within [`CLOSURE_WINDOW_DAYS`] of the suggestion with the matching reason, is true;
//! a dismissal with a reason is false; anything else is no truth.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::{Value, json};

use crate::config::Labels;
use crate::decision::{self, Status};
use crate::docs::{Action, CloseReason};
use crate::event::Kind;
use crate::history::{HistoryItem, LabelChange};
use crate::ledger;
use crate::root::parse_time;
use crate::suggest;
use crate::translate::Translation;
use crate::{Error, Result};

/// A label set this soon after the issue was created was set at filing (3.1).
pub const FILED_WITHIN_MINUTES: i64 = 10;

/// A close counts as a closure verdict this long after its suggestion (3.1).
pub const CLOSURE_WINDOW_DAYS: i64 = 14;

/// The closure decisions, judged live (3.2).
pub const CLOSURE_DECISIONS: [&str; 4] = ["likely_fixed", "duplicate", "obsolete", "support_question"];

/// What one record does to a decision's set.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Op {
    Add(String),
    Remove(String),
    Set(Vec<String>),
}

/// One truth record of an (item, decision).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Record {
    pub item: String,
    pub decision: String,
    pub at: DateTime<Utc>,
    pub provenance: String,
    pub op: Op,
    /// The backend of the suggestion an owner record answered (3.1 `from_backend`).
    pub from_backend: Option<String>,
    /// That suggestion's top answer and question, for drift (3.5).
    pub suggested: Option<Value>,
    pub question: Option<String>,
}

/// The truth of an (item, decision).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Truth {
    /// A string for a single choice; a sorted array of areas for `area`.
    pub value: Value,
    pub provenance: String,
    pub at: DateTime<Utc>,
}

fn hit(names: &Labels, label: &str) -> bool {
    names.iter().any(|n| n.eq_ignore_ascii_case(label))
}

/// The (decision, value) pairs a label means in `t`: a label decision's only.
pub fn label_meaning(t: &Translation, label: &str) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    for (v, names) in &t.types {
        if hit(names, label) {
            out.push(("type", v.clone()));
        }
    }
    for (p, names) in &t.priorities {
        if hit(names, label) {
            out.push((
                "priority",
                serde_json::to_value(p)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_string))
                    .unwrap_or_default(),
            ));
        }
    }
    for (a, names) in &t.areas {
        if hit(names, label) {
            out.push(("area", a.clone()));
        }
    }
    out
}

/// The provenance of one label event (3.1): tagged, set at filing, or later.
pub fn label_provenance(item: &HistoryItem, e: &LabelChange, tags: &BTreeMap<String, String>) -> Result<String> {
    if let Some(name) = tags.get(&e.id) {
        return Ok(format!("bulk_review:{name}"));
    }
    let filed =
        e.added && parse_time(&e.at)? - parse_time(&item.created_at)? <= Duration::minutes(FILED_WITHIN_MINUTES);
    Ok(if filed { "filed" } else { "github_later" }.to_string())
}

/// The records an issue's label events give, in time order. A value stays while any label
/// meaning it is on the issue: removing `enhancement` beside a `todo` label (both todo) is no
/// removal, whichever of the two events GitHub lists first.
pub fn label_records(t: &Translation, item: &HistoryItem, tags: &BTreeMap<String, String>) -> Result<Vec<Record>> {
    let mut out = Vec::new();
    let mut on: Vec<&str> = Vec::new();
    for e in &item.label_events {
        if e.added {
            on.push(&e.label);
        } else {
            on.retain(|l| !l.eq_ignore_ascii_case(&e.label));
        }
        for (decision, value) in label_meaning(t, &e.label) {
            let still = on
                .iter()
                .any(|l| label_meaning(t, l).iter().any(|(d, v)| *d == decision && *v == value));
            if !e.added && still {
                continue;
            }
            out.push(Record {
                item: item.id.clone(),
                decision: decision.to_string(),
                at: parse_time(&e.at)?,
                provenance: label_provenance(item, e, tags)?,
                op: if e.added { Op::Add(value) } else { Op::Remove(value) },
                from_backend: None,
                suggested: None,
                question: None,
            });
        }
    }
    Ok(out)
}

/// Folds `records` of one (item, decision) (3.1), skipping the provenances in `exclude`.
pub fn fold(decision: &str, records: &[Record], exclude: &BTreeSet<String>) -> Option<Truth> {
    let mut kept: Vec<&Record> = records
        .iter()
        .filter(|r| r.decision == decision && !exclude.contains(&r.provenance))
        .collect();
    kept.sort_by_key(|r| r.at);
    let last = kept.last()?;
    let mut set: BTreeSet<String> = BTreeSet::new();
    for r in &kept {
        match &r.op {
            Op::Add(v) => {
                set.insert(v.clone());
            }
            Op::Remove(v) => {
                set.remove(v);
            }
            Op::Set(vs) => set = vs.iter().cloned().collect(),
        }
    }
    let value = if decision == "area" {
        json!(set)
    } else if set.len() == 1 {
        json!(set.into_iter().next().expect("one value"))
    } else {
        return None;
    };
    Some(Truth {
        value,
        provenance: last.provenance.clone(),
        at: last.at,
    })
}

/// Every `truth_tag` event of `project`: label event id → review name (the latest tag wins).
pub fn tags(conn: &Connection, project: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for e in ledger::events_by_kind(conn, Kind::TruthTag)? {
        if e.data["project"] == project
            && let Some(name) = e.data["name"].as_str()
        {
            out.insert(e.subject.clone(), name.to_string());
        }
    }
    Ok(out)
}

/// A recorded suggestion, as a page decision names it by its document id.
#[derive(Debug, Clone)]
pub struct Suggested {
    pub item: String,
    pub decision: String,
    pub at: DateTime<Utc>,
    pub body: Value,
}

/// Every suggestion of `project` by the document id a page decision names it by.
pub fn suggestions(conn: &Connection, project: &str) -> Result<BTreeMap<String, Suggested>> {
    let mut out = BTreeMap::new();
    for e in ledger::events_by_kind(conn, Kind::Suggested)? {
        if e.data["project"] != project {
            continue;
        }
        let decision = e.distinct["decision"].as_str().unwrap_or_default().to_string();
        let run = e.distinct["run_id"].as_str().unwrap_or_default();
        out.insert(
            suggest::doc_id(run, &decision, &e.subject),
            Suggested {
                item: e.subject.clone(),
                decision,
                at: parse_time(&e.time)?,
                body: e.data["suggestion"].clone(),
            },
        );
    }
    Ok(out)
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

fn priority_str(p: crate::docs::Priority) -> String {
    serde_json::to_value(p)
        .ok()
        .and_then(|v| v.as_str().map(str::to_string))
        .unwrap_or_default()
}

/// The owner records of `project`'s effective decisions (3.1), for the label decisions and the
/// Details decisions; closures are [`closure_truth`]'s.
pub fn owner_records(conn: &Connection, project: &str) -> Result<Vec<Record>> {
    let found = suggestions(conn, project)?;
    let mut out = Vec::new();
    for d in decision::effective(conn, project)? {
        let at = parse_time(&d.created_at)?;
        let value = d.value.clone().unwrap_or_default();
        let about = d.target.suggestion.as_ref().and_then(|s| found.get(s));
        let from = |s: &Suggested, provenance: &str, decision: &str, set: Vec<String>| Record {
            item: s.item.clone(),
            decision: decision.to_string(),
            at,
            provenance: provenance.to_string(),
            op: Op::Set(set),
            from_backend: s.body["backend"].as_str().map(str::to_string),
            suggested: Some(s.body["answer"].clone()),
            question: s.body["question"].as_str().map(str::to_string),
        };
        let plain = |item: &str, decision: &str, set: Vec<String>| Record {
            item: item.to_string(),
            decision: decision.to_string(),
            at,
            provenance: "owner_edit".to_string(),
            op: Op::Set(set),
            from_backend: None,
            suggested: None,
            question: None,
        };
        match d.action {
            Action::AcceptSuggestion => {
                // Apply all is owner_bulk: never truth (3.1, 4.3).
                if value.bulk == Some(true) {
                    continue;
                }
                if let Some(s) = about
                    && !CLOSURE_DECISIONS.contains(&s.decision.as_str())
                {
                    out.push(from(s, "owner_accept", &s.decision, strings(&s.body["answer"])));
                }
            }
            Action::EditSuggestion => {
                if let Some(s) = about {
                    let set = match s.decision.as_str() {
                        "type" => value.type_.clone().map(|t| vec![t]),
                        "area" => value.areas.clone(),
                        "priority" => value.priority.map(|p| vec![priority_str(p)]),
                        _ => None,
                    };
                    if let Some(set) = set {
                        out.push(from(s, "owner_edit", &s.decision, set));
                    }
                }
            }
            Action::DismissSuggestion => {
                // A dismissed second opinion confirms the current label (3.1, 4.2).
                if let Some(s) = about
                    && let Some(current) = s.body.get("second_opinion").and_then(|o| o.get("current"))
                {
                    out.push(from(s, "owner_edit", &s.decision, strings(current)));
                }
            }
            Action::SetPriority => {
                if let (Some(item), Some(p)) = (&d.target.item, value.priority) {
                    out.push(plain(item, "priority", vec![priority_str(p)]));
                }
            }
            Action::SetOverride => {
                if let Some(item) = &d.target.item {
                    if let Some(t) = &value.type_ {
                        out.push(plain(item, "type", vec![t.clone()]));
                    }
                    if let Some(a) = &value.areas {
                        out.push(plain(item, "area", a.clone()));
                    }
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// The truth of one closure suggestion (3.1).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ClosureTruth {
    pub suggestion: String,
    pub item: String,
    pub decision: String,
    pub value: bool,
    pub provenance: String,
    pub at: DateTime<Utc>,
}

/// The number of an alias owner/repo#N.
fn number_of(alias: &str) -> Option<u64> {
    alias.rsplit_once('#')?.1.parse().ok()
}

/// The first close of `item` the ledger observed after `after`: (closed_at, state_reason).
fn close_after(conn: &Connection, item: &str, after: DateTime<Utc>) -> Result<Option<(DateTime<Utc>, String)>> {
    for e in ledger::events_of(conn, item, Kind::Observed)? {
        let c = &e.data["content"];
        if c["state"] != "closed" {
            continue;
        }
        let Some(at) = c["closed_at"].as_str() else { continue };
        let at = parse_time(at)?;
        if at >= after {
            return Ok(Some((at, c["state_reason"].as_str().unwrap_or_default().to_string())));
        }
    }
    Ok(None)
}

/// The truth of every closure suggestion of `project` that has one (3.1): bound to the
/// suggestion's own document; the latest verdict wins.
pub fn closure_truth(conn: &Connection, project: &str) -> Result<Vec<ClosureTruth>> {
    let found = suggestions(conn, project)?;
    let resolved = decision::resolved(conn, project)?;
    let window = Duration::days(CLOSURE_WINDOW_DAYS);
    let mut out = Vec::new();
    for (doc, s) in &found {
        if !CLOSURE_DECISIONS.contains(&s.decision.as_str()) || s.body["answer"] != json!(true) {
            continue;
        }
        let mut verdicts: Vec<(DateTime<Utc>, bool, &str)> = Vec::new();
        for (_, d, status) in &resolved {
            if *status != Status::Applied {
                continue;
            }
            let at = parse_time(&d.created_at)?;
            let mine = d.target.suggestion.as_deref() == Some(doc.as_str());
            match d.action {
                Action::AcceptSuggestion if mine => verdicts.push((at, true, "owner_accept")),
                Action::DismissSuggestion if mine => {
                    if d.reason.as_deref().is_some_and(|r| !r.trim().is_empty()) {
                        verdicts.push((at, false, "owner_edit"));
                    }
                }
                // A duplicate close must name the suggested other issue (3.1).
                Action::Close if s.decision == "duplicate" && d.target.item.as_deref() == Some(s.item.as_str()) => {
                    let named = d.value.as_ref().and_then(|v| v.close.as_ref());
                    let related = s.body["related_alias"].as_str().and_then(number_of);
                    if let Some(c) = named
                        && c.reason == CloseReason::Duplicate
                        && at >= s.at
                        && at - s.at <= window
                        && c.duplicate_of.is_some()
                        && c.duplicate_of == related
                    {
                        verdicts.push((at, true, "owner_accept"));
                    }
                }
                _ => {}
            }
        }
        if s.decision != "duplicate"
            && let Some((at, reason)) = close_after(conn, &s.item, s.at)?
            && at - s.at <= window
        {
            let matches = match reason.as_str() {
                "completed" => s.decision == "likely_fixed",
                "not_planned" => s.decision == "obsolete" || s.decision == "support_question",
                _ => false,
            };
            if matches {
                verdicts.push((at, true, "owner_accept"));
            }
        }
        verdicts.sort_by_key(|v| v.0);
        if let Some((at, value, provenance)) = verdicts.last() {
            out.push(ClosureTruth {
                suggestion: doc.clone(),
                item: s.item.clone(),
                decision: s.decision.clone(),
                value: *value,
                provenance: provenance.to_string(),
                at: *at,
            });
        }
    }
    Ok(out)
}

/// One label event `truth tag` lists (3.3).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Tagged {
    pub event: String,
    pub item: String,
    pub alias: String,
    pub label: String,
    pub added: bool,
    pub at: String,
}

/// The later label events of `decision` in [from, to] (3.3), optionally of the named issues
/// only: what `truth tag` lists and, with `--yes`, records.
pub fn taggable(
    config: &crate::config::ConfigSet,
    history: &[crate::history::RepoHistory],
    decision: &str,
    from: DateTime<Utc>,
    to: DateTime<Utc>,
    issues: &[String],
) -> Result<Vec<Tagged>> {
    if !crate::triage::LABEL_DECISIONS.contains(&decision) {
        return Err(Error::refused(format!(
            "truth tag tags label decisions (type, area, priority), not {decision}"
        )));
    }
    let none = BTreeMap::new();
    let mut out = Vec::new();
    for h in history {
        let t = crate::triage::translation_of(config, &h.repo);
        for item in &h.items {
            if !issues.is_empty() && !issues.iter().any(|i| i.eq_ignore_ascii_case(&item.alias)) {
                continue;
            }
            for e in &item.label_events {
                let at = parse_time(&e.at)?;
                if at < from || at > to || label_provenance(item, e, &none)? != "github_later" {
                    continue;
                }
                if label_meaning(&t, &e.label).iter().any(|(d, _)| *d == decision) {
                    out.push(Tagged {
                        event: e.id.clone(),
                        item: item.id.clone(),
                        alias: item.alias.clone(),
                        label: e.label.clone(),
                        added: e.added,
                        at: e.at.clone(),
                    });
                }
            }
        }
    }
    Ok(out)
}

/// Records one `truth_tag` event per listed label event (3.3); a repeat is not recorded again.
pub fn record_tags(
    tx: &mut ledger::Tx<'_>,
    project: &str,
    name: &str,
    decision: &str,
    tagged: &[Tagged],
) -> Result<usize> {
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)) {
        return Err(Error::refused(format!(
            "--name {name}: a review name is letters, digits, '.', '_' and '-'"
        )));
    }
    let mut new = 0;
    for t in tagged {
        let r = tx.record_valid(
            Kind::TruthTag,
            &t.event,
            json!({"name": name}),
            json!({
                "project": project,
                "name": name,
                "decision": decision,
                "item": t.item,
                "alias": t.alias,
                "label": t.label,
                "added": t.added,
                "at": t.at,
            }),
            ledger::Dedupe::Any,
        )?;
        if r.is_new() {
            new += 1;
        }
    }
    Ok(new)
}
