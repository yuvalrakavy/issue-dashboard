//! `eval` (triage spec 2.4, 3.2-3.5): the split, replay questions and their manifest, the
//! calibration fit and its maps, the metrics and bars, the `evaluated` event, and drift.
//!
//! **Replay** (label decisions): `prepare` builds a blinded question per sampled history item
//! (the replay cutoff of 1.1), split by item into tuning and held-out, and writes a manifest of
//! question ids, input hashes and truths under `state/evals/`; the session answers both files;
//! `score` rebuilds the manifest from history and the ledger and refuses any difference, fits
//! the map on tuning, freezes it, then scores the held-out answers through it. Priority asks
//! in two stages, as triage does (2.1): the first stage's answers become priority's input.
//!
//! **Live** (closure decisions): the recorded suggestions under the backend, scored against
//! the verdicts that followed (`truth::closure_truth`), split by item the same way.
//!
//! **The map in force** for a key (project, backend, decision, question hash) is the one its
//! latest `evaluated` event names; a fit at the grid edge names none (`uncalibratable`).

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::canonical::{hash_value, sha256_hex};
use crate::config::ConfigSet;
use crate::derive::INSIDE;
use crate::event::Kind;
use crate::history::{self, HistoryItem, RepoHistory};
use crate::ledger::{self, Dedupe, Tx};
use crate::root::parse_time;
use crate::suggest::Answer;
use crate::triage::{self, AnswerKind, Catalogue, Comment, LABEL_DECISIONS, Source};
use crate::truth::{self, CLOSURE_DECISIONS, Record};
use crate::{Error, Result};

/// The share of the sha256 range that is tuning (3.4).
pub const TUNING_SHARE: f64 = 0.4;
/// The most tuning items a replay asks about, unless the project says (3.2).
pub const EVAL_TUNING: u32 = 120;
/// The most held-out items a replay asks about, unless the project says (3.4).
pub const EVAL_SAMPLE: u32 = 120;
/// The temperature grid (2.4): 0.25 to 4 in steps of 0.05, as hundredths.
pub const GRID: (u32, u32, u32) = (25, 400, 5);
/// ECE bins (3.4).
pub const BINS: usize = 10;
/// Bootstrap resamples (3.4).
pub const RESAMPLES: usize = 1000;
/// A truth answer given at most this is a second opinion's false alarm (3.4, 4.1).
pub const FALSE_ALARM_AT: f64 = 0.2;
/// The Evaluated bar (3.4, owner 2026-10-25).
pub const EVALUATED_MIN_N: usize = 100;
pub const EVALUATED_MAX_ECE: f64 = 0.10;
/// The Bulk bar's accuracy (3.4).
pub const BULK_MIN_ACCURACY: f64 = 0.85;
/// Drift (3.5): the last this many owner records, more than this far below.
pub const DRIFT_RECORDS: usize = 30;
pub const DRIFT_POINTS: f64 = 0.10;

// --- Split ----------------------------------------------------------------------------------

/// Whether an item is tuning (3.4): the first 40 % of the sha256 range of its node id. Stable as
/// truth grows, and the same for every decision.
pub fn is_tuning(node_id: &str) -> bool {
    let h = u64::from_str_radix(&sha256_hex(node_id)[..15], 16).expect("hex");
    (h as f64) / (16f64.powi(15)) < TUNING_SHARE
}

/// The order a sample is taken in (3.4): by sha256 of the node id.
pub fn hash_order(node_id: &str) -> String {
    sha256_hex(node_id)
}

// --- Calibration ----------------------------------------------------------------------------

/// The temperatures of the grid, in order.
pub fn grid() -> Vec<f64> {
    let (lo, hi, step) = GRID;
    (lo..=hi).step_by(step as usize).map(|h| f64::from(h) / 100.0).collect()
}

/// `raw` through temperature `t` (2.4): softmax(log p / t) over the answers; per area as yes/no.
pub fn apply_t(kind: AnswerKind, raw: &BTreeMap<String, f64>, t: f64) -> BTreeMap<String, f64> {
    let pow = |p: f64| (p.max(1e-12).ln() / t).exp();
    match kind {
        AnswerKind::Areas => raw
            .iter()
            .map(|(a, p)| {
                let (y, n) = (pow(*p), pow(1.0 - p));
                (a.clone(), y / (y + n))
            })
            .collect(),
        AnswerKind::Choice | AnswerKind::Boolean => {
            let w: BTreeMap<&String, f64> = raw.iter().map(|(a, p)| (a, pow(*p))).collect();
            let sum: f64 = w.values().sum();
            w.into_iter().map(|(a, x)| (a.clone(), x / sum)).collect()
        }
    }
}

/// One answered question with its truth: the probabilities (raw or calibrated) and the truth,
/// a string for a single choice ("true"/"false" for a boolean) or the area set.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Example {
    pub item: String,
    pub probs: BTreeMap<String, f64>,
    pub truth: Value,
    /// For an owner truth, `<provenance>/<from_backend>` (3.1: accuracy on owner records is
    /// reported apart); `None` for a label truth.
    pub source: Option<String>,
}

/// The owner source of a truth, for [`Example::source`].
pub fn owner_source(provenance: &str, from_backend: Option<&str>) -> Option<String> {
    matches!(provenance, "owner_accept" | "owner_edit")
        .then(|| format!("{provenance}/{}", from_backend.unwrap_or("none")))
}

/// Accuracy on the examples of each owner source (3.1); label truths are not counted.
pub fn by_source(kind: AnswerKind, allowed: &[String], examples: &[Example]) -> BTreeMap<String, SourceAccuracy> {
    let mut counts: BTreeMap<String, (usize, usize)> = BTreeMap::new();
    for e in examples {
        if let Some(src) = &e.source {
            let c = counts.entry(src.clone()).or_default();
            c.0 += 1;
            c.1 += usize::from(correct(kind, allowed, e));
        }
    }
    counts
        .into_iter()
        .map(|(k, (n, right))| {
            (
                k,
                SourceAccuracy {
                    n,
                    accuracy: right as f64 / n as f64,
                },
            )
        })
        .collect()
}

/// Accuracy on one owner source's held-out examples.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceAccuracy {
    pub n: usize,
    pub accuracy: f64,
}

fn truth_set(v: &Value) -> BTreeSet<String> {
    match v {
        Value::Array(a) => a.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        Value::String(s) => BTreeSet::from([s.clone()]),
        _ => BTreeSet::new(),
    }
}

fn truth_str(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Bool(b) => b.to_string(),
        other => other.to_string(),
    }
}

