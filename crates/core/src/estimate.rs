//! The per-phase estimate (harness spec chapter 1): the sizes a triage session states for an
//! issue's design, implementation and review, the one consistency check every path applies
//! (1.3), the effective estimate (1.7) and when an open issue owes one (1.6).
//!
//! An estimate is never a question: it is recorded unevaluated (triage spec 1.1), so nothing
//! here is calibrated. Minutes, tokens and prices come later, from the seed table (3.1).

use std::collections::BTreeMap;

use chrono::Duration;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::docs::EstimatesEdit;
use crate::event::Kind;
use crate::root::parse_time;
use crate::{Result, ledger};

/// How long selection skips an item whose estimate was left out twice in a row (1.6).
pub const LEFT_OUT_SKIP_DAYS: i64 = 7;

/// A phase size (1.1's table).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Size {
    S,
    M,
    L,
}

/// A design size: `none` exactly when the tier is T0 (1.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DesignSize {
    #[serde(rename = "none")]
    None,
    S,
    M,
    L,
}

/// One phase's size with its one-line reason.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phase<T> {
    pub size: T,
    pub reason: String,
}

/// The three phases of an estimate as a session states them (1.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Phases {
    pub design: Phase<DesignSize>,
    pub implementation: Phase<Size>,
    pub review: Phase<Size>,
}

/// The phase sizes an owner's Details edit sets (1.7); each optional, a later edit's size
/// over an earlier one's. Reasons stay the session's.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PhasesEdit {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub design: Option<DesignSize>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub implementation: Option<Size>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub review: Option<Size>,
}

impl PhasesEdit {
    pub fn is_empty(&self) -> bool {
        self.design.is_none() && self.implementation.is_none() && self.review.is_none()
    }

    /// `later`'s sizes over this one's.
    pub fn merge(&mut self, later: &PhasesEdit) {
        if later.design.is_some() {
            self.design = later.design;
        }
        if later.implementation.is_some() {
            self.implementation = later.implementation;
        }
        if later.review.is_some() {
            self.review = later.review;
        }
    }
}

/// The consistency rules (1.3), applied to whatever is known: a rule whose inputs are not all
/// known is not applied. Returns every rule broken, each as a sentence; empty: consistent.
///
/// The page applies the same rules for immediate feedback (`page/core.js` `estimateProblems`);
/// `crates/core/tests/fixtures/estimate-check.json` holds the cases both must agree on.
pub fn check(tier: Option<&str>, design: Option<DesignSize>, review: Option<Size>) -> Vec<String> {
    let mut out = Vec::new();
    if let (Some(t), Some(d)) = (tier, design) {
        if t == "T0" && d != DesignSize::None {
            out.push("a T0 issue has no design: design is none".to_string());
        }
        if t != "T0" && d == DesignSize::None {
            out.push(format!("a {t} issue has a design: design is S, M or L"));
        }
        if (t == "T2" || t == "T3") && d == DesignSize::S {
            out.push(format!("a {t} issue's design is at least M"));
        }
    }
    if let (Some("T3"), Some(Size::S)) = (tier, review) {
        out.push("a T3 issue's review is at least M".to_string());
    }
    out
}

/// An item's effective estimate (1.7): its latest recorded estimate that is not stale, with
/// the owner's Details edits merged over it. Every field may be unknown: an old estimate has
/// no phases, an edit alone has no recorded fields.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Effective {
    pub tier: Option<String>,
    pub design: Option<DesignSize>,
    pub implementation: Option<Size>,
    pub review: Option<Size>,
}

impl Effective {
    /// `recorded` is a recorded estimate's `estimate` object (any shape the ledger holds:
    /// an old one has `effort` and no `phases`); `edit` the owner's merged edit.
    pub fn of(recorded: Option<&Value>, edit: Option<&EstimatesEdit>) -> Effective {
        let mut e = Effective::default();
        if let Some(r) = recorded {
            e.tier = r["tier"].as_str().map(str::to_string);
            if let Ok(p) = serde_json::from_value::<Phases>(r["phases"].clone()) {
                e.design = Some(p.design.size);
                e.implementation = Some(p.implementation.size);
                e.review = Some(p.review.size);
            }
        }
        if let Some(x) = edit {
            if x.tier.is_some() {
                e.tier = x.tier.clone();
            }
            if let Some(p) = &x.phases {
                e.design = p.design.or(e.design);
                e.implementation = p.implementation.or(e.implementation);
                e.review = p.review.or(e.review);
            }
        }
        e
    }

    /// Every phase has a size: the estimate an offer can price (1.6's "has `phases`").
    pub fn has_phases(&self) -> bool {
        self.design.is_some() && self.implementation.is_some() && self.review.is_some()
    }

    pub fn problems(&self) -> Vec<String> {
        check(self.tier.as_deref(), self.design, self.review)
    }
}

