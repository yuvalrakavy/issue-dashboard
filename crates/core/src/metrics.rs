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

use std::collections::BTreeMap;

use chrono::{DateTime, Duration, Utc};
use rusqlite::Connection;
use serde_json::{Value, json};

use crate::Result;
use crate::canonical::sha256_hex;
use crate::decision;
use crate::docs::Action;
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
    let mut counts: BTreeMap<Key, Counts> = BTreeMap::new();
    for d in decision::effective(conn, project)? {
        let at = parse_time(&d.created_at)?;
        if at <= since || at > now {
            continue;
        }
        let Some(s) = d.target.suggestion.as_ref().and_then(|id| found.get(id)) else {
            continue;
        };
        let Some((q, _)) = s.body["question"].as_str().and_then(|id| questions.get(id)) else {
            continue;
        };
        let Some(backend) = s.body["backend"].as_str() else {
            continue;
        };
        let key = (backend.to_string(), s.decision.clone(), q.question_hash.clone());
        let c = counts.entry(key).or_default();
        match d.action {
            Action::AcceptSuggestion => {
                c.accepted += 1;
                if d.value.as_ref().and_then(|v| v.bulk) == Some(true) {
                    c.bulk += 1;
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