/// The log loss of `examples` under temperature `t`.
pub fn log_loss(kind: AnswerKind, examples: &[Example], t: f64) -> f64 {
    let mut loss = 0.0;
    for e in examples {
        let q = apply_t(kind, &e.probs, t);
        match kind {
            AnswerKind::Areas => {
                let truth = truth_set(&e.truth);
                for (a, p) in &q {
                    let p = if truth.contains(a) { *p } else { 1.0 - p };
                    loss -= p.max(1e-12).ln();
                }
            }
            AnswerKind::Choice | AnswerKind::Boolean => {
                loss -= q.get(&truth_str(&e.truth)).copied().unwrap_or(0.0).max(1e-12).ln();
            }
        }
    }
    loss
}

/// A temperature fit (2.4).
#[derive(Debug, Clone, Copy, PartialEq, Serialize)]
pub struct Fit {
    pub t: f64,
    /// The minimum is at either end of the grid: the key stays uncalibrated.
    pub at_edge: bool,
}

/// Fits `t` on the tuning examples: the grid temperature of least log loss, the lowest on a tie.
pub fn fit(kind: AnswerKind, examples: &[Example]) -> Fit {
    let g = grid();
    let mut best = (0, f64::INFINITY);
    for (i, t) in g.iter().enumerate() {
        let l = log_loss(kind, examples, *t);
        if l < best.1 {
            best = (i, l);
        }
    }
    Fit {
        t: g[best.0],
        at_edge: best.0 == 0 || best.0 == g.len() - 1,
    }
}

/// A calibration map (2.4). Its id is the hash of its contents, so it never changes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Map {
    pub project: String,
    pub backend: String,
    pub decision: String,
    pub question_hash: String,
    pub kind: AnswerKind,
    pub t: f64,
    /// How many tuning examples it was fitted on.
    pub fitted_on: usize,
}

impl Map {
    pub fn id(&self) -> String {
        hash_value(&serde_json::to_value(self).expect("a map serialises"))[..16].to_string()
    }
}

/// A path segment for a name: anything but letters, digits and `@.-_` is percent-encoded, so
/// two names never share a segment (`a/b_c` and `a_b/c` stay apart).
fn segment(s: &str) -> String {
    let mut out = String::new();
    for c in s.chars() {
        if c.is_ascii_alphanumeric() || "@.-_".contains(c) {
            out.push(c);
        } else {
            let mut buf = [0u8; 4];
            for b in c.encode_utf8(&mut buf).bytes() {
                out.push_str(&format!("%{b:02X}"));
            }
        }
    }
    out
}

/// `state/evals/<project>/<backend>/`.
pub fn key_dir(root: &Path, project: &str, backend: &str) -> PathBuf {
    root.join("state")
        .join("evals")
        .join(segment(project))
        .join(segment(backend))
}

/// Where a map is written (2.4): `state/evals/<project>/<backend>/<decision>-<hash>-<map_id>.json`.
pub fn map_path(root: &Path, m: &Map) -> PathBuf {
    key_dir(root, &m.project, &m.backend).join(format!("{}-{}-{}.json", m.decision, m.question_hash, m.id()))
}

/// The subject of a key's `evaluated` events.
pub fn key_subject(project: &str, backend: &str, decision: &str, hash: &str) -> String {
    format!("{project}/{backend}/{decision}/{hash}")
}

/// The map in force for a key (2.4): the latest `evaluated` event's `map_id`.
pub fn map_in_force(
    conn: &Connection,
    project: &str,
    backend: &str,
    decision: &str,
    hash: &str,
) -> Result<Option<String>> {
    Ok(
        ledger::latest(conn, &key_subject(project, backend, decision, hash), Kind::Evaluated)?
            .and_then(|e| e.data["map_id"].as_str().map(str::to_string)),
    )
}

/// The temperature of map `map_id`, from the `evaluated` event that wrote it.
pub fn map_t(conn: &Connection, map_id: &str) -> Result<f64> {
    ledger::events_by_kind(conn, Kind::Evaluated)?
        .into_iter()
        .find(|e| e.data["map_id"] == map_id)
        .and_then(|e| e.data["t"].as_f64())
        .ok_or_else(|| Error::refused(format!("no evaluated event wrote calibration map {map_id}")))
}

// --- Metrics --------------------------------------------------------------------------------

/// Plain ℓ1 ECE (3.4): Σ_b (n_b/N)·|ā_b − c̄_b| over bins [k/10, (k+1)/10), the last closed,
/// empty bins skipped. NaN for no points.
pub fn ece(points: &[(f64, bool)]) -> f64 {
    let mut bins = [(0usize, 0.0f64, 0.0f64); BINS];
    for (c, ok) in points {
        let b = ((c * BINS as f64).floor() as usize).min(BINS - 1);
        bins[b].0 += 1;
        bins[b].1 += c;
        bins[b].2 += if *ok { 1.0 } else { 0.0 };
    }
    let n = points.len() as f64;
    bins.iter()
        .filter(|b| b.0 > 0)
        .map(|(k, conf, acc)| (*k as f64 / n) * (acc / *k as f64 - conf / *k as f64).abs())
        .sum::<f64>()
        / if points.is_empty() { f64::NAN } else { 1.0 }
}

/// The first answer of `order` with the highest probability (the tie rule of 1.4).
fn top<'a>(probs: &BTreeMap<String, f64>, order: &'a [String]) -> &'a str {
    order
        .iter()
        .fold(None::<&String>, |best, a| match best {
            Some(b) if probs.get(b).copied().unwrap_or(0.0) >= probs.get(a).copied().unwrap_or(0.0) => Some(b),
            _ => Some(a),
        })
        .map_or("", String::as_str)
}

/// The order ties are broken in: a boolean is true only above false (1.4).
fn order(kind: AnswerKind, allowed: &[String]) -> Vec<String> {
    match kind {
        AnswerKind::Boolean => vec!["false".into(), "true".into()],
        _ => allowed.to_vec(),
    }
}

/// The set an area answer chooses (1.4): every area at 0.5 or above, or the top one.
fn chosen_areas(probs: &BTreeMap<String, f64>, allowed: &[String]) -> BTreeSet<String> {
    let set: BTreeSet<String> = allowed
        .iter()
        .filter(|a| probs.get(*a).copied().unwrap_or(0.0) >= 0.5)
        .cloned()
        .collect();
    if set.is_empty() && !allowed.is_empty() {
        return BTreeSet::from([top(probs, allowed).to_string()]);
    }
    set
}

/// The ECE points of one example (3.4): its top answer's confidence and whether it is right;
/// for area every per-area yes/no.
fn points_of(kind: AnswerKind, allowed: &[String], e: &Example) -> Vec<(f64, bool)> {
    match kind {
        AnswerKind::Areas => {
            let truth = truth_set(&e.truth);
            allowed
                .iter()
                .map(|a| {
                    let q = e.probs.get(a).copied().unwrap_or(0.0);
                    (q.max(1.0 - q), (q >= 0.5) == truth.contains(a))
                })
                .collect()
        }
        _ => {
            let o = order(kind, allowed);
            let t = top(&e.probs, &o);
            vec![(e.probs.get(t).copied().unwrap_or(0.0), t == truth_str(&e.truth))]
        }
    }
}