/// Each item's latest recorded estimate of `project` that is not stale (1.6, 1.7): the
/// `suggestion` of its latest `estimates` event.
pub fn recorded(conn: &Connection, project: &str) -> Result<BTreeMap<String, Value>> {
    let mut out = BTreeMap::new();
    for e in ledger::events_by_kind(conn, Kind::Suggested)? {
        if e.data["project"] != project || e.distinct["decision"] != "estimates" {
            continue;
        }
        let s = &e.data["suggestion"];
        if s["stale"] == true {
            continue;
        }
        out.insert(e.subject.clone(), s.clone());
    }
    Ok(out)
}

/// An estimate obligation (1.6): `prepare` asked the session for a phased estimate of `item`,
/// whose issue was last updated at `updated_at`. Recorded in the `prepared` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Obligation {
    pub item: String,
    pub alias: String,
    pub updated_at: String,
}

/// The obligations of `launch` that no recorded `estimates` or `left_out` of that launch
/// answers yet.
pub fn unanswered(conn: &Connection, project: &str, launch: &str, obligations: Vec<Obligation>) -> Result<Vec<Obligation>> {
    let mut done = std::collections::BTreeSet::new();
    for e in ledger::events_by_kind(conn, Kind::Suggested)? {
        if e.data["project"] == project
            && e.distinct["run_id"] == launch
            && (e.distinct["decision"] == "estimates" || e.distinct["decision"] == "left_out")
        {
            done.insert(e.subject.clone());
        }
    }
    Ok(obligations.into_iter().filter(|o| !done.contains(&o.item)).collect())
}

/// What `project`'s estimate selection reads (1.6): the effective estimates and the items
/// skipped for being left out twice in a row.
pub struct Owing {
    recorded: BTreeMap<String, Value>,
    edits: BTreeMap<String, EstimatesEdit>,
    /// Per item, when its skip ends.
    skipped_until: BTreeMap<String, chrono::DateTime<chrono::Utc>>,
}

impl Owing {
    pub fn load(conn: &Connection, project: &str, edits: &BTreeMap<String, EstimatesEdit>) -> Result<Owing> {
        Ok(Owing {
            recorded: recorded(conn, project)?,
            edits: edits.clone(),
            skipped_until: skipped_until(conn, project)?,
        })
    }

    pub fn effective(&self, item: &str) -> Effective {
        Effective::of(self.recorded.get(item).map(|s| &s["estimate"]), self.edits.get(item))
    }

    /// The item owes a phased estimate now: its effective estimate has no phases, and it was
    /// not left out twice in a row within the last [`LEFT_OUT_SKIP_DAYS`].
    pub fn due(&self, item: &str, now: &str) -> Result<bool> {
        if self.effective(item).has_phases() {
            return Ok(false);
        }
        Ok(match self.skipped_until.get(item) {
            Some(until) => parse_time(now)? >= *until,
            None => true,
        })
    }
}

/// Per item, the end of its skip: when the last two of its obligations that were answered
/// at all were both answered by a `left_out`, the second one's time plus seven days. An
/// obligation its launch never answered counts neither way.
fn skipped_until(conn: &Connection, project: &str) -> Result<BTreeMap<String, chrono::DateTime<chrono::Utc>>> {
    // Per (launch, item): what answered the obligation, and when.
    let mut answered: BTreeMap<(String, String), (bool, String)> = BTreeMap::new();
    for e in ledger::events_by_kind(conn, Kind::Suggested)? {
        if e.data["project"] != project {
            continue;
        }
        let decision = e.distinct["decision"].as_str().unwrap_or_default();
        let left_out = match decision {
            "left_out" => true,
            "estimates" => false,
            _ => continue,
        };
        let run = e.distinct["run_id"].as_str().unwrap_or_default().to_string();
        answered.insert((run, e.subject.clone()), (left_out, e.time.clone()));
    }
    // Per item, its answered obligations in the order they were prepared.
    let mut history: BTreeMap<String, Vec<(bool, String)>> = BTreeMap::new();
    for e in ledger::events_by_kind(conn, Kind::Prepared)? {
        if e.data["project"] != project {
            continue;
        }
        let Some(list) = e.data["obligations"].as_array() else {
            continue;
        };
        let launch = e.data["launch"].as_str().unwrap_or_default().to_string();
        for o in list {
            let item = o["item"].as_str().unwrap_or_default().to_string();
            if let Some(a) = answered.get(&(launch.clone(), item.clone())) {
                let h = history.entry(item).or_default();
                // A repeated prepare of the same launch is one obligation.
                if h.last() != Some(a) {
                    h.push(a.clone());
                }
            }
        }
    }
    let mut out = BTreeMap::new();
    for (item, h) in history {
        if let [.., (true, _), (true, second)] = h.as_slice() {
            out.insert(item, parse_time(second)? + Duration::days(LEFT_OUT_SKIP_DAYS));
        }
    }
    Ok(out)
}
