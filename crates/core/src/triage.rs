//! The triage seam (triage spec ch. 1-2, phase 1/5): the question files of `decisions/`, the
//! question hash, the blinded inputs, `prepare` and its `prepared` event, and the calibration
//! applied when answers are recorded (`suggest::record`).
//!
//! A run prepares its questions in two stages (2.1): every decision but priority first, then
//! priority from the first stage's recorded answers. A question is asked about an item when
//! it was never answered under the decision's current question hash, or the item was observed
//! again since it was last answered (2.2); a closure also after `closure_recheck_days`. Only
//! a RECORDED answer counts as asked, so an abandoned run leaves its items eligible.
//!
//! **Blinding** (1.1): no input carries a label of any label decision (type, area, priority),
//! and a replay input holds only the comments before one cutoff per item, the earliest time
//! one of those labels was set. Phase 1 reads no comment text (`read.rs` keeps each comment's
//! author and time only), so a live input carries the title and body; the builder takes
//! comment bodies when its source has them.
//!
//! **Calibration** (2.4): a map is fitted by `eval` (`eval.rs`). Until a key has one, the map in
//! force is none, `map_id` is null, and the calibrated probabilities are the raw ones.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::OnceLock;

use chrono::Duration;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::canonical::{hash_value, sha256_hex};
use crate::config::{ConfigSet, Labels};
use crate::derive::INSIDE;
use crate::docs::{LaunchKind, Priority, Routing, TriageScope};
use crate::event::Kind;
use crate::launch::{self, Launch, State};
use crate::ledger::{self, Dedupe, Tx};
use crate::root::parse_time;
use crate::translate::Translation;
use crate::{Error, Result};

macro_rules! decisions {
    ($($stem:literal),* $(,)?) => {
        &[$(($stem, include_str!(concat!("../../../decisions/", $stem, ".toml")))),*]
    };
}

/// Every question file, by decision. A test checks this list against the files in `decisions/`.
pub const DECISION_FILES: &[(&str, &str)] = decisions![
    "area",
    "detectability",
    "duplicate",
    "instructs_agent",
    "likely_fixed",
    "obsolete",
    "priority",
    "reach",
    "severity",
    "support_question",
    "type",
];

/// The decisions whose answers are labels: blinded from every input (1.1).
pub const LABEL_DECISIONS: [&str; 3] = ["type", "area", "priority"];

/// The decisions an answer of which may need a stronger look (2.5): its calibrated top
/// probability below `stronger_look_below`.
pub const STRONGER_LOOK_DECISIONS: [&str; 4] = ["priority", "type", "duplicate", "likely_fixed"];

/// How long a closure answer holds before it is asked again (2.2), unless the project says.
pub const CLOSURE_RECHECK_DAYS: u32 = 14;

/// The stated probabilities are clipped to this range (2.3).
pub const CLIP: (f64, f64) = (0.01, 0.99);

/// How far from 1 a single-choice answer's probabilities may sum before it is refused (1.4).
pub const SUM_TOLERANCE: f64 = 0.02;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AnswerKind {
    /// One of `answers`, with a probability for each.
    Choice,
    /// true or false, with a probability for each.
    Boolean,
    /// A set of the repo's areas, with a yes/no probability for each area.
    Areas,
}

/// Which launches ask the question (1.1): `full` triage only, or `closures` too.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Scope {
    Full,
    Both,
}

/// What a closure answer names (1.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Subject {
    Commit,
    Related,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RubricLevel {
    pub at_least: u32,
    pub level: Priority,
}

/// The rubric table (1.5), from `decisions/priority.toml`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Rubric {
    pub severity: BTreeMap<String, u32>,
    pub reach: BTreeMap<String, u32>,
    /// Highest first: the first whose `at_least` the points reach.
    pub levels: Vec<RubricLevel>,
    pub otherwise: Priority,
    pub silent_raises: u32,
}

impl Rubric {
    /// The level the table gives `points`, before the silent bump.
    pub fn level_for_points(&self, points: u32) -> Priority {
        self.levels
            .iter()
            .find(|l| points >= l.at_least)
            .map_or(self.otherwise, |l| l.level)
    }

    /// `rubric_level` (1.5): the table applied to the top answers; `None` for an answer the
    /// table does not know.
    pub fn level(&self, severity: &str, reach: &str, detectability: &str) -> Option<Priority> {
        let points = self.severity.get(severity)? * self.reach.get(reach)?;
        let mut level = self.level_for_points(points);
        if detectability == "silent" {
            for _ in 0..self.silent_raises {
                level = match level {
                    Priority::P3 => Priority::P2,
                    Priority::P2 => Priority::P1,
                    Priority::P1 | Priority::P0 => Priority::P0,
                };
            }
        }
        Some(level)
    }
}

/// One `decisions/<decision>.toml` (1.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct QuestionFile {
    pub decision: String,
    pub kind: AnswerKind,
    pub scope: Scope,
    /// The allowed answers of a `choice`; empty otherwise.
    #[serde(default)]
    pub answers: Vec<String>,
    pub question: String,
    /// The decisions whose recorded answers this one's input holds (priority, 2.1).
    #[serde(default)]
    pub reads: Vec<String>,
    #[serde(default)]
    pub subject: Option<Subject>,
    /// Asked only about an item an outside author wrote.
    #[serde(default)]
    pub outside_only: bool,
    #[serde(default)]
    pub rubric: Option<Rubric>,
}

