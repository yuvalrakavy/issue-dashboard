//! The layers an item's type, areas and priority are taken from (triage spec 4.2): an override,
//! else an accepted (or edited) suggestion, else the GitHub label; priority's middle layer is the
//! project priority, which `set_priority` and an accepted or edited priority suggestion both set.
//! One reading serves derive (the issue document), `record-suggestions` (the value a suggestion
//! saw, `current`) and ingest (a stale accept refuses), so the three never disagree.
//!
//! Details edits (severity, reach, detectability) are kept here too: stored for ordering, never
//! a label.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;
use serde_json::{Value, json};

use crate::config::ConfigSet;
use crate::decision;
use crate::docs::{Action, DecisionValue, Priority};
use crate::event::Kind;
use crate::ledger;
use crate::translate::Canonical;
use crate::triage;
use crate::truth;

/// The decisions a page card sets a label-like value for.
pub const LABEL_DECISIONS: [&str; 3] = ["type", "area", "priority"];

/// The Details decisions an edit stores (4.2).
pub const DETAIL_DECISIONS: [&str; 3] = ["severity", "reach", "detectability"];

/// Where a value came from.
pub const OVERRIDE: &str = "override";
pub const SUGGESTION: &str = "suggestion";
pub const PROJECT: &str = "project";
pub const LABEL: &str = "label";

#[derive(Debug, Clone, Default)]
pub struct Layers {
    pub overrides: BTreeMap<String, (DecisionValue, Option<String>)>,
    /// The project priority: the later of `set_priority` / `clear_priority` and an accepted or
    /// edited priority suggestion.
    pub priorities: BTreeMap<String, Priority>,
    /// The accepted (or edited) type suggestion's value, per item.
    pub types: BTreeMap<String, String>,
    /// The accepted (or edited) area suggestion's set, per item.
    pub areas: BTreeMap<String, Vec<String>>,
    /// Details edits: item → decision → the answer the owner set.
    pub details: BTreeMap<String, BTreeMap<String, String>>,
    /// The suggestion documents an applied accept or edit acted on.
    pub acted: BTreeSet<String>,
}

pub(crate) fn priority_str(p: Priority) -> &'static str {
    match p {
        Priority::P0 => "p0",
        Priority::P1 => "p1",
        Priority::P2 => "p2",
        Priority::P3 => "p3",
    }
}

pub(crate) fn parse_priority(s: &str) -> Option<Priority> {
    match s {
        "p0" => Some(Priority::P0),
        "p1" => Some(Priority::P1),
        "p2" => Some(Priority::P2),
        "p3" => Some(Priority::P3),
        _ => None,
    }
}

fn strings(v: &Value) -> Vec<String> {
    match v {
        Value::String(s) => vec![s.clone()],
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        _ => Vec::new(),
    }
}

/// The value an accept or edit of a `decision` suggestion sets: the suggestion's answer for an
/// accept, the edited value for an edit; `None` when the decision carries none.
fn set_by(action: Action, decision: &str, answer: &Value, value: &DecisionValue) -> Option<Vec<String>> {
    let edited = action == Action::EditSuggestion;
    match decision {
        "type" if edited => value.type_.clone().map(|t| vec![t]),
        "area" if edited => value.areas.clone(),
        "priority" if edited => value.priority.map(|p| vec![priority_str(p).to_string()]),
        "type" | "area" | "priority" => Some(strings(answer)).filter(|s| !s.is_empty()),
        _ => None,
    }
}