fn correct(kind: AnswerKind, allowed: &[String], e: &Example) -> bool {
    match kind {
        AnswerKind::Areas => chosen_areas(&e.probs, allowed) == truth_set(&e.truth),
        _ => top(&e.probs, &order(kind, allowed)) == truth_str(&e.truth),
    }
}

/// The share of examples (area: of (item, area) pairs in the truth set) whose truth answer was
/// given at most [`FALSE_ALARM_AT`] (3.4); NaN for none.
pub fn false_alarm_rate(kind: AnswerKind, examples: &[Example]) -> f64 {
    let (mut alarms, mut n) = (0usize, 0usize);
    for e in examples {
        match kind {
            AnswerKind::Areas => {
                for a in truth_set(&e.truth) {
                    n += 1;
                    if e.probs.get(&a).copied().unwrap_or(0.0) <= FALSE_ALARM_AT {
                        alarms += 1;
                    }
                }
            }
            _ => {
                n += 1;
                if e.probs.get(&truth_str(&e.truth)).copied().unwrap_or(0.0) <= FALSE_ALARM_AT {
                    alarms += 1;
                }
            }
        }
    }
    alarms as f64 / n as f64
}

/// Per-answer (classwise) ECE (3.4): for each answer, its probability against whether it is the
/// truth (area: each area's yes probability against membership).
pub fn classwise_ece(kind: AnswerKind, allowed: &[String], examples: &[Example]) -> BTreeMap<String, f64> {
    allowed
        .iter()
        .map(|c| {
            let pts: Vec<(f64, bool)> = examples
                .iter()
                .map(|e| {
                    let hit = match kind {
                        AnswerKind::Areas => truth_set(&e.truth).contains(c),
                        _ => truth_str(&e.truth) == *c,
                    };
                    (e.probs.get(c).copied().unwrap_or(0.0), hit)
                })
                .collect();
            (c.clone(), ece(&pts))
        })
        .collect()
}

/// splitmix64: a small deterministic generator, so an interval is reproducible from its seed.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
}

/// The seed of a key (3.4): the first 16 hex of the sha256 of its subject.
pub fn seed_of(key: &str) -> u64 {
    u64::from_str_radix(&sha256_hex(key)[..16], 16).expect("hex")
}

/// The ECE interval (3.4): [`RESAMPLES`] bootstrap resamples by item, the 5th and 95th
/// percentiles (nearest rank).
pub fn ece_interval(kind: AnswerKind, allowed: &[String], examples: &[Example], seed: u64) -> (f64, f64) {
    let mut by_item: BTreeMap<&str, Vec<(f64, bool)>> = BTreeMap::new();
    for e in examples {
        by_item.entry(&e.item).or_default().extend(points_of(kind, allowed, e));
    }
    let items: Vec<&Vec<(f64, bool)>> = by_item.values().collect();
    if items.is_empty() {
        return (f64::NAN, f64::NAN);
    }
    let mut rng = Rng(seed);
    let mut values = Vec::with_capacity(RESAMPLES);
    for _ in 0..RESAMPLES {
        let mut pts = Vec::new();
        for _ in 0..items.len() {
            pts.extend_from_slice(items[(rng.next() % items.len() as u64) as usize]);
        }
        values.push(ece(&pts));
    }
    values.sort_by(f64::total_cmp);
    let at = |p: f64| values[(p * (RESAMPLES - 1) as f64).round() as usize];
    (at(0.05), at(0.95))
}

/// The bars (3.4).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Bars {
    pub evaluated: bool,
    pub bulk: bool,
}

pub fn bars(n: usize, ece: f64, accuracy: f64) -> Bars {
    let evaluated = n >= EVALUATED_MIN_N && ece <= EVALUATED_MAX_ECE;
    Bars {
        evaluated,
        bulk: evaluated && accuracy >= BULK_MIN_ACCURACY,
    }
}

/// What one evaluation measured on the held-out examples.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Metrics {
    pub n: usize,
    pub accuracy: f64,
    pub ece: f64,
    pub ece_interval: (f64, f64),
    pub classwise_ece: BTreeMap<String, f64>,
    pub false_alarm_rate: f64,
    pub bars: Bars,
}

/// Measures the (calibrated) held-out `examples`; a non-finite result refuses (3.4).
pub fn metrics(kind: AnswerKind, allowed: &[String], examples: &[Example], seed: u64) -> Result<Metrics> {
    let n = examples.len();
    if n == 0 {
        return Err(Error::refused(
            "no held-out example has an answer and a truth: nothing to score",
        ));
    }
    let pts: Vec<(f64, bool)> = examples.iter().flat_map(|e| points_of(kind, allowed, e)).collect();
    let accuracy = examples.iter().filter(|e| correct(kind, allowed, e)).count() as f64 / n as f64;
    let e = ece(&pts);
    let interval = ece_interval(kind, allowed, examples, seed);
    let fa = false_alarm_rate(kind, examples);
    let classwise = classwise_ece(kind, allowed, examples);
    let finite = [accuracy, e, interval.0, interval.1].iter().all(|x| x.is_finite());
    if !finite {
        return Err(Error::refused(format!(
            "the evaluation is not finite (accuracy {accuracy}, ECE {e}, interval {interval:?}); nothing recorded"
        )));
    }
    Ok(Metrics {
        n,
        accuracy,
        ece: e,
        ece_interval: interval,
        classwise_ece: classwise,
        false_alarm_rate: fa,
        bars: bars(n, e, accuracy),
    })
}

// --- Replay questions -----------------------------------------------------------------------

/// Which part of a replay a manifest is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvalStage {
    /// Priority's first stage: the decisions priority reads, about the sampled items.
    First,
    /// The decision measured.
    Scored,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Split {
    Tuning,
    HeldOut,
}

/// First-stage answers by item, then decision: [answer, calibrated probabilities].
pub type StageOne = BTreeMap<String, BTreeMap<String, (Value, Value)>>;

/// Answers by question id: the answer and its stated probabilities.
pub type Answered = BTreeMap<String, (Value, BTreeMap<String, f64>)>;

/// One question as a manifest freezes it; the truth never reaches the question files.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ManifestQuestion {
    pub id: String,
    pub item: String,
    pub alias: String,
    pub decision: String,
    pub split: Split,
    pub input_hash: String,
    /// The truth of the decision measured, its provenance, and for an owner truth the
    /// backend of the suggestion it answered.
    pub truth: Value,
    pub provenance: String,
    pub from_backend: Option<String>,
}

/// A replay's manifest (3.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub project: String,
    pub backend: String,
    pub decision: String,
    pub question_hash: String,
    pub stage: EvalStage,
    pub exclusions: Vec<String>,
    pub repo: Option<String>,
    /// Priority's first-stage answers its inputs were built from, by item then decision:
    /// [answer, calibrated probabilities].
    pub stage_one: Option<StageOne>,
    pub questions: Vec<ManifestQuestion>,
}

