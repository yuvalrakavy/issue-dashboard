//! Suggestions from a read-only recommender (5.8): what `record-suggestions` records, and the
//! `suggestions` documents derive builds from them.
//!
//! A file is checked whole before anything is recorded: every entry must name an issue of the
//! launch by its alias, open at its latest observation, with the facts its kind needs. One bad
//! entry refuses the file with every reason listed. Each entry is one `suggested` event, its
//! run id the launch id, so the same file recorded again is a repeat.
//!
//! A document shows the latest suggestion of each kind for each issue until the issue closes,
//! or until the owner dismisses it. A dismissal holds for that issue and kind across later
//! runs, until the issue's observation changes after it (5.8: "unless its facts change").

use std::collections::{BTreeMap, HashSet};

use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::canonical::sha256_hex;
use crate::config::ConfigSet;
use crate::decision::{self, Status};
use crate::docs::{Action, LaunchKind};
use crate::event::Kind;
use crate::launch::{self, Launch, State};
use crate::ledger::{self, Dedupe, Tx};
use crate::root::{format_time, parse_time};
use crate::sync;
use crate::{Error, Result};

/// The backend name a recommender's suggestions carry.
pub const BACKEND: &str = "recommend_closures";

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    #[serde(default)]
    pub suggestions: Vec<Entry>,
    #[serde(default)]
    pub left_out: Vec<LeftOut>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    LikelyFixed,
    Duplicate,
    Obsolete,
    Batch,
}