/// The question files in force, with their text (the hash is over the text).
#[derive(Debug, Clone)]
pub struct Catalogue {
    files: BTreeMap<String, (String, QuestionFile)>,
}

impl Catalogue {
    /// The files compiled into the binary from `decisions/`.
    pub fn embedded() -> &'static Catalogue {
        static CELL: OnceLock<Catalogue> = OnceLock::new();
        CELL.get_or_init(|| Catalogue::parse(DECISION_FILES).unwrap_or_else(|e| panic!("decisions/: {e}")))
    }

    /// Parses question files given as (decision, text); each file must name its own decision,
    /// and a decision a file reads must be one of them.
    pub fn parse(texts: &[(&str, &str)]) -> Result<Catalogue> {
        let mut files = BTreeMap::new();
        for (stem, text) in texts {
            let f: QuestionFile =
                toml::from_str(text).map_err(|e| Error::refused(format!("decisions/{stem}.toml: {e}")))?;
            if f.decision != *stem {
                return Err(Error::refused(format!(
                    "decisions/{stem}.toml names decision {}",
                    f.decision
                )));
            }
            let ok = match f.kind {
                AnswerKind::Choice => f.answers.len() >= 2,
                AnswerKind::Boolean | AnswerKind::Areas => f.answers.is_empty(),
            };
            if !ok {
                return Err(Error::refused(format!(
                    "decisions/{stem}.toml: a choice lists two or more answers, any other kind none"
                )));
            }
            files.insert(stem.to_string(), (text.to_string(), f));
        }
        for (_, f) in files.values() {
            if let Some(missing) = f.reads.iter().find(|r| !files.contains_key(*r)) {
                return Err(Error::refused(format!(
                    "decisions/{}.toml reads {missing}, which has no file",
                    f.decision
                )));
            }
        }
        Ok(Catalogue { files })
    }

    pub fn file(&self, decision: &str) -> Result<&QuestionFile> {
        self.files
            .get(decision)
            .map(|(_, f)| f)
            .ok_or_else(|| Error::refused(format!("no question file for decision {decision}")))
    }

    pub fn decisions(&self) -> impl Iterator<Item = &QuestionFile> {
        self.files.values().map(|(_, f)| f)
    }

    /// The question hash (1.3): the first 12 hex of sha256 over the decision's file, the files
    /// of the decisions it reads, and for `area` the repo's area list.
    pub fn question_hash(&self, decision: &str, areas: &[String]) -> Result<String> {
        let f = self.file(decision)?;
        let mut texts = BTreeMap::new();
        for d in std::iter::once(&f.decision).chain(&f.reads) {
            texts.insert(d.clone(), self.files[d].0.clone());
        }
        let mut basis = json!({"files": texts});
        if f.kind == AnswerKind::Areas {
            let sorted: BTreeSet<&String> = areas.iter().collect();
            basis["areas"] = json!(sorted);
        }
        Ok(hash_value(&basis)[..12].to_string())
    }

    /// The answers a question allows: a choice's, true and false, or the repo's areas.
    pub fn allowed(&self, decision: &str, areas: &[String]) -> Result<Vec<String>> {
        let f = self.file(decision)?;
        Ok(match f.kind {
            AnswerKind::Choice => f.answers.clone(),
            AnswerKind::Boolean => vec!["true".into(), "false".into()],
            AnswerKind::Areas => areas.to_vec(),
        })
    }
}

/// A backend's name (1.3): the routing's model and effort, as `model@effort`.
pub fn backend(r: &Routing) -> String {
    format!("{}@{}", r.model, r.effort)
}

// --- Inputs ---------------------------------------------------------------------------------

/// A comment as an input may hold it. Phase 1 reads no comment text, so `body` is `None` for
/// every live comment.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Comment {
    pub author: String,
    pub inside: bool,
    pub at: String,
    #[serde(default)]
    pub body: Option<String>,
}

/// A label set or removed, with its time: what a replay cutoff is found from (1.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LabelEvent {
    pub label: String,
    pub at: String,
}

/// What an input is built from: an item as observed, or as history holds it (replay).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Source {
    pub id: String,
    pub alias: String,
    pub title: String,
    pub body: Option<String>,
    pub body_truncated: bool,
    pub labels: Vec<String>,
    pub author: String,
    /// The author is inside (an owner, member, collaborator or maintainer).
    pub inside: bool,
    pub comments: Vec<Comment>,
    pub reactions: u64,
}