impl Manifest {
    pub fn hash(&self) -> String {
        hash_value(&serde_json::to_value(self).expect("a manifest serialises"))
    }
}

/// What a replay is about.
#[derive(Debug, Clone)]
pub struct Replay {
    pub project: String,
    pub decision: String,
    pub backend: String,
    /// The provenances dropped (CLI `--exclude` plus the project's `eval_exclude`).
    pub exclude: Vec<String>,
    /// Only this repo's items: area's question differs per repo (1.3).
    pub repo: Option<String>,
}

impl Replay {
    fn excluded(&self) -> BTreeSet<String> {
        self.exclude.iter().cloned().collect()
    }
}

/// The exclusions in force: the project's `eval_exclude` and the command's, each checked.
pub fn exclusions(config: &ConfigSet, project: &str, extra: &[String]) -> Result<Vec<String>> {
    let mut out: BTreeSet<String> = config
        .project(project)?
        .eval_exclude
        .clone()
        .unwrap_or_default()
        .into_iter()
        .collect();
    out.extend(extra.iter().cloned());
    for x in &out {
        let ok = matches!(x.as_str(), "filed" | "github_later" | "owner_accept" | "owner_edit")
            || x.strip_prefix("bulk_review:")
                .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_alphanumeric() || "._-".contains(c)));
        if !ok {
            return Err(Error::refused(format!(
                "--exclude {x}: not a provenance (filed, github_later, owner_accept, owner_edit, bulk_review:<name>)"
            )));
        }
    }
    Ok(out.into_iter().collect())
}

/// A backend name (1.3): model@effort.
fn check_backend(b: &str) -> Result<()> {
    match b.split_once('@') {
        Some((m, e)) if !m.is_empty() && !e.is_empty() => Ok(()),
        _ => Err(Error::refused(format!("--backend {b}: a backend is model@effort"))),
    }
}

/// One history item that has a truth for the decision.
struct Candidate<'h> {
    item: &'h HistoryItem,
    repo: String,
    truth: truth::Truth,
    cutoff: Option<String>,
    source: Source,
}

/// The labels an issue carried at `cutoff`, from its label events; its labels now without one.
fn labels_at(item: &HistoryItem, cutoff: Option<&str>) -> Result<Vec<String>> {
    let Some(cut) = cutoff else {
        return Ok(item.labels.clone());
    };
    let cut = parse_time(cut)?;
    let mut set: Vec<String> = Vec::new();
    for e in &item.label_events {
        if parse_time(&e.at)? > cut {
            break;
        }
        if e.added {
            if !set.iter().any(|l| l.eq_ignore_ascii_case(&e.label)) {
                set.push(e.label.clone());
            }
        } else {
            set.retain(|l| !l.eq_ignore_ascii_case(&e.label));
        }
    }
    Ok(set)
}

/// A history item as an input's source (1.1): comment text included; reactions are not read.
fn source_of(item: &HistoryItem, labels: Vec<String>, maintainers: &[String]) -> Source {
    let inside =
        |association: &str, author: &str| INSIDE.contains(&association) || maintainers.iter().any(|m| m == author);
    Source {
        id: item.id.clone(),
        alias: item.alias.clone(),
        title: item.title.clone(),
        body: item.body.clone(),
        body_truncated: item.body_truncated,
        labels,
        author: item.author.clone(),
        inside: inside(&item.author_association, &item.author),
        comments: item
            .comments
            .iter()
            .map(|c| Comment {
                author: c.author.clone(),
                inside: inside(&c.association, &c.author),
                at: c.at.clone(),
                body: Some(c.body.clone()),
            })
            .collect(),
        reactions: 0,
    }
}

/// Every closed history item in its window with a truth for `r.decision`, in hash order.
fn candidates<'h>(
    conn: &Connection,
    config: &ConfigSet,
    history: &'h [RepoHistory],
    r: &Replay,
) -> Result<Vec<Candidate<'h>>> {
    let tags = truth::tags(conn, &r.project)?;
    let mut owner: BTreeMap<String, Vec<Record>> = BTreeMap::new();
    for rec in truth::owner_records(conn, &r.project)? {
        owner.entry(rec.item.clone()).or_default().push(rec);
    }
    let maintainers = config.project(&r.project)?.maintainers.clone().unwrap_or_default();
    let exclude = r.excluded();
    let mut out = Vec::new();
    for h in history {
        if r.repo.as_ref().is_some_and(|x| !x.eq_ignore_ascii_case(&h.repo)) {
            continue;
        }
        let t = triage::translation_of(config, &h.repo);
        if r.decision == "area" && t.areas.is_empty() {
            continue;
        }
        for item in &h.items {
            if item.state != "closed" || item.closed_at.as_deref().is_none_or(|c| c < h.since.as_str()) {
                continue;
            }
            let mut records = truth::label_records(&t, item, &tags)?;
            records.extend(owner.get(&item.id).cloned().unwrap_or_default());
            let Some(found) = truth::fold(&r.decision, &records, &exclude) else {
                continue;
            };
            // An item whose folded area truth is empty is left out of area's eval (3.4).
            if r.decision == "area" && found.value.as_array().is_some_and(Vec::is_empty) {
                continue;
            }
            // The replay cutoff (1.1): the earliest truth time among the label decisions.
            let cutoff = records
                .iter()
                .filter(|x| LABEL_DECISIONS.contains(&x.decision.as_str()))
                .map(|x| x.at)
                .min()
                .map(crate::root::format_time);
            let labels = labels_at(item, cutoff.as_deref())?;
            out.push(Candidate {
                source: source_of(item, labels, &maintainers),
                item,
                repo: h.repo.clone(),
                truth: found,
                cutoff,
            });
        }
    }
    out.sort_by_key(|c| hash_order(&c.item.id));
    Ok(out)
}

fn eval_question_id(r: &Replay, decision: &str, qhash: &str, item: &str, ihash: &str) -> String {
    format!(
        "q-{}",
        &sha256_hex(&format!(
            "eval|{}|{}|{decision}|{qhash}|{item}|{ihash}",
            r.project, r.backend
        ))[..20]
    )
}

/// A replay's questions: the manifest, and the entries of the two question files.
pub struct Built {
    pub manifest: Manifest,
    /// The question file entries, by split.
    pub files: BTreeMap<Split, Vec<Value>>,
    pub available: BTreeMap<Split, usize>,
}