/// The layers of `project` from its effective decisions, in sequence order.
pub fn layers(conn: &Connection, project: &str) -> crate::Result<Layers> {
    let effective = decision::effective(conn, project)?;
    let found = if effective
        .iter()
        .any(|d| matches!(d.action, Action::AcceptSuggestion | Action::EditSuggestion))
    {
        truth::suggestions(conn, project)?
    } else {
        BTreeMap::new()
    };
    let mut out = Layers {
        overrides: decision::overrides(conn, project)?,
        ..Default::default()
    };
    for d in effective {
        let value = d.value.clone().unwrap_or_default();
        match d.action {
            Action::SetPriority => {
                if let (Some(item), Some(p)) = (d.target.item.clone(), value.priority) {
                    out.priorities.insert(item, p);
                }
            }
            Action::ClearPriority => {
                if let Some(item) = &d.target.item {
                    out.priorities.remove(item);
                }
            }
            Action::AcceptSuggestion | Action::EditSuggestion => {
                let about = d.target.suggestion.as_ref().and_then(|s| found.get(s).map(|f| (s, f)));
                if let Some((doc, s)) = about
                    && let Some(set) = set_by(d.action, &s.decision, &s.body["answer"], &value)
                {
                    out.acted.insert(doc.clone());
                    match s.decision.as_str() {
                        "type" => {
                            out.types.insert(s.item.clone(), set[0].clone());
                        }
                        "area" => {
                            out.areas.insert(s.item.clone(), set);
                        }
                        _ => {
                            if let Some(p) = parse_priority(&set[0]) {
                                out.priorities.insert(s.item.clone(), p);
                            }
                        }
                    }
                }
                // A Details edit names its item, and its suggestion when the page had one.
                if d.action == Action::EditSuggestion {
                    let item = d.target.item.clone().or_else(|| about.map(|(_, s)| s.item.clone()));
                    if let Some(item) = item {
                        for (decision, v) in [
                            ("severity", &value.severity),
                            ("reach", &value.reach),
                            ("detectability", &value.detectability),
                        ] {
                            if let Some(v) = v {
                                out.details
                                    .entry(item.clone())
                                    .or_default()
                                    .insert(decision.to_string(), v.clone());
                            }
                        }
                    }
                }
            }
            _ => {}
        }
    }
    Ok(out)
}

/// One item's effective type, areas and priority, each with its source.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Effective {
    pub type_: Option<String>,
    pub type_source: Option<&'static str>,
    pub areas: Vec<String>,
    pub areas_source: Option<&'static str>,
    pub priority: Option<Priority>,
    pub priority_source: Option<&'static str>,
}

impl Layers {
    /// The effective values of `item`, whose labels translate to `canon`.
    pub fn effective(&self, item: &str, canon: &Canonical) -> Effective {
        let over = self.overrides.get(item).map(|(v, _)| v);
        let (type_, type_source) = match (over.and_then(|v| v.type_.clone()), self.types.get(item)) {
            (Some(t), _) => (Some(t), Some(OVERRIDE)),
            (None, Some(t)) => (Some(t.clone()), Some(SUGGESTION)),
            (None, None) => (canon.type_.clone(), canon.type_.as_ref().map(|_| LABEL)),
        };
        let (areas, areas_source) = match (over.and_then(|v| v.areas.clone()), self.areas.get(item)) {
            (Some(a), _) => (a, Some(OVERRIDE)),
            (None, Some(a)) => (a.clone(), Some(SUGGESTION)),
            (None, None) => (canon.areas.clone(), (!canon.areas.is_empty()).then_some(LABEL)),
        };
        let (priority, priority_source) = match (
            over.and_then(|v| v.priority),
            self.priorities.get(item).copied(),
            canon.priority,
        ) {
            (Some(p), _, _) => (Some(p), Some(OVERRIDE)),
            (None, Some(p), _) => (Some(p), Some(PROJECT)),
            (None, None, Some(p)) => (Some(p), Some(LABEL)),
            _ => (None, None),
        };
        Effective {
            type_,
            type_source,
            areas,
            areas_source,
            priority,
            priority_source,
        }
    }
}

impl Effective {
    /// The value of a label decision as a suggestion records it (`current`): type and priority
    /// a string or null, area the set (empty: none).
    pub fn current(&self, decision: &str) -> Value {
        match decision {
            "type" => json!(self.type_),
            "priority" => json!(self.priority.map(priority_str)),
            "area" => {
                let mut a = self.areas.clone();
                a.sort();
                json!(a)
            }
            _ => Value::Null,
        }
    }
}

/// Whether a recorded `current` says the decision had no value (the item was untriaged on it).
pub fn is_unset(current: &Value) -> bool {
    match current {
        Value::Null => true,
        Value::Array(a) => a.is_empty(),
        _ => false,
    }
}

/// The effective values of observed item `id` of `project`, with its repo's translation.
pub fn effective_of(
    conn: &Connection,
    config: &ConfigSet,
    layers: &Layers,
    id: &str,
) -> crate::Result<Option<Effective>> {
    let Some(e) = ledger::latest(conn, id, Kind::Observed)? else {
        return Ok(None);
    };
    let alias = e.data["alias"].as_str().unwrap_or(id);
    let repo = alias.rsplit_once('#').map_or(alias, |(r, _)| r);
    let labels = strings(&e.data["content"]["labels"]);
    let canon = triage::translation_of(config, repo).translate(&labels);
    Ok(Some(layers.effective(id, &canon)))
}