impl Source {
    /// The source of an observed item's `content`.
    pub fn observed(id: &str, alias: &str, content: &Value, maintainers: &[String]) -> Source {
        let inside =
            |association: &str, author: &str| INSIDE.contains(&association) || maintainers.iter().any(|m| m == author);
        let s = |k: &str| content.get(k).and_then(Value::as_str).unwrap_or_default();
        let author = content.get("author").and_then(Value::as_str).unwrap_or("ghost");
        let association = content
            .get("author_association")
            .and_then(Value::as_str)
            .unwrap_or("NONE");
        Source {
            id: id.to_string(),
            alias: alias.to_string(),
            title: s("title").to_string(),
            body: content.get("body").and_then(Value::as_str).map(str::to_string),
            body_truncated: content.get("body_truncated").and_then(Value::as_bool) == Some(true),
            labels: content
                .get("labels")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|l| l.as_str().map(str::to_string))
                .collect(),
            author: author.to_string(),
            inside: inside(association, author),
            comments: content
                .get("comments")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .map(|c| {
                    let author = c.get("author").and_then(Value::as_str).unwrap_or("ghost");
                    let association = c.get("association").and_then(Value::as_str).unwrap_or("NONE");
                    Comment {
                        author: author.to_string(),
                        inside: inside(association, author),
                        at: c.get("at").and_then(Value::as_str).unwrap_or_default().to_string(),
                        body: c.get("body").and_then(Value::as_str).map(str::to_string),
                    }
                })
                .collect(),
            reactions: content
                .get("reactions")
                .and_then(Value::as_object)
                .into_iter()
                .flatten()
                .filter_map(|(_, n)| n.as_u64())
                .sum(),
        }
    }
}

/// Whether `label` is a label of a label decision (type, area or priority) in `t`, whatever
/// else (a modifier) it also means.
fn is_decision_label(t: &Translation, label: &str) -> bool {
    let hit = |names: &Labels| names.iter().any(|n| n.eq_ignore_ascii_case(label));
    t.types
        .values()
        .chain(t.areas.values())
        .chain(t.priorities.values())
        .any(hit)
}

/// An item's replay cutoff (1.1): the earliest time a label of a label decision was set or
/// removed; `None` when no such label event exists.
pub fn replay_cutoff(t: &Translation, events: &[LabelEvent]) -> Result<Option<String>> {
    let mut earliest: Option<(chrono::DateTime<chrono::Utc>, &str)> = None;
    for e in events.iter().filter(|e| is_decision_label(t, &e.label)) {
        let at = parse_time(&e.at)?;
        if earliest.is_none_or(|(t0, _)| at < t0) {
            earliest = Some((at, &e.at));
        }
    }
    Ok(earliest.map(|(_, s)| s.to_string()))
}

/// The labels an input may show: every label that is not a label decision's.
pub fn blinded_labels(t: &Translation, labels: &[String]) -> Vec<String> {
    labels.iter().filter(|l| !is_decision_label(t, l)).cloned().collect()
}

/// Whether `decision` is asked about `source` at all: an outside-only question needs an
/// outside author, and `area` needs a repo that declares areas.
pub fn applies(f: &QuestionFile, source: &Source, areas: &[String]) -> bool {
    !(f.outside_only && source.inside) && !(f.kind == AnswerKind::Areas && areas.is_empty())
}

/// The blinded input of `decision` about `source` (1.1). `cutoff` is the replay cutoff (no
/// comment at or after it); `extra` is merged in (priority's recorded answers and rubric).
pub fn input(
    cat: &Catalogue,
    decision: &str,
    source: &Source,
    t: &Translation,
    cutoff: Option<&str>,
    extra: Option<Value>,
) -> Result<Value> {
    let f = cat.file(decision)?;
    let cutoff = cutoff.map(parse_time).transpose()?;
    // Inside comments for every question but instructs_agent, which reads outside text only.
    let want_inside = decision != "instructs_agent";
    let mut comments = Vec::new();
    for c in &source.comments {
        let Some(body) = &c.body else { continue };
        if c.inside != want_inside {
            continue;
        }
        if let Some(cut) = cutoff
            && parse_time(&c.at)? >= cut
        {
            continue;
        }
        comments.push(json!({"author": c.author, "at": c.at, "body": body}));
    }
    let mut v = json!({
        "alias": source.alias,
        "title": source.title,
        "body": source.body,
        "body_truncated": source.body_truncated,
        "labels": blinded_labels(t, &source.labels),
        "comments": comments,
    });
    if decision == "instructs_agent" && source.inside {
        // An inside author's title and body are not outside text.
        v["title"] = Value::Null;
        v["body"] = Value::Null;
    }
    match f.kind {
        AnswerKind::Areas => v["areas"] = json!(t.areas.keys().collect::<Vec<_>>()),
        AnswerKind::Choice | AnswerKind::Boolean => {}
    }
    if decision == "reach" {
        // Counted under the same cutoff as the comments shown; reactions carry no time.
        let mut outside: BTreeSet<&str> = BTreeSet::new();
        for c in source
            .comments
            .iter()
            .filter(|c| !c.inside && c.author != source.author)
        {
            if let Some(cut) = cutoff
                && parse_time(&c.at)? >= cut
            {
                continue;
            }
            outside.insert(c.author.as_str());
        }
        v["outside_demand"] = json!({
            "outside_commenters": outside.len(),
            "reactions": source.reactions,
        });
    }
    if let Some(Value::Object(extra)) = extra {
        for (k, x) in extra {
            v[k] = x;
        }
    }
    Ok(v)
}