/// Builds a replay's questions (3.2) from history and the ledger: the manifest's stage, its
/// sample, and for priority's scored stage the first stage's answers.
pub fn build(
    conn: &Connection,
    config: &ConfigSet,
    cat: &Catalogue,
    history: &[RepoHistory],
    r: &Replay,
    stage: EvalStage,
    stage_one: Option<&StageOne>,
) -> Result<Built> {
    check_backend(&r.backend)?;
    if !LABEL_DECISIONS.contains(&r.decision.as_str()) {
        return Err(Error::refused(format!(
            "{} is judged live (eval score --live), not replayed: only type, area and priority are",
            r.decision
        )));
    }
    let f = cat.file(&r.decision)?;
    if stage == EvalStage::First && f.reads.is_empty() {
        return Err(Error::refused(format!("{} has no first stage", r.decision)));
    }
    let p = config.project(&r.project)?;
    let caps = BTreeMap::from([
        (Split::Tuning, p.eval_tuning.unwrap_or(EVAL_TUNING) as usize),
        (Split::HeldOut, p.eval_sample.unwrap_or(EVAL_SAMPLE) as usize),
    ]);
    let all = candidates(conn, config, history, r)?;
    let mut hashes: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for c in &all {
        let areas = triage::areas_of(&triage::translation_of(config, &c.repo));
        hashes
            .entry(cat.question_hash(&r.decision, &areas)?)
            .or_default()
            .push(c.repo.clone());
    }
    if hashes.len() > 1 {
        let repos: BTreeSet<&String> = hashes.values().flatten().collect();
        return Err(Error::refused(format!(
            "{} is asked differently in {} repos ({}): evaluate one with --repo",
            r.decision,
            hashes.len(),
            repos.into_iter().cloned().collect::<Vec<_>>().join(", ")
        )));
    }
    let qhash = match hashes.keys().next() {
        Some(h) => h.clone(),
        None => cat.question_hash(&r.decision, &[])?,
    };
    let mut available = BTreeMap::from([(Split::Tuning, 0), (Split::HeldOut, 0)]);
    let mut taken = BTreeMap::from([(Split::Tuning, 0), (Split::HeldOut, 0)]);
    let mut questions = Vec::new();
    let mut files: BTreeMap<Split, Vec<Value>> = BTreeMap::new();
    for c in &all {
        let split = if is_tuning(&c.item.id) {
            Split::Tuning
        } else {
            Split::HeldOut
        };
        *available.get_mut(&split).expect("both splits") += 1;
        if taken[&split] >= caps[&split] {
            continue;
        }
        let t = triage::translation_of(config, &c.repo);
        let areas = triage::areas_of(&t);
        let asked: Vec<String> = match stage {
            EvalStage::First => f.reads.clone(),
            EvalStage::Scored => vec![r.decision.clone()],
        };
        let extra = match (stage, f.reads.is_empty()) {
            (EvalStage::Scored, false) => {
                let Some(recorded) = stage_one.and_then(|s| s.get(&c.item.id)) else {
                    // The first stage left this item unanswered: it is not asked.
                    continue;
                };
                Some(triage::priority_extra(cat, recorded)?)
            }
            _ => None,
        };
        *taken.get_mut(&split).expect("both splits") += 1;
        for d in asked {
            let df = cat.file(&d)?;
            if !triage::applies(df, &c.source, &areas) {
                continue;
            }
            let input = triage::input(cat, &d, &c.source, &t, c.cutoff.as_deref(), extra.clone())?;
            let ihash = triage::input_hash(&input);
            let dhash = cat.question_hash(&d, &areas)?;
            let id = eval_question_id(r, &d, &dhash, &c.item.id, &ihash);
            files.entry(split).or_default().push(json!({
                "id": id,
                "item": c.item.alias,
                "decision": d,
                "kind": df.kind,
                "question": df.question.trim(),
                "answers": cat.allowed(&d, &areas)?,
                "input": input,
            }));
            questions.push(ManifestQuestion {
                id,
                item: c.item.id.clone(),
                alias: c.item.alias.clone(),
                decision: d,
                split,
                input_hash: ihash,
                truth: c.truth.value.clone(),
                provenance: c.truth.provenance.clone(),
                from_backend: c.truth.from_backend.clone(),
            });
        }
    }
    Ok(Built {
        manifest: Manifest {
            project: r.project.clone(),
            backend: r.backend.clone(),
            decision: r.decision.clone(),
            question_hash: qhash,
            stage,
            exclusions: r.exclude.clone(),
            repo: r.repo.clone(),
            stage_one: stage_one.cloned(),
            questions,
        },
        files,
        available,
    })
}

/// Where a replay's manifest is kept: one per key and stage, the latest prepare's.
pub fn manifest_path(
    root: &Path,
    project: &str,
    backend: &str,
    decision: &str,
    repo: Option<&str>,
    stage: EvalStage,
) -> PathBuf {
    let stage = match stage {
        EvalStage::First => "first",
        EvalStage::Scored => "replay",
    };
    let repo = repo.map(|r| format!("-{}", segment(r))).unwrap_or_default();
    key_dir(root, project, backend).join(format!("{decision}{repo}.{stage}.manifest.json"))
}

fn load_manifest(path: &Path) -> Result<Manifest> {
    let text = fs::read_to_string(path)
        .map_err(|e| Error::refused(format!("{}: no manifest ({e}): run eval prepare first", path.display())))?;
    serde_json::from_str(&text).map_err(|e| Error::refused(format!("{}: not a manifest: {e}", path.display())))
}

/// An answers file of an eval session (3.2).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvalAnswers {
    pub backend: String,
    pub answers: Vec<Answer>,
}

/// The answers checked against a manifest: by question id, (answer, raw probabilities). Every
/// answer must answer one of its questions, once, under its backend, with valid probabilities;
/// one that does not refuses the files with every reason listed.
fn checked_answers(cat: &Catalogue, config: &ConfigSet, m: &Manifest, files: &[EvalAnswers]) -> Result<Answered> {
    let by_id: BTreeMap<&str, &ManifestQuestion> = m.questions.iter().map(|q| (q.id.as_str(), q)).collect();
    let mut out = BTreeMap::new();
    let mut errors = Vec::new();
    for f in files {
        if f.backend != m.backend {
            errors.push(format!(
                "the answers are {}'s, but the manifest measures {}",
                f.backend, m.backend
            ));
            continue;
        }
        for a in &f.answers {
            let Some(q) = by_id.get(a.question.as_str()) else {
                errors.push(format!("{}: the manifest has no such question", a.question));
                continue;
            };
            if out.contains_key(&a.question) {
                errors.push(format!("{} is answered twice", a.question));
                continue;
            }
            let qf = cat.file(&q.decision)?;
            let repo = q.alias.rsplit_once('#').map_or(q.alias.as_str(), |(r, _)| r);
            let areas = triage::areas_of(&triage::translation_of(config, repo));
            match triage::check_probabilities(qf.kind, &cat.allowed(&q.decision, &areas)?, &a.probabilities) {
                Ok(x) => {
                    out.insert(a.question.clone(), x);
                }
                Err(m) => errors.push(format!("{} ({}, {}): {m}", q.alias, q.decision, q.id)),
            }
        }
    }
    if !errors.is_empty() {
        return Err(Error::refused(format!(
            "the answers are refused: {}",
            errors.join("; ")
        )));
    }
    Ok(out)
}