impl EntryKind {
    fn as_str(self) -> &'static str {
        match self {
            EntryKind::LikelyFixed => "likely_fixed",
            EntryKind::Duplicate => "duplicate",
            EntryKind::Obsolete => "obsolete",
            EntryKind::Batch => "batch",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub kind: EntryKind,
    /// owner/repo#N; every kind but `batch`.
    #[serde(default)]
    pub item: Option<String>,
    /// A batch's members, owner/repo#N each.
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub surface: Option<String>,
    /// The other issue of a duplicate, owner/repo#N.
    #[serde(default)]
    pub related: Option<String>,
    /// The fixing commit of `likely_fixed`.
    #[serde(default)]
    pub commit: Option<String>,
    pub confidence: f64,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Why {
    NeedsDesign,
    Blocked,
    KeepOpen,
}

impl Why {
    fn as_str(self) -> &'static str {
        match self {
            Why::NeedsDesign => "needs_design",
            Why::Blocked => "blocked",
            Why::KeepOpen => "keep_open",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeftOut {
    pub item: String,
    pub why: Why,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Report {
    pub launch: String,
    pub recorded: usize,
    pub repeats: usize,
}

/// One event to record: its subject (the item, or a batch's first member), its kind, and the
/// suggestion body.
struct Pending {
    subject: String,
    decision: &'static str,
    body: Value,
}

/// Collects every reason a file is refused.
struct Check<'a> {
    conn: &'a Connection,
    launch: &'a Launch,
    errors: Vec<String>,
}

impl Check<'_> {
    fn fail(&mut self, e: impl Into<String>) {
        self.errors.push(e.into());
    }

    /// The node id of `alias` among the launch's items, open at its latest observation.
    fn open_item(&mut self, alias: &str) -> Result<Option<(String, String)>> {
        let l = self.launch;
        let Some(pos) = l.request.aliases.iter().position(|a| a.eq_ignore_ascii_case(alias)) else {
            self.fail(format!("{alias} is not in launch {}", l.id));
            return Ok(None);
        };
        let Some(id) = l.request.items.get(pos).cloned() else {
            self.fail(format!("launch {} names {alias} without its item", l.id));
            return Ok(None);
        };
        match ledger::latest(self.conn, &id, Kind::Observed)? {
            None => {
                self.fail(format!("{alias} has never been read"));
                Ok(None)
            }
            Some(e) if e.data["content"]["state"] != "open" => {
                self.fail(format!("{alias} is already closed"));
                Ok(None)
            }
            Some(e) => {
                let current = e.data["alias"].as_str().unwrap_or(alias).to_string();
                Ok(Some((id, current)))
            }
        }
    }

    fn common(&mut self, what: &str, confidence: f64, reason: &str) {
        if !(0.0..=1.0).contains(&confidence) {
            self.fail(format!("{what}: confidence {confidence} is not between 0 and 1"));
        }
        if reason.trim().is_empty() {
            self.fail(format!("{what} needs a reason"));
        }
    }
}

fn is_commit(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

fn probabilities(confidence: f64) -> Value {
    json!({"true": confidence, "false": 1.0 - confidence})
}

/// Records the suggestions of `input` for recommender launch `launch_id`, or refuses the whole
/// file with every reason.
pub fn record(tx: &mut Tx<'_>, config: &ConfigSet, launch_id: &str, input: &Input) -> Result<Report> {
    let l = launch::load(tx.conn(), launch_id)?.ok_or_else(|| Error::refused(format!("no launch {launch_id}")))?;
    if l.request.kind != LaunchKind::RecommendClosures {
        return Err(Error::refused(format!(
            "launch {launch_id} is not a recommend_closures launch; only a recommender records suggestions"
        )));
    }
    if matches!(l.state, State::Requested | State::Queued | State::Withdrawn) {
        return Err(Error::refused(format!(
            "launch {launch_id} is {}; only a launch that holds or held a slot records suggestions",
            l.state
        )));
    }
    let project = l.request.project.clone();
    let mut c = Check {
        conn: tx.conn(),
        launch: &l,
        errors: Vec::new(),
    };
    let mut pending: Vec<Pending> = Vec::new();
    let mut seen: HashSet<(String, &'static str)> = HashSet::new();
    let mut batched: HashSet<String> = HashSet::new();

    for e in &input.suggestions {
        let kind = e.kind.as_str();
        if e.kind == EntryKind::Batch {
            let what = format!("the batch of {}", e.members.join(", "));
            c.common(&what, e.confidence, &e.reason);
            if e.members.len() < 2 {
                c.fail(format!("{what}: a batch needs at least two members"));
            }
            if e.surface.as_deref().is_none_or(|s| s.trim().is_empty()) {
                c.fail(format!("{what}: a batch needs its surface, the code its members share"));
            }
            let mut members = Vec::new();
            for m in &e.members {
                if let Some(found) = c.open_item(m)? {
                    if !batched.insert(found.0.clone()) {
                        c.fail(format!("{m} is in two batches"));
                    }
                    members.push(found);
                }
            }
            // By the aliases the batch names (the launch's): an item keeps the name it was last
            // read under, so after a repo rename its members' observed names may differ.
            let repo = |a: &str| a.rsplit_once('#').map(|(r, _)| r.to_ascii_lowercase());
            if e.members.windows(2).any(|w| repo(&w[0]) != repo(&w[1])) {
                c.fail(format!("{what}: a batch's members are in one repo"));
            }
            if members.len() == e.members.len() && members.len() >= 2 {
                // The subject (what a dismissal is keyed by) is the same whatever order the
                // members are listed in; the card still leads with the first.
                let subject = members.iter().map(|m| m.0.clone()).min().expect("two or more members");
                pending.push(Pending {
                    subject,
                    decision: kind,
                    body: json!({
                        "item": members[0].0,
                        "alias": members[0].1,
                        "decision": kind,
                        "answer": true,
                        "probabilities": probabilities(e.confidence),
                        "backend": BACKEND,
                        "evaluated": false,
                        "reason": e.reason,
                        "members": members.iter().map(|m| &m.0).collect::<Vec<_>>(),
                        "member_aliases": members.iter().map(|m| &m.1).collect::<Vec<_>>(),
                        "surface": e.surface,
                    }),
                });
            }
            continue;
        }
        let Some(alias) = e.item.as_deref() else {
            c.fail(format!("a {kind} entry needs its item"));
            continue;
        };
        let what = format!("{alias} ({kind})");
        c.common(&what, e.confidence, &e.reason);
        let found = c.open_item(alias)?;
        let mut body = json!({
            "decision": kind,
            "answer": true,
            "probabilities": probabilities(e.confidence),
            "backend": BACKEND,
            "evaluated": false,
            "reason": e.reason,
        });
        match e.kind {
            EntryKind::LikelyFixed => match e.commit.as_deref() {
                None => c.fail(format!("{alias}: likely_fixed needs the fixing commit")),
                Some(sha) if !is_commit(sha) => c.fail(format!("{alias}: {sha} is not a commit hash")),
                Some(sha) => body["commit"] = json!(sha),
            },
            EntryKind::Duplicate => match e.related.as_deref() {
                None => c.fail(format!("{alias}: duplicate needs the other issue (related)")),
                Some(other) => match sync::tracked(tx.conn(), config, &project, other) {
                    Err(Error::Refused(m)) => c.fail(m),
                    Err(err) => return Err(err),
                    Ok(other_id) => {
                        if found.as_ref().is_some_and(|f| f.0 == other_id) {
                            c.fail(format!("{alias} cannot be a duplicate of itself"));
                        }
                        body["related"] = json!(other_id);
                        body["related_alias"] = json!(alias_of(tx.conn(), &other_id)?.unwrap_or(other.to_string()));
                    }
                },
            },
            EntryKind::Obsolete | EntryKind::Batch => {}
        }
        if let Some((id, current)) = found {
            if !seen.insert((id.clone(), kind)) {
                c.fail(format!("{alias} has two {kind} entries"));
            }
            body["item"] = json!(id);
            body["alias"] = json!(current);
            pending.push(Pending {
                subject: id,
                decision: kind,
                body,
            });
        }
    }
    for o in &input.left_out {
        let what = format!("{} (left_out)", o.item);
        c.common(&what, 1.0, &o.reason);
        if let Some((id, current)) = c.open_item(&o.item)? {
            if !seen.insert((id.clone(), "left_out")) {
                c.fail(format!("{} is left out twice", o.item));
            }
            pending.push(Pending {
                subject: id.clone(),
                decision: "left_out",
                body: json!({
                    "item": id,
                    "alias": current,
                    "decision": "left_out",
                    "answer": o.why.as_str(),
                    "probabilities": {o.why.as_str(): 1.0},
                    "backend": BACKEND,
                    "evaluated": false,
                    "reason": o.reason,
                    "why": o.why.as_str(),
                }),
            });
        }
    }
    if c.errors.is_empty() && pending.is_empty() {
        c.fail("the file has no suggestions and nothing left out");
    }
    if !c.errors.is_empty() {
        return Err(Error::refused(format!(
            "record-suggestions refused for launch {launch_id}; nothing was recorded:\n- {}",
            c.errors.join("\n- ")
        )));
    }

    let mut report = Report {
        launch: launch_id.to_string(),
        recorded: 0,
        repeats: 0,
    };
    for p in pending {
        let data = json!({"project": project, "run_id": launch_id, "suggestion": p.body});
        let r = tx.record_valid(
            Kind::Suggested,
            &p.subject,
            json!({"run_id": launch_id, "decision": p.decision}),
            data.clone(),
            Dedupe::Any,
        )?;
        if r.is_new() {
            report.recorded += 1;
        } else if r.event().data == data {
            report.repeats += 1;
        } else {
            return Err(Error::refused(format!(
                "launch {launch_id} already recorded a different {} suggestion for {} (seq {}); a run records each once",
                p.decision,
                data["suggestion"]["alias"].as_str().unwrap_or(&p.subject),
                r.event().seq
            )));
        }
    }
    Ok(report)
}

fn alias_of(conn: &Connection, id: &str) -> Result<Option<String>> {
    Ok(ledger::latest(conn, id, Kind::Observed)?.and_then(|e| e.data["alias"].as_str().map(str::to_string)))
}

/// The document id of the suggestion of `decision` about `subject` in run `run`.
fn doc_id(run: &str, decision: &str, subject: &str) -> String {
    format!("sg-{}", &sha256_hex(&format!("{run}|{decision}|{subject}"))[..24])
}

fn action_label(decision: &str) -> Option<&'static str> {
    match decision {
        "likely_fixed" => Some("Check and close…"),
        "duplicate" => Some("Close as duplicate…"),
        "obsolete" => Some("Close as not planned…"),
        "batch" => Some("Launch batch…"),
        _ => None,
    }
}

/// The latest observation's seq and whether it shows the item open.
fn observed(conn: &Connection, id: &str) -> Result<Option<(i64, bool, Option<String>)>> {
    Ok(ledger::latest(conn, id, Kind::Observed)?.map(|e| {
        (
            e.seq,
            e.data["content"]["state"] == "open",
            e.data["alias"].as_str().map(str::to_string),
        )
    }))
}

/// The `suggestions` documents of `project` (5.8).
pub fn documents(conn: &Connection, project: &str) -> Result<Vec<Value>> {
    let events: Vec<_> = ledger::events_by_kind(conn, Kind::Suggested)?
        .into_iter()
        .filter(|e| e.data["project"] == project)
        .collect();
    // Every document id ever shown, to what it was about; and the latest of each (item, kind).
    let mut about: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut latest: BTreeMap<(String, String), &crate::event::Event> = BTreeMap::new();
    for e in &events {
        let decision = e.distinct["decision"].as_str().unwrap_or_default().to_string();
        let run = e.distinct["run_id"].as_str().unwrap_or_default();
        about.insert(
            doc_id(run, &decision, &e.subject),
            (e.subject.clone(), decision.clone()),
        );
        latest.insert((e.subject.clone(), decision), e);
    }
    // The latest applied dismissal of each (item, kind), by the seq it was decided at.
    let mut dismissed: BTreeMap<(String, String), i64> = BTreeMap::new();
    for (seq, d, status) in decision::resolved(conn, project)? {
        if d.action != Action::DismissSuggestion || status != Status::Applied {
            continue;
        }
        if let Some(key) = d.target.suggestion.as_ref().and_then(|s| about.get(s)) {
            let at = dismissed.entry(key.clone()).or_insert(seq);
            *at = (*at).max(seq);
        }
    }

    let mut out = Vec::new();
    for ((subject, decision), e) in latest {
        let Some((mut seen_at, open, _)) = observed(conn, &subject)? else {
            continue;
        };
        if decision == "batch" {
            // A dismissed batch returns when any of its members changes.
            for m in e.data["suggestion"]["members"].as_array().into_iter().flatten() {
                if let Some((seq, _, _)) = observed(conn, m.as_str().unwrap_or_default())? {
                    seen_at = seen_at.max(seq);
                }
            }
        }
        if dismissed
            .get(&(subject.clone(), decision.clone()))
            .is_some_and(|d| seen_at < *d)
        {
            continue;
        }
        let mut s = e.data["suggestion"].clone();
        if decision == "batch" {
            // A closed member leaves the batch; below two members there is no batch.
            let mut kept = Vec::new();
            let members = s["members"].as_array().cloned().unwrap_or_default();
            let aliases = s["member_aliases"].as_array().cloned().unwrap_or_default();
            for (m, a) in members.iter().zip(aliases) {
                let id = m.as_str().unwrap_or_default();
                if let Some((_, true, current)) = observed(conn, id)? {
                    kept.push((id.to_string(), current.map_or(a, Value::String)));
                }
            }
            if kept.len() < 2 {
                continue;
            }
            s["item"] = json!(kept[0].0);
            s["alias"] = kept[0].1.clone();
            s["members"] = json!(kept.iter().map(|k| &k.0).collect::<Vec<_>>());
            s["member_aliases"] = json!(kept.iter().map(|k| &k.1).collect::<Vec<_>>());
        } else if !open {
            continue;
        }
        let run = e.distinct["run_id"].as_str().unwrap_or_default();
        let confidence = s["probabilities"]["true"].as_f64().unwrap_or(1.0);
        let Value::Object(mut doc) = s else { continue };
        doc.insert("id".into(), json!(doc_id(run, &decision, &subject)));
        doc.insert("project".into(), json!(project));
        doc.insert("run_id".into(), json!(run));
        doc.insert("kind".into(), json!(decision));
        doc.insert("made_at".into(), json!(format_time(parse_time(&e.time)?)));
        doc.insert("confidence".into(), json!(confidence));
        if let Some(label) = action_label(&decision) {
            doc.insert("action_label".into(), json!(label));
        }
        out.push(Value::Object(doc));
    }
    out.sort_by(|a, b| (a["made_at"].as_str(), a["id"].as_str()).cmp(&(b["made_at"].as_str(), b["id"].as_str())));
    Ok(out)
}