/// The hash an input is compared by at record (2.1).
pub fn input_hash(input: &Value) -> String {
    hash_value(input)
}

// --- Calibration ----------------------------------------------------------------------------

/// The map in force for (project, backend, decision, question hash) (2.4): the one its latest
/// `evaluated` event names, passing or not; `None` with no evaluation, or when the latest fit
/// was at the grid edge (`uncalibratable`).
pub fn map_in_force(
    conn: &Connection,
    project: &str,
    backend: &str,
    decision: &str,
    hash: &str,
) -> Result<Option<String>> {
    crate::eval::map_in_force(conn, project, backend, decision, hash)
}

/// The calibrated probabilities of `raw` under map `map_id` (2.4): softmax(log p / T) over the
/// answers (per area as yes/no); the identity without a map.
pub fn calibrate(
    conn: &Connection,
    kind: AnswerKind,
    raw: &BTreeMap<String, f64>,
    map_id: Option<&str>,
) -> Result<BTreeMap<String, f64>> {
    match map_id {
        None => Ok(raw.clone()),
        Some(m) => Ok(crate::eval::apply_t(kind, raw, crate::eval::map_t(conn, m)?)),
    }
}

// --- Prepare --------------------------------------------------------------------------------

/// The two stages of a run (2.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Stage {
    /// Every decision of the launch's scope but priority.
    First,
    /// Priority, from the first stage's recorded answers.
    Priority,
}

/// One question as a `prepared` event freezes it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: String,
    pub item: String,
    pub alias: String,
    pub decision: String,
    pub question_hash: String,
    pub input_hash: String,
    /// The seq of the observation the input was built from.
    pub observed: i64,
    pub map_id: Option<String>,
}

/// The `data` of a `prepared` event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedData {
    pub project: String,
    pub launch: String,
    pub backend: String,
    pub stage: Stage,
    pub questions: Vec<Question>,
}

/// What [`prepare`] wrote: the questions file for the session, and a report.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Prepared {
    pub file: Value,
    pub report: PrepareReport,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PrepareReport {
    pub launch: String,
    pub backend: String,
    pub stage: Stage,
    pub questions: usize,
    /// The `prepared` event's seq; `None` when nothing was eligible (nothing is recorded).
    pub seq: Option<i64>,
    pub repeat: bool,
    /// Items asked nothing, each with why.
    pub skipped: Vec<Value>,
}

/// The scope a launch asks in (5.1): a triage launch's own scope (`full` when it names none),
/// and `closures` for the phase-1 alias `recommend_closures`. A `stronger_look` launch (2.5)
/// may re-ask any decision of [`STRONGER_LOOK_DECISIONS`], priority among them: `full`.
pub(crate) fn launch_scope(l: &Launch) -> Result<Scope> {
    if l.request.kind == LaunchKind::StrongerLook {
        return Ok(Scope::Full);
    }
    match l.request.triage_scope() {
        Some(TriageScope::Full) => Ok(Scope::Full),
        Some(TriageScope::Closures) => Ok(Scope::Both),
        None => Err(Error::refused(format!(
            "launch {} is a {} launch; only triage, recommend_closures and stronger_look launches answer questions",
            l.id,
            serde_json::to_value(l.request.kind)?.as_str().unwrap_or_default()
        ))),
    }
}

/// The launch `launch_id`, refused unless it asks questions and holds or held a slot.
pub(crate) fn answering_launch(conn: &Connection, launch_id: &str) -> Result<(Launch, Scope)> {
    let l = launch::load(conn, launch_id)?.ok_or_else(|| Error::refused(format!("no launch {launch_id}")))?;
    let scope = launch_scope(&l)?;
    if matches!(l.state, State::Requested | State::Queued | State::Withdrawn) {
        return Err(Error::refused(format!(
            "launch {launch_id} is {}; only a launch that holds or held a slot answers questions",
            l.state
        )));
    }
    Ok((l, scope))
}

/// Every question any `prepared` event froze, by id.
pub(crate) fn prepared_questions(conn: &Connection) -> Result<BTreeMap<String, (Question, String)>> {
    let mut out = BTreeMap::new();
    for e in ledger::events_by_kind(conn, Kind::Prepared)? {
        let d: PreparedData = serde_json::from_value(e.data)?;
        for q in d.questions {
            out.insert(q.id.clone(), (q, d.launch.clone()));
        }
    }
    Ok(out)
}

/// When an (item, decision) was last answered: the question it answered and the time.
#[derive(Debug, Clone)]
pub(crate) struct Asked {
    pub question: Question,
    pub time: String,
    pub suggestion: Value,
}

/// The latest recorded answer to a prepared question, per (item, decision).
pub(crate) fn asked(conn: &Connection) -> Result<BTreeMap<(String, String), Asked>> {
    let questions = prepared_questions(conn)?;
    let mut out = BTreeMap::new();
    for e in ledger::events_by_kind(conn, Kind::Suggested)? {
        let s = &e.data["suggestion"];
        let Some((q, _)) = s["question"].as_str().and_then(|id| questions.get(id)) else {
            continue;
        };
        out.insert(
            (q.item.clone(), q.decision.clone()),
            Asked {
                question: q.clone(),
                time: e.time.clone(),
                suggestion: s.clone(),
            },
        );
    }
    Ok(out)
}