/// What `eval prepare` wrote.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PrepareReport {
    pub project: String,
    pub backend: String,
    pub decision: String,
    pub question_hash: String,
    pub stage: EvalStage,
    pub manifest: PathBuf,
    pub manifest_hash: String,
    pub files: BTreeMap<String, PathBuf>,
    /// Questions per split.
    pub questions: BTreeMap<String, usize>,
    /// Items with a truth per split, before the sample caps.
    pub available: BTreeMap<String, usize>,
    pub exclusions: Vec<String>,
}

fn split_name(s: Split) -> &'static str {
    match s {
        Split::Tuning => "tuning",
        Split::HeldOut => "heldout",
    }
}

/// Priority's first-stage answers as its input holds them, by item then decision: the answer
/// and the probabilities calibrated through each first-stage key's map in force.
fn stage_one_answers(
    conn: &Connection,
    cat: &Catalogue,
    config: &ConfigSet,
    first: &Manifest,
    files: &[EvalAnswers],
) -> Result<StageOne> {
    let answered = checked_answers(cat, config, first, files)?;
    let mut by_item: StageOne = BTreeMap::new();
    for q in &first.questions {
        let Some((answer, raw)) = answered.get(&q.id) else {
            continue;
        };
        let kind = cat.file(&q.decision)?.kind;
        let qhash = cat.question_hash(&q.decision, &[])?;
        let map = map_in_force(conn, &first.project, &first.backend, &q.decision, &qhash)?;
        let calibrated = triage::calibrate(conn, kind, raw, map.as_deref())?;
        by_item
            .entry(q.item.clone())
            .or_default()
            .insert(q.decision.clone(), (answer.clone(), json!(calibrated)));
    }
    let reads = &cat.file(&first.decision)?.reads;
    // Only an item with every first-stage answer is asked priority.
    by_item.retain(|_, m| reads.iter().all(|d| m.contains_key(d)));
    Ok(by_item)
}

/// `eval prepare` (3.2): writes the tuning and held-out question files into `out` and the
/// manifest under `state/evals/`. Priority's scored stage needs the first stage's answers.
#[allow(clippy::too_many_arguments)]
pub fn prepare(
    conn: &Connection,
    root: &Path,
    config: &ConfigSet,
    r: &Replay,
    stage: EvalStage,
    first_answers: &[EvalAnswers],
    out: &Path,
) -> Result<PrepareReport> {
    let cat = Catalogue::embedded();
    let history = history::project_history(root, config, &r.project)?;
    let two_stage = !cat.file(&r.decision)?.reads.is_empty();
    let stage_one = match (stage, two_stage) {
        (EvalStage::Scored, true) => {
            if first_answers.is_empty() {
                return Err(Error::refused(format!(
                    "{} is prepared in two stages: prepare --stage first, answer it, then pass its answers",
                    r.decision
                )));
            }
            let path = manifest_path(
                root,
                &r.project,
                &r.backend,
                &r.decision,
                r.repo.as_deref(),
                EvalStage::First,
            );
            let first = load_manifest(&path)?;
            verify(conn, config, cat, &history, &first)?;
            Some(stage_one_answers(conn, cat, config, &first, first_answers)?)
        }
        _ => None,
    };
    let built = build(conn, config, cat, &history, r, stage, stage_one.as_ref())?;
    let m = &built.manifest;
    let path = manifest_path(root, &r.project, &r.backend, &r.decision, r.repo.as_deref(), stage);
    fs::create_dir_all(path.parent().expect("a manifest has a directory"))?;
    fs::write(&path, serde_json::to_string_pretty(m)? + "\n")?;
    fs::create_dir_all(out)?;
    let mut files = BTreeMap::new();
    let mut counts = BTreeMap::new();
    for split in [Split::Tuning, Split::HeldOut] {
        let entries = built.files.get(&split).cloned().unwrap_or_default();
        let name = match stage {
            EvalStage::First => format!("{}-first.json", split_name(split)),
            EvalStage::Scored => format!("{}.json", split_name(split)),
        };
        let file = out.join(&name);
        counts.insert(split_name(split).to_string(), entries.len());
        fs::write(
            &file,
            serde_json::to_string_pretty(&json!({
                "project": m.project,
                "backend": m.backend,
                "decision": m.decision,
                "split": split,
                "questions": entries,
            }))? + "\n",
        )?;
        files.insert(split_name(split).to_string(), file);
    }
    Ok(PrepareReport {
        project: m.project.clone(),
        backend: m.backend.clone(),
        decision: m.decision.clone(),
        question_hash: m.question_hash.clone(),
        stage,
        manifest: path,
        manifest_hash: m.hash(),
        files,
        questions: counts,
        available: built
            .available
            .iter()
            .map(|(s, n)| (split_name(*s).to_string(), *n))
            .collect(),
        exclusions: m.exclusions.clone(),
    })
}

/// Rebuilds a manifest from history and the ledger and refuses any difference (3.2: score
/// re-verifies the manifest): a changed history, truth, tag or question file means the answers
/// are about questions that are no longer the ones asked.
fn verify(conn: &Connection, config: &ConfigSet, cat: &Catalogue, history: &[RepoHistory], m: &Manifest) -> Result<()> {
    let r = Replay {
        project: m.project.clone(),
        decision: m.decision.clone(),
        backend: m.backend.clone(),
        exclude: m.exclusions.clone(),
        repo: m.repo.clone(),
    };
    let again = build(conn, config, cat, history, &r, m.stage, m.stage_one.as_ref())?.manifest;
    if again != *m {
        let key = |q: &ManifestQuestion| format!("{}|{}|{}", q.id, q.truth, q.provenance);
        let before: BTreeSet<String> = m.questions.iter().map(key).collect();
        let after: BTreeSet<String> = again.questions.iter().map(key).collect();
        let changed = before.symmetric_difference(&after).count();
        return Err(Error::refused(format!(
            "the manifest of {} no longer matches history and the ledger ({changed} questions or truths differ, question hash {} → {}): eval prepare again",
            m.decision, m.question_hash, again.question_hash
        )));
    }
    Ok(())
}

// --- Score ----------------------------------------------------------------------------------

/// The `data` of an `evaluated` event (3.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvaluatedData {
    pub project: String,
    pub backend: String,
    pub decision: String,
    pub question_hash: String,
    pub mode: String,
    /// The map written; null when the fit was at the grid edge.
    pub map_id: Option<String>,
    pub uncalibratable: bool,
    /// The fitted temperature (at the edge too, for the record).
    pub t: f64,
    pub n: usize,
    pub n_tuning: usize,
    pub accuracy: f64,
    pub ece: f64,
    pub ece_interval: [f64; 2],
    pub classwise_ece: BTreeMap<String, f64>,
    pub false_alarm_rate: Option<f64>,
    pub exclusions: Vec<String>,
    pub bars: Bars,
    /// Held-out items asked but not answered, or answered but without a truth.
    pub unanswered: usize,
    /// Accuracy on the held-out owner truths, by `<provenance>/<from_backend>` (3.1).
    pub by_source: BTreeMap<String, SourceAccuracy>,
}

