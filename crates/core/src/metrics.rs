//! The `metrics` documents of a project (triage spec 4.4): one per triage key (backend,
//! decision, question hash), shown on Trends.
//!
//! Each holds what the owner did with the key's suggestions over the last [`METRICS_DAYS`]
//! days, by the time the page decision was written: accepted (apply all's included, and also
//! counted apart as `bulk`), edited and dismissed, and `of`, their sum, so a ratio always
//! carries its base ("type: 41 of 44"); with the accuracy of the key's latest passing
//! evaluation and its drift flag (3.5). A key shows when the window holds a decision on one of
//! its suggestions or the project holds an evaluation of it. A suggestion with no prepared
//! question (a phase-1 recommender's) has no key and is not counted.
//!
//! A closure suggestion is approved through the close sheet, not an accept: a `close` decision
//! on its item within [`truth::CLOSURE_WINDOW_DAYS`] after it, with the matching reason (a
//! duplicate naming the suggested issue), counts as its accept, as closure truth reads it (3.1).
//! A suggestion is counted accepted once, however many decisions approve it, and a close does
//! not count for a suggestion the owner dismissed or edited before it.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Duration, Utc};
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::Result;
use crate::canonical::sha256_hex;
use crate::decision;
use crate::docs::{Action, CloseReason};
use crate::eval;
use crate::event::Kind;
use crate::ledger;
use crate::root::parse_time;
use crate::triage;
use crate::truth;

/// How many days a metrics document counts back (4.4).
pub const METRICS_DAYS: i64 = 30;

/// (backend, decision, question hash).
type Key = (String, String, String);

#[derive(Debug, Default, Clone, Copy)]
struct Counts {
    accepted: u64,
    bulk: u64,
    edited: u64,
    dismissed: u64,
}

/// The id of the metrics document of a key.
fn doc_id(project: &str, key: &Key) -> String {
    let subject = eval::key_subject(project, &key.0, &key.1, &key.2);
    format!("mt-{}", &sha256_hex(&subject)[..24])
}

/// The `metrics` documents of `project` at `now`, sorted by decision, then backend.
pub fn documents(conn: &Connection, project: &str, now: DateTime<Utc>) -> Result<Vec<Value>> {
    let since = now - Duration::days(METRICS_DAYS);
    let found = truth::suggestions(conn, project)?;
    let questions = triage::prepared_questions(conn)?;
    let key_of = |s: &truth::Suggested| -> Option<Key> {
        let (q, _) = s.body["question"].as_str().and_then(|id| questions.get(id))?;
        let backend = s.body["backend"].as_str()?;
        Some((backend.to_string(), s.decision.clone(), q.question_hash.clone()))
    };
    let window = Duration::days(truth::CLOSURE_WINDOW_DAYS);
    let mut counts: BTreeMap<Key, Counts> = BTreeMap::new();
    let mut accepted: BTreeSet<String> = BTreeSet::new();
    let effective = decision::effective(conn, project)?;
    // When the owner first dismissed or edited each suggestion: a later close is not its accept.
    let mut judged: BTreeMap<String, DateTime<Utc>> = BTreeMap::new();
    for d in &effective {
        if matches!(d.action, Action::DismissSuggestion | Action::EditSuggestion)
            && let Some(id) = d.target.suggestion.as_ref()
        {
            let at = parse_time(&d.created_at)?;
            let first = judged.entry(id.clone()).or_insert(at);
            *first = (*first).min(at);
        }
    }
    for d in effective {
        let at = parse_time(&d.created_at)?;
        if at <= since || at > now {
            continue;
        }
        let (doc, s) = if d.action == Action::Close {
            // The latest closure suggestion on the item this close approves, if any.
            let Some(c) = d.value.as_ref().and_then(|v| v.close.as_ref()) else {
                continue;
            };
            let approved = found
                .iter()
                .filter(|(id, s)| {
                    Some(s.item.as_str()) == d.target.item.as_deref()
                        && judged.get(*id).is_none_or(|j| *j > at)
                        && s.body["answer"] == json!(true)
                        && s.at <= at
                        && at - s.at <= window
                        && match c.reason {
                            CloseReason::Completed => s.decision == "likely_fixed",
                            CloseReason::NotPlanned => s.decision == "obsolete" || s.decision == "support_question",
                            CloseReason::Duplicate => {
                                s.decision == "duplicate"
                                    && c.duplicate_of.is_some()
                                    && c.duplicate_of == s.body["related_alias"].as_str().and_then(truth::number_of)
                            }
                        }
                })
                .max_by_key(|(_, s)| s.at);
            match approved {
                Some(x) => x,
                None => continue,
            }
        } else {
            match d.target.suggestion.as_ref().and_then(|id| found.get_key_value(id)) {
                Some(x) => x,
                None => continue,
            }
        };
        let Some(key) = key_of(s) else {
            continue;
        };
        let c = counts.entry(key).or_default();
        match d.action {
            Action::AcceptSuggestion | Action::Close => {
                if accepted.insert(doc.clone()) {
                    c.accepted += 1;
                    if d.value.as_ref().and_then(|v| v.bulk) == Some(true) {
                        c.bulk += 1;
                    }
                }
            }
            Action::EditSuggestion => c.edited += 1,
            Action::DismissSuggestion => c.dismissed += 1,
            _ => {}
        }
    }
    for e in ledger::events_by_kind(conn, Kind::Evaluated)? {
        if e.data["project"] != project {
            continue;
        }
        // An evaluation names its key; one that does not names none to show.
        let field = |f: &str| e.data[f].as_str().filter(|s| !s.is_empty()).map(str::to_string);
        if let (Some(b), Some(d), Some(h)) = (field("backend"), field("decision"), field("question_hash")) {
            counts.entry((b, d, h)).or_default();
        }
    }
    let mut out = Vec::new();
    for (key, c) in counts {
        let drift = eval::drift(conn, project, &key.0, &key.1, &key.2)?;
        out.push(json!({
            "id": doc_id(project, &key),
            "project": project,
            "kind": "triage_key",
            "period_days": METRICS_DAYS,
            "backend": key.0,
            "decision": key.1,
            "question_hash": key.2,
            "accepted": c.accepted,
            "bulk": c.bulk,
            "edited": c.edited,
            "dismissed": c.dismissed,
            "of": c.accepted + c.edited + c.dismissed,
            "evaluated_accuracy": drift.evaluated_accuracy,
            "drifting": drift.drifting,
        }));
    }
    out.sort_by(|a, b| {
        (a["decision"].as_str(), a["backend"].as_str()).cmp(&(b["decision"].as_str(), b["backend"].as_str()))
    });
    Ok(out)
}