/// An item as the latest observation shows it, ready to build inputs from.
pub(crate) struct Observed {
    pub seq: i64,
    pub open: bool,
    pub issue: bool,
    pub repo: String,
    pub source: Source,
}

pub(crate) fn observed(conn: &Connection, config: &ConfigSet, project: &str, id: &str) -> Result<Option<Observed>> {
    let Some(e) = ledger::latest(conn, id, Kind::Observed)? else {
        return Ok(None);
    };
    let alias = e.data["alias"].as_str().unwrap_or(id).to_string();
    let repo = alias.rsplit_once('#').map_or(alias.as_str(), |(r, _)| r).to_string();
    let maintainers = config.project(project)?.maintainers.clone().unwrap_or_default();
    let content = &e.data["content"];
    Ok(Some(Observed {
        seq: e.seq,
        open: content["state"] == "open",
        issue: e.data["item"] == "issue",
        source: Source::observed(id, &alias, content, &maintainers),
        repo,
    }))
}

pub(crate) fn translation_of(config: &ConfigSet, repo: &str) -> Translation {
    // The registry's key spelling, so a repo read under another case still finds its entry.
    let key = config
        .registry
        .repo
        .keys()
        .find(|k| k.eq_ignore_ascii_case(repo))
        .cloned()
        .unwrap_or_else(|| repo.to_string());
    Translation::for_repo(config, &key)
}

/// The areas a repo's area question lists.
pub(crate) fn areas_of(t: &Translation) -> Vec<String> {
    t.areas.keys().cloned().collect()
}

/// The first stage's recorded answers priority reads, with the rubric (2.1 stage 2); `None`
/// while one of them has no answer under its current question hash about observation `seq`
/// (an answer about older facts is no answer: stage 1 is due again).
fn stage_one(
    cat: &Catalogue,
    asked: &BTreeMap<(String, String), Asked>,
    item: &str,
    seq: i64,
) -> Result<std::result::Result<Value, Vec<String>>> {
    let f = cat.file("priority")?;
    let mut recorded = BTreeMap::new();
    let mut missing = Vec::new();
    for d in &f.reads {
        let current = cat.question_hash(d, &[])?;
        match asked.get(&(item.to_string(), d.clone())) {
            Some(a) if a.question.question_hash == current && a.question.observed >= seq => {
                recorded.insert(
                    d.clone(),
                    (a.suggestion["answer"].clone(), a.suggestion["probabilities"].clone()),
                );
            }
            _ => missing.push(d.clone()),
        }
    }
    if !missing.is_empty() {
        return Ok(Err(missing));
    }
    Ok(Ok(priority_extra(cat, &recorded)?))
}

/// What priority's input holds beyond the issue (2.1 stage 2): the first stage's answers as
/// recorded (answer and calibrated probabilities, by decision), the rubric and `rubric_level`.
pub fn priority_extra(cat: &Catalogue, recorded: &BTreeMap<String, (Value, Value)>) -> Result<Value> {
    let f = cat.file("priority")?;
    let rubric = f
        .rubric
        .as_ref()
        .ok_or_else(|| Error::refused("decisions/priority.toml has no rubric"))?;
    let top = |d: &str| recorded.get(d).and_then(|(a, _)| a.as_str()).unwrap_or_default();
    let level = rubric.level(top("severity"), top("reach"), top("detectability"));
    let answers: serde_json::Map<String, Value> = recorded
        .iter()
        .map(|(d, (a, p))| (d.clone(), json!({"answer": a, "probabilities": p})))
        .collect();
    Ok(json!({"recorded": answers, "rubric": rubric, "rubric_level": level}))
}

/// The input of `decision` about an observed item, as `prepare` builds it and as `record`
/// rebuilds it to see whether the item changed; `Err` names why it cannot be built.
pub(crate) fn live_input(
    config: &ConfigSet,
    cat: &Catalogue,
    asked: &BTreeMap<(String, String), Asked>,
    o: &Observed,
    decision: &str,
) -> Result<std::result::Result<(Value, String), String>> {
    let t = translation_of(config, &o.repo);
    let extra = if decision == "priority" {
        match stage_one(cat, asked, &o.source.id, o.seq)? {
            Ok(v) => Some(v),
            Err(missing) => {
                return Ok(Err(format!("stage 1 has no recorded {} answer", missing.join(", "))));
            }
        }
    } else {
        None
    };
    let v = input(cat, decision, &o.source, &t, None, extra)?;
    let h = input_hash(&v);
    Ok(Ok((v, h)))
}

/// The id of a question: the launch, item, decision, question hash, input hash and the
/// observation it was built from (a read that leaves the input alone still makes a new
/// question, so the observation an answer is counted against is never an older one).
fn question_id(launch: &str, item: &str, decision: &str, qhash: &str, ihash: &str, seq: i64) -> String {
    format!(
        "q-{}",
        &sha256_hex(&format!("{launch}|{item}|{decision}|{qhash}|{ihash}|{seq}"))[..20]
    )
}