/// What `eval score` recorded.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct ScoreReport {
    pub seq: i64,
    pub repeat: bool,
    pub map: Option<PathBuf>,
    pub evaluated: EvaluatedData,
}

/// Fits on `tuning`, writes the map unless the fit is at the edge, scores `held_out` through
/// it and records the `evaluated` event.
#[allow(clippy::too_many_arguments)]
fn evaluate(
    tx: &mut Tx<'_>,
    root: &Path,
    key: (&str, &str, &str, &str),
    mode: &str,
    kind: AnswerKind,
    allowed: &[String],
    tuning: &[Example],
    held_out: &[Example],
    exclusions: Vec<String>,
    unanswered: usize,
    distinct: Value,
) -> Result<ScoreReport> {
    let (project, backend, decision, qhash) = key;
    let f = fit(kind, tuning);
    let map = (!f.at_edge).then(|| Map {
        project: project.to_string(),
        backend: backend.to_string(),
        decision: decision.to_string(),
        question_hash: qhash.to_string(),
        kind,
        t: f.t,
        fitted_on: tuning.len(),
    });
    let scored: Vec<Example> = held_out
        .iter()
        .map(|e| Example {
            probs: match &map {
                Some(m) => apply_t(kind, &e.probs, m.t),
                None => e.probs.clone(),
            },
            ..e.clone()
        })
        .collect();
    let subject = key_subject(project, backend, decision, qhash);
    let m = metrics(kind, allowed, &scored, seed_of(&subject))?;
    let by_source = by_source(kind, allowed, &scored);
    let fa = m.false_alarm_rate;
    let data = EvaluatedData {
        project: project.to_string(),
        backend: backend.to_string(),
        decision: decision.to_string(),
        question_hash: qhash.to_string(),
        mode: mode.to_string(),
        map_id: map.as_ref().map(Map::id),
        uncalibratable: f.at_edge,
        t: f.t,
        n: m.n,
        n_tuning: tuning.len(),
        accuracy: m.accuracy,
        ece: m.ece,
        ece_interval: [m.ece_interval.0, m.ece_interval.1],
        classwise_ece: m.classwise_ece.into_iter().filter(|(_, v)| v.is_finite()).collect(),
        false_alarm_rate: fa.is_finite().then_some(fa),
        exclusions,
        bars: m.bars,
        unanswered,
        by_source,
    };
    // The map is written before the event that names it, so no event names a missing map.
    let path = match &map {
        Some(m) => {
            let p = map_path(root, m);
            fs::create_dir_all(p.parent().expect("a map has a directory"))?;
            fs::write(&p, serde_json::to_string_pretty(m)? + "\n")?;
            Some(p)
        }
        None => None,
    };
    let r = tx.record_valid(
        Kind::Evaluated,
        &subject,
        distinct,
        serde_json::to_value(&data)?,
        Dedupe::Latest,
    )?;
    Ok(ScoreReport {
        seq: r.event().seq,
        repeat: !r.is_new(),
        map: path,
        evaluated: data,
    })
}

/// `eval score` of a replay (3.2): re-verifies the manifest, fits on the tuning answers,
/// freezes the map, scores the held-out answers and records the `evaluated` event.
pub fn score(
    tx: &mut Tx<'_>,
    root: &Path,
    config: &ConfigSet,
    r: &Replay,
    answers: &[EvalAnswers],
) -> Result<ScoreReport> {
    let cat = Catalogue::embedded();
    check_backend(&r.backend)?;
    let path = manifest_path(
        root,
        &r.project,
        &r.backend,
        &r.decision,
        r.repo.as_deref(),
        EvalStage::Scored,
    );
    let m = load_manifest(&path)?;
    // The file is the request's: its key, and the exclusions it was prepared under.
    let asked = (&r.project, &r.backend, &r.decision, &r.repo, EvalStage::Scored);
    if (&m.project, &m.backend, &m.decision, &m.repo, m.stage) != asked {
        return Err(Error::refused(format!(
            "{} is the manifest of {}/{}/{} (repo {:?}, stage {:?}), not of this request: eval prepare again",
            path.display(),
            m.project,
            m.backend,
            m.decision,
            m.repo,
            m.stage
        )));
    }
    let mut want = r.exclude.clone();
    want.sort();
    want.dedup();
    let mut had = m.exclusions.clone();
    had.sort();
    had.dedup();
    if want != had {
        return Err(Error::refused(format!(
            "the manifest was prepared excluding {had:?} and this score excludes {want:?}: eval prepare again with the same exclusions"
        )));
    }
    let history = history::project_history(root, config, &r.project)?;
    verify(tx.conn(), config, cat, &history, &m)?;
    let answered = checked_answers(cat, config, &m, answers)?;
    let kind = cat.file(&m.decision)?.kind;
    let repo_areas = m
        .questions
        .first()
        .map(|q| {
            let repo = q.alias.rsplit_once('#').map_or(q.alias.as_str(), |(r, _)| r);
            triage::areas_of(&triage::translation_of(config, repo))
        })
        .unwrap_or_default();
    let allowed = cat.allowed(&m.decision, &repo_areas)?;
    let (mut tuning, mut held_out, mut unanswered) = (Vec::new(), Vec::new(), 0);
    for q in &m.questions {
        let Some((_, raw)) = answered.get(&q.id) else {
            if q.split == Split::HeldOut {
                unanswered += 1;
            }
            continue;
        };
        let e = Example {
            item: q.item.clone(),
            probs: raw.clone(),
            truth: q.truth.clone(),
            source: owner_source(&q.provenance, q.from_backend.as_deref()),
        };
        match q.split {
            Split::Tuning => tuning.push(e),
            Split::HeldOut => held_out.push(e),
        }
    }
    let distinct = json!({
        "manifest": m.hash(),
        "answers": hash_value(&json!(answered.iter().map(|(k, (_, p))| (k, p)).collect::<BTreeMap<_, _>>())),
    });
    evaluate(
        tx,
        root,
        (&m.project, &m.backend, &m.decision, &m.question_hash),
        "replay",
        kind,
        &allowed,
        &tuning,
        &held_out,
        m.exclusions.clone(),
        unanswered,
        distinct,
    )
}

/// `eval score --live` (3.2): the recorded suggestions of a closure decision under `backend`,
/// against the verdicts that followed, split by item; the map is fitted on the tuning items.
pub fn score_live(
    tx: &mut Tx<'_>,
    root: &Path,
    config: &ConfigSet,
    project: &str,
    decision: &str,
    backend: &str,
    exclude: &[String],
) -> Result<ScoreReport> {
    let cat = Catalogue::embedded();
    check_backend(backend)?;
    if !CLOSURE_DECISIONS.contains(&decision) {
        return Err(Error::refused(format!(
            "{decision} is replayed (eval prepare), not judged live: only the closure decisions are"
        )));
    }
    let exclusions = exclusions(config, project, exclude)?;
    let excluded: BTreeSet<String> = exclusions.iter().cloned().collect();
    let qhash = cat.question_hash(decision, &[])?;
    let questions = triage::prepared_questions(tx.conn())?;
    let found = truth::suggestions(tx.conn(), project)?;
    let verdicts: BTreeMap<String, truth::ClosureTruth> = truth::closure_truth(tx.conn(), project, &excluded)?
        .into_iter()
        .map(|t| (t.suggestion.clone(), t))
        .collect();
    let (mut tuning, mut held_out) = (Vec::new(), Vec::new());
    for (doc, s) in &found {
        if s.decision != decision || s.body["backend"] != backend {
            continue;
        }
        let Some((q, _)) = s.body["question"].as_str().and_then(|id| questions.get(id)) else {
            continue;
        };
        if q.question_hash != qhash {
            continue;
        }
        let Some(v) = verdicts.get(doc) else { continue };
        let raw = s.body.get("raw").unwrap_or(&s.body["probabilities"]);
        let probs: BTreeMap<String, f64> = serde_json::from_value(raw.clone())?;
        let e = Example {
            item: s.item.clone(),
            probs,
            truth: json!(v.value.to_string()),
            source: owner_source(&v.provenance, v.from_backend.as_deref()),
        };
        if is_tuning(&s.item) {
            tuning.push(e);
        } else {
            held_out.push(e);
        }
    }
    let distinct = json!({"manifest": "live", "answers": hash_value(&json!([&tuning, &held_out]))});
    evaluate(
        tx,
        root,
        (project, backend, decision, &qhash),
        "live",
        AnswerKind::Boolean,
        &cat.allowed(decision, &[])?,
        &tuning,
        &held_out,
        exclusions,
        0,
        distinct,
    )
}

// --- Drift ----------------------------------------------------------------------------------

/// A key's drift (3.5).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Drift {
    /// The owner records counted (at most [`DRIFT_RECORDS`]), since the latest passing eval.
    pub records: usize,
    pub live_accuracy: Option<f64>,
    pub evaluated_accuracy: Option<f64>,
    pub drifting: bool,
}

/// A key's drift (3.5): the agreement over its last [`DRIFT_RECORDS`] `owner_accept` /
/// `owner_edit` records from its backend since its latest passing eval; drifting when that
/// falls more than [`DRIFT_POINTS`] below the eval's accuracy. Fewer records, or no passing
/// eval, is not drifting.
pub fn drift(conn: &Connection, project: &str, backend: &str, decision: &str, hash: &str) -> Result<Drift> {
    let subject = key_subject(project, backend, decision, hash);
    let passing = ledger::events_of(conn, &subject, Kind::Evaluated)?
        .into_iter()
        .rev()
        .find(|e| e.data["bars"]["evaluated"] == true);
    let Some(eval) = passing else {
        return Ok(Drift {
            records: 0,
            live_accuracy: None,
            evaluated_accuracy: None,
            drifting: false,
        });
    };
    let since = parse_time(&eval.time)?;
    let evaluated = eval.data["accuracy"].as_f64();
    let questions = triage::prepared_questions(conn)?;
    let of_key = |b: Option<&str>, at: DateTime<Utc>, q: Option<&String>| {
        b == Some(backend)
            && at > since
            && q.and_then(|q| questions.get(q))
                .is_some_and(|(q, _)| q.question_hash == hash)
    };
    // A closure suggestion is asked only when answered true (3.1), so its verdict agrees when
    // the truth is true.
    let agreements: Vec<(DateTime<Utc>, bool)> = if CLOSURE_DECISIONS.contains(&decision) {
        truth::closure_truth(conn, project, &BTreeSet::new())?
            .into_iter()
            .filter(|t| t.decision == decision && of_key(t.from_backend.as_deref(), t.at, t.question.as_ref()))
            .map(|t| (t.at, t.value))
            .collect()
    } else {
        owner_agreements(conn, project, decision, &of_key)?
    };
    drift_of(agreements, evaluated)
}

/// Whether a verdict (its backend, time and prepared question) is of the drift key.
type OfKey<'a> = dyn Fn(Option<&str>, DateTime<Utc>, Option<&String>) -> bool + 'a;

/// The label and Details decisions' owner records of a key, as (time, agreed).
fn owner_agreements(
    conn: &Connection,
    project: &str,
    decision: &str,
    of_key: &OfKey,
) -> Result<Vec<(DateTime<Utc>, bool)>> {
    Ok(truth::owner_records(conn, project)?
        .into_iter()
        .filter(|r| r.decision == decision && of_key(r.from_backend.as_deref(), r.at, r.question.as_ref()))
        .map(|r| {
            let truth: BTreeSet<String> = match &r.op {
                truth::Op::Set(v) => v.iter().cloned().collect(),
                truth::Op::Add(v) | truth::Op::Remove(v) => BTreeSet::from([v.clone()]),
            };
            (r.at, r.suggested.as_ref().is_some_and(|s| truth_set(s) == truth))
        })
        .collect())
}

/// Drift over the last [`DRIFT_RECORDS`] agreements against the evaluated accuracy.
fn drift_of(mut agreements: Vec<(DateTime<Utc>, bool)>, evaluated: Option<f64>) -> Result<Drift> {
    agreements.sort_by_key(|a| a.0);
    let last: Vec<bool> = agreements.iter().rev().take(DRIFT_RECORDS).map(|a| a.1).collect();
    if last.len() < DRIFT_RECORDS {
        return Ok(Drift {
            records: last.len(),
            live_accuracy: None,
            evaluated_accuracy: evaluated,
            drifting: false,
        });
    }
    let agree = last.iter().filter(|a| **a).count();
    let live = agree as f64 / last.len() as f64;
    Ok(Drift {
        records: last.len(),
        live_accuracy: Some(live),
        evaluated_accuracy: evaluated,
        drifting: evaluated.is_some_and(|a| a - live > DRIFT_POINTS),
    })
}

/// Every key's latest evaluation with its drift: what `eval status` prints.
pub fn status(conn: &Connection, project: &str) -> Result<Vec<Value>> {
    let mut latest: BTreeMap<String, crate::Event> = BTreeMap::new();
    for e in ledger::events_by_kind(conn, Kind::Evaluated)? {
        if e.data["project"] == project {
            latest.insert(e.subject.clone(), e);
        }
    }
    let mut out = Vec::new();
    for e in latest.values() {
        let d: EvaluatedData = serde_json::from_value(e.data.clone())?;
        let drift = drift(conn, project, &d.backend, &d.decision, &d.question_hash)?;
        out.push(json!({"key": e.subject, "time": e.time, "evaluated": d, "drift": drift}));
    }
    Ok(out)
}