/// Whether (item, decision) is due (2.2): never answered under `qhash`, observed again since
/// its last answer, asked for by the owner after it (`owner_asked`, the time of the owner's
/// latest Triage… request of the item), or a closure answered more than `recheck` ago.
#[allow(clippy::too_many_arguments)]
fn due(
    asked: Option<&Asked>,
    qhash: &str,
    seq: i64,
    closure: bool,
    recheck: Duration,
    owner_asked: Option<&str>,
    now: &str,
) -> Result<bool> {
    let Some(a) = asked else { return Ok(true) };
    if a.question.question_hash != qhash || seq > a.question.observed {
        return Ok(true);
    }
    if let Some(t) = owner_asked
        && parse_time(t)? > parse_time(&a.time)?
    {
        return Ok(true);
    }
    Ok(closure && parse_time(now)? - parse_time(&a.time)? >= recheck)
}

/// Whether a recorded suggestion needs a stronger look (2.5): `record` marks it so.
pub(crate) fn needs_stronger_look(suggestion: &Value) -> bool {
    suggestion["needs_stronger_look"] == true
}

/// Whether `f` is a closure decision: re-asked after `closure_recheck_days` (2.2).
fn is_closure(f: &QuestionFile) -> bool {
    f.scope == Scope::Both && f.kind == AnswerKind::Boolean && f.decision != "instructs_agent"
}

/// The time of the owner's latest Triage… request (the page's `requests` document of kind
/// `triage`, 2.2) of each item of `project`.
pub(crate) fn owner_asked(conn: &Connection, project: &str) -> Result<BTreeMap<String, String>> {
    let mut out = BTreeMap::new();
    for e in ledger::events_by_kind(conn, Kind::Request)? {
        if e.data["project"] != project || e.data["document"]["kind"] != "triage" {
            continue;
        }
        for item in e.data["document"]["items"].as_array().into_iter().flatten() {
            if let Some(id) = item.as_str() {
                out.insert(id.to_string(), e.time.clone());
            }
        }
    }
    Ok(out)
}

/// Every issue `project` tracks in a repo it may write to: a read-only member's issues
/// (declared, or observed without write access, as the dashboard reads it) are someone else's
/// to label (1.4).
fn project_items(conn: &Connection, config: &ConfigSet, project: &str) -> Result<BTreeSet<String>> {
    let mut out = BTreeSet::new();
    for view in config.views(project)?.into_values() {
        if crate::derive::read_only(conn, project, &view)?.is_some() {
            continue;
        }
        let node = match crate::snapshot::read_state(conn, project, &view.repo)?.and_then(|s| s.repo_node) {
            Some(n) => Some(n),
            None => crate::snapshot::repo_node_of(conn, &view.repo)?,
        };
        let Some(node) = node else { continue };
        if let Some((_, members)) = crate::snapshot::membership(conn, project, &node)? {
            out.extend(members.issues.iter().cloned());
        }
    }
    Ok(out)
}

/// One item a triage launch would ask about (2.2).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Candidate {
    pub item: String,
    pub alias: String,
    /// Untriaged on the page: no effective type or priority, or no area where the repo
    /// declares areas. These go first.
    pub untriaged: bool,
    /// When the item was last answered, any decision; `None` if never.
    pub last_triaged: Option<String>,
}

/// The items a triage launch of `scope` would ask about now (2.2), in order: untriaged items
/// first, then the oldest last triage; uncapped (the caller caps at `max_triage_items`). An
/// item is selected when any question of the scope is due about it, by the rule `prepare`
/// applies to each (item, decision).
pub fn select(
    conn: &Connection,
    config: &ConfigSet,
    project: &str,
    scope: TriageScope,
    now: &str,
) -> Result<Vec<Candidate>> {
    select_with(conn, config, Catalogue::embedded(), project, scope, now)
}

pub(crate) fn select_with(
    conn: &Connection,
    config: &ConfigSet,
    cat: &Catalogue,
    project: &str,
    scope: TriageScope,
    now: &str,
) -> Result<Vec<Candidate>> {
    let scope = match scope {
        TriageScope::Full => Scope::Full,
        TriageScope::Closures => Scope::Both,
    };
    let recheck = Duration::days(i64::from(
        config
            .project(project)?
            .closure_recheck_days
            .unwrap_or(CLOSURE_RECHECK_DAYS),
    ));
    let asked = asked(conn)?;
    let by_owner = owner_asked(conn, project)?;
    let layers = crate::layers::layers(conn, project)?;
    let mut out = Vec::new();
    for id in project_items(conn, config, project)? {
        let Some(o) = observed(conn, config, project, &id)? else {
            continue;
        };
        if !o.issue || !o.open {
            continue;
        }
        let t = translation_of(config, &o.repo);
        let areas = areas_of(&t);
        let mut any = false;
        let mut last: Option<String> = None;
        for f in cat.decisions() {
            let a = asked.get(&(id.clone(), f.decision.clone()));
            if let Some(a) = a
                && last.as_ref().is_none_or(|l| &a.time > l)
            {
                last = Some(a.time.clone());
            }
            if any || !(scope == Scope::Full || f.scope == Scope::Both) || !applies(f, &o.source, &areas) {
                continue;
            }
            let qhash = cat.question_hash(&f.decision, &areas)?;
            any = due(
                a,
                &qhash,
                o.seq,
                is_closure(f),
                recheck,
                by_owner.get(&id).map(String::as_str),
                now,
            )?;
        }
        if !any {
            continue;
        }
        let untriaged = crate::layers::effective_of(conn, config, &layers, &id)?
            .is_some_and(|e| e.type_.is_none() || e.priority.is_none() || (t.declares_areas() && e.areas.is_empty()));
        out.push(Candidate {
            item: id,
            alias: o.source.alias.clone(),
            untriaged,
            last_triaged: last,
        });
    }
    // Untriaged first; then never triaged before the oldest last triage; then by alias.
    out.sort_by(|a, b| {
        b.untriaged
            .cmp(&a.untriaged)
            .then_with(|| a.last_triaged.is_some().cmp(&b.last_triaged.is_some()))
            .then_with(|| a.last_triaged.cmp(&b.last_triaged))
            .then_with(|| a.alias.cmp(&b.alias))
    });
    Ok(out)
}

/// `prepare` (2.1 step 2): checks the session's model and effort against the launch's
/// routing, builds the blinded input of every due (item, decision) of the stage, and records
/// a `prepared` event freezing each question's hashes, observation and map.
pub fn prepare(
    tx: &mut Tx<'_>,
    config: &ConfigSet,
    launch_id: &str,
    session_model: &str,
    stage: Stage,
) -> Result<Prepared> {
    prepare_with(tx, config, Catalogue::embedded(), launch_id, session_model, stage)
}

/// [`prepare`] with the question files `cat` in place of the embedded ones: what a reworded
/// file does to what is due.
pub fn prepare_with(
    tx: &mut Tx<'_>,
    config: &ConfigSet,
    cat: &Catalogue,
    launch_id: &str,
    session_model: &str,
    stage: Stage,
) -> Result<Prepared> {
    let (l, scope) = answering_launch(tx.conn(), launch_id)?;
    let backend = backend(&l.request.routing);
    if session_model != backend {
        return Err(Error::refused(format!(
            "this session runs {session_model}, but launch {launch_id} is routed to {backend}; \
             its answers would be measured as another backend's, so nothing is prepared"
        )));
    }
    // A stronger look (2.5) re-asks the answers that need one, in one stage: a priority it
    // re-asks reads the first stage's answers already recorded.
    let stronger = l.request.kind == LaunchKind::StrongerLook;
    if stage == Stage::Priority && stronger {
        return Err(Error::refused(format!(
            "launch {launch_id} is a stronger look; it asks every question in one stage, priority included"
        )));
    }
    if stage == Stage::Priority && scope != Scope::Full {
        return Err(Error::refused(format!(
            "launch {launch_id} asks closures only; it has no priority stage"
        )));
    }
    let project = l.request.project.clone();
    let recheck = Duration::days(i64::from(
        config
            .project(&project)?
            .closure_recheck_days
            .unwrap_or(CLOSURE_RECHECK_DAYS),
    ));
    let now = tx.now().to_string();
    let asked = asked(tx.conn())?;
    let by_owner = owner_asked(tx.conn(), &project)?;

    let mut questions = Vec::new();
    let mut entries = Vec::new();
    let mut skipped = Vec::new();
    let mut waiting = Vec::new();
    for (id, alias) in l.request.items.iter().zip(&l.request.aliases) {
        let o = match observed(tx.conn(), config, &project, id)? {
            None => {
                skipped.push(json!({"item": alias, "why": "never read"}));
                continue;
            }
            Some(o) if !o.issue => {
                skipped.push(json!({"item": alias, "why": "not an issue"}));
                continue;
            }
            Some(o) if !o.open => {
                skipped.push(json!({"item": alias, "why": "closed"}));
                continue;
            }
            Some(o) => o,
        };
        let t = translation_of(config, &o.repo);
        let areas = areas_of(&t);
        let mut asked_here = 0;
        for f in cat.decisions() {
            let last = asked.get(&(id.clone(), f.decision.clone()));
            let wanted = if stronger {
                last.is_some_and(|a| needs_stronger_look(&a.suggestion))
            } else {
                match stage {
                    Stage::First => f.decision != "priority" && (scope == Scope::Full || f.scope == Scope::Both),
                    Stage::Priority => f.decision == "priority",
                }
            };
            if !wanted || !applies(f, &o.source, &areas) {
                continue;
            }
            let qhash = cat.question_hash(&f.decision, &areas)?;
            if !stronger
                && !due(
                    last,
                    &qhash,
                    o.seq,
                    is_closure(f),
                    recheck,
                    by_owner.get(id).map(String::as_str),
                    &now,
                )?
            {
                continue;
            }
            let (input, ihash) = match live_input(config, cat, &asked, &o, &f.decision)? {
                Ok(x) => x,
                Err(why) if stronger => {
                    skipped.push(json!({"item": alias, "why": format!("{}: {why}", f.decision)}));
                    continue;
                }
                Err(why) => {
                    waiting.push(format!("{} ({why})", o.source.alias));
                    continue;
                }
            };
            let qid = question_id(launch_id, id, &f.decision, &qhash, &ihash, o.seq);
            let map_id = map_in_force(tx.conn(), &project, &backend, &f.decision, &qhash)?;
            entries.push(json!({
                "id": qid,
                "item": o.source.alias,
                "decision": f.decision,
                "kind": f.kind,
                "question": f.question.trim(),
                "answers": cat.allowed(&f.decision, &areas)?,
                "subject": f.subject,
                "input": input,
            }));
            questions.push(Question {
                id: qid,
                item: id.clone(),
                alias: o.source.alias.clone(),
                decision: f.decision.clone(),
                question_hash: qhash,
                input_hash: ihash,
                observed: o.seq,
                map_id,
            });
            asked_here += 1;
        }
        if asked_here == 0 {
            let why = if stronger {
                "nothing needs a stronger look"
            } else {
                "nothing due"
            };
            skipped.push(json!({"item": alias, "why": why}));
        }
    }
    if stage == Stage::Priority && questions.is_empty() && !waiting.is_empty() {
        return Err(Error::refused(format!(
            "priority cannot be prepared before stage 1 is recorded: {}",
            waiting.join("; ")
        )));
    }

    let (seq, repeat) = if questions.is_empty() {
        (None, false)
    } else {
        let ids: Vec<String> = questions.iter().map(|q| q.id.clone()).collect();
        let data = PreparedData {
            project: project.clone(),
            launch: launch_id.to_string(),
            backend: backend.clone(),
            stage,
            questions,
        };
        let r = tx.record_valid(
            Kind::Prepared,
            launch_id,
            json!({"stage": stage, "questions": hash_value(&json!(ids))}),
            serde_json::to_value(&data)?,
            Dedupe::Any,
        )?;
        (Some(r.event().seq), !r.is_new())
    };
    let n = entries.len();
    Ok(Prepared {
        file: json!({
            "launch": launch_id,
            "backend": backend,
            "stage": stage,
            "questions": entries,
        }),
        report: PrepareReport {
            launch: launch_id.to_string(),
            backend,
            stage,
            questions: n,
            seq,
            repeat,
            skipped,
        },
    })
}

// --- Answers --------------------------------------------------------------------------------

/// Checks a stated answer against its question and returns (answer, raw probabilities):
/// every allowed answer and nothing else, each in [0, 1]; a single choice summing to 1 within
/// [`SUM_TOLERANCE`]. Each probability is clipped to [`CLIP`], then a single choice is
/// normalised to sum to 1. Areas are yes/no per area, not normalised (1.4).
pub fn check_probabilities(
    kind: AnswerKind,
    allowed: &[String],
    stated: &BTreeMap<String, f64>,
) -> std::result::Result<(Value, BTreeMap<String, f64>), String> {
    let keys: BTreeSet<&String> = stated.keys().collect();
    let want: BTreeSet<&String> = allowed.iter().collect();
    if keys != want {
        return Err(format!("the probabilities must name exactly {}", allowed.join(", ")));
    }
    if let Some((k, p)) = stated.iter().find(|(_, p)| !p.is_finite() || !(0.0..=1.0).contains(*p)) {
        return Err(format!("{k}: probability {p} is not between 0 and 1"));
    }
    let clipped: BTreeMap<String, f64> = stated
        .iter()
        .map(|(k, p)| (k.clone(), p.clamp(CLIP.0, CLIP.1)))
        .collect();
    if kind == AnswerKind::Areas {
        let mut set: Vec<&String> = allowed.iter().filter(|a| clipped[*a] >= 0.5).collect();
        if set.is_empty() {
            // The top one; on a tie, the first in the list.
            let top = allowed
                .iter()
                .fold(None::<&String>, |best, a| match best {
                    Some(b) if clipped[b] >= clipped[a] => Some(b),
                    _ => Some(a),
                })
                .expect("a repo with areas lists at least one");
            set.push(top);
        }
        return Ok((json!(set), clipped));
    }
    let sum: f64 = stated.values().sum();
    if (sum - 1.0).abs() > SUM_TOLERANCE {
        return Err(format!("the probabilities sum to {sum}, not 1 within {SUM_TOLERANCE}"));
    }
    let total: f64 = clipped.values().sum();
    let raw: BTreeMap<String, f64> = clipped.into_iter().map(|(k, p)| (k, p / total)).collect();
    let answer = match kind {
        AnswerKind::Boolean => json!(raw["true"] > raw["false"]),
        _ => {
            let top = allowed
                .iter()
                .fold(None::<&String>, |best, a| match best {
                    Some(b) if raw[b] >= raw[a] => Some(b),
                    _ => Some(a),
                })
                .expect("a choice lists two or more answers");
            json!(top)
        }
    };
    Ok((answer, raw))
}
