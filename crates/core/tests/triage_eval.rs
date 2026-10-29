//! Triage phase 2/5, eval (triage spec 6.1's rows for 2.4 and 3): the calibration fit, ECE,
//! its interval and the bars, the false-alarm rate, the split, provenance over a paginated
//! history, owner and closure truth, replay prepare and score with the map in force, area's
//! truth-set pairs, the replay cutoff, and drift.
//!
//! The history is real: `tests/fixtures/history-replay` holds airzoned's issues read five to a
//! page and rhai's filtered search, recorded from GitHub (`history-replay-meta.json`).

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use chrono::{DateTime, Duration, Utc};
use common::fixture::*;
use common::triage::*;
use common::*;
use issue_dashboard_core::config::ConfigSet;
use issue_dashboard_core::decision;
use issue_dashboard_core::docs::{Action, CloseReason, CloseValue, Decision, DecisionValue, LaunchKind, Target};
use issue_dashboard_core::eval::{self, Bars, EvalAnswers, EvalStage, Example, Manifest, Replay as EvalReplay, Split};
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::github::{GitHub, Replay};
use issue_dashboard_core::history::{self, HistoryComment, HistoryItem, LabelChange, RepoHistory};
use issue_dashboard_core::root::{format_time, parse_time};
use issue_dashboard_core::translate::Translation;
use issue_dashboard_core::triage::{AnswerKind, Catalogue, Stage};
use issue_dashboard_core::truth::{self, Op, Record, Tagged};
use issue_dashboard_core::{Error, Result, ledger};
use serde_json::{Value, json};

/// The clock the history recording was made at.
const NOW: &str = "2026-10-29T12:00:00Z";
const HISTORY_PAGE: u32 = 5;

fn now() -> DateTime<Utc> {
    parse_time(NOW).unwrap()
}

fn close(a: f64, b: f64) -> bool {
    (a - b).abs() < 1e-9
}

fn ex(item: &str, probs: &[(&str, f64)], truth: Value) -> Example {
    Example {
        item: item.to_string(),
        probs: probs.iter().map(|(k, v)| (k.to_string(), *v)).collect(),
        truth,
        source: None,
    }
}

fn strings(xs: &[&str]) -> Vec<String> {
    xs.iter().map(|s| s.to_string()).collect()
}

// --- ECE and bars (3.4) ---------------------------------------------------------------------

#[test]
fn ece_of_the_two_bin_fixture_is_seven_points() {
    // Bin 9: two at 0.95, both right (|1 − 0.95| = 0.05); bin 5: eight at 0.575, four right
    // (|0.5 − 0.575| = 0.075). ECE = ⅕·0.05 + ⅘·0.075 = 0.07; unweighted bins would give 0.0625.
    let mut pts = vec![(0.95, true), (0.95, true)];
    pts.extend((0..8).map(|i| (0.575, i < 4)));
    assert!(close(eval::ece(&pts), 0.07), "{}", eval::ece(&pts));
}

#[test]
fn a_one_item_bin_counts_by_its_share_and_empty_bins_are_skipped() {
    assert!(close(eval::ece(&[(0.35, true)]), 0.65));
    // 1.0 is in the last bin, which is closed: one bin, ā 0.5, c̄ 0.975.
    assert!(close(eval::ece(&[(1.0, true), (0.95, false)]), 0.475));
    // A one-item bin beside a four-item one: ⅕·|0 − 0.15| + ⅘·|1 − 0.85|.
    let pts = [(0.15, false), (0.85, true), (0.85, true), (0.85, true), (0.85, true)];
    assert!(close(eval::ece(&pts), 0.03 + 0.12), "{}", eval::ece(&pts));
    assert!(eval::ece(&[]).is_nan());
}

fn interval_fixture() -> Vec<Example> {
    (0..40)
        .map(|i| {
            let right = i % 4 != 0;
            let truth = if right { "bug" } else { "todo" };
            ex(
                &format!("I_{i}"),
                &[
                    ("bug", 0.6 + (i % 5) as f64 * 0.08),
                    ("todo", 0.4 - (i % 5) as f64 * 0.08),
                ],
                json!(truth),
            )
        })
        .collect()
}

#[test]
fn the_same_seed_gives_the_same_interval_and_another_seed_another() {
    let e = interval_fixture();
    let allowed = strings(&["bug", "todo"]);
    let a = eval::ece_interval(AnswerKind::Choice, &allowed, &e, 7);
    let b = eval::ece_interval(AnswerKind::Choice, &allowed, &e, 7);
    let c = eval::ece_interval(AnswerKind::Choice, &allowed, &e, 8);
    assert_eq!(a, b);
    assert_ne!(a, c);
    assert!(a.0 <= a.1 && a.0 >= 0.0, "{a:?}");
    // A key's seed is its subject's hash, the same on every run.
    assert_eq!(
        eval::seed_of("home/m@e/type/3c9e1a7b2d4f"),
        eval::seed_of("home/m@e/type/3c9e1a7b2d4f")
    );
}

#[test]
fn the_bars_flip_at_100_examples_ece_0_10_and_85_percent() {
    let b = |n, e, a| eval::bars(n, e, a);
    assert_eq!(
        b(99, 0.05, 0.95),
        Bars {
            evaluated: false,
            bulk: false
        }
    );
    assert_eq!(
        b(100, 0.05, 0.95),
        Bars {
            evaluated: true,
            bulk: true
        }
    );
    assert_eq!(
        b(100, 0.10, 0.85),
        Bars {
            evaluated: true,
            bulk: true
        }
    );
    assert_eq!(
        b(100, 0.1001, 0.95),
        Bars {
            evaluated: false,
            bulk: false
        }
    );
    assert_eq!(
        b(100, 0.05, 0.8499),
        Bars {
            evaluated: true,
            bulk: false
        }
    );
}

// --- Gate checks (3.4) ----------------------------------------------------------------------

#[test]
fn stated_0_1_answers_true_40_percent_of_the_time_are_a_0_4_false_alarm_rate() {
    let e: Vec<Example> = (0..10)
        .map(|i| {
            ex(
                &format!("I_{i}"),
                &[("true", 0.1), ("false", 0.9)],
                json!(if i < 4 { "true" } else { "false" }),
            )
        })
        .collect();
    assert!(close(eval::false_alarm_rate(AnswerKind::Boolean, &e), 0.4));
}

#[test]
fn area_s_false_alarms_are_over_the_truth_set_s_pairs() {
    let e = [
        // Two truth pairs, kernel given 0.1: one alarm. renderer's 0.05 is no pair.
        ex(
            "I_1",
            &[("kernel", 0.1), ("sdl", 0.9), ("renderer", 0.05)],
            json!(["kernel", "sdl"]),
        ),
        // One pair, given 0.15: an alarm. sdl and renderer are no pairs, whatever they are given.
        ex(
            "I_2",
            &[("kernel", 0.15), ("sdl", 0.6), ("renderer", 0.5)],
            json!(["kernel"]),
        ),
    ];
    assert!(close(eval::false_alarm_rate(AnswerKind::Areas, &e), 2.0 / 3.0));
}

// --- Calibration (2.4) ----------------------------------------------------------------------

/// Boolean examples whose true rate is `q` while the backend states p with p^(1/t) ∝ q.
fn tempered(t: f64, q: f64, n: usize, offset: usize) -> Vec<Example> {
    let (a, b) = (q.powf(t), (1.0 - q).powf(t));
    let p = a / (a + b);
    let right = (q * n as f64).round() as usize;
    (0..n)
        .map(|i| {
            ex(
                &format!("I_{}", offset + i),
                &[("true", p), ("false", 1.0 - p)],
                json!(if i < right { "true" } else { "false" }),
            )
        })
        .collect()
}

#[test]
fn a_fit_on_answers_tempered_at_a_known_t_finds_it_within_one_grid_step() {
    for t in [0.6, 1.0, 2.0, 3.1] {
        let mut e = tempered(t, 0.8, 100, 0);
        e.extend(tempered(t, 0.65, 100, 100));
        let f = eval::fit(AnswerKind::Boolean, &e);
        assert!((f.t - t).abs() <= 0.05 + 1e-9, "T {t}: fitted {}", f.t);
        assert!(!f.at_edge);
    }
    // Calibrating a tempered answer by its T gives back the true rate.
    let e = tempered(2.0, 0.8, 10, 0);
    assert!(close(eval::apply_t(AnswerKind::Boolean, &e[0].probs, 2.0)["true"], 0.8));
}

#[test]
fn a_fit_at_either_end_of_the_grid_is_uncalibratable() {
    // Always right at 0.6: wants T below 0.25.
    let under: Vec<Example> = (0..20)
        .map(|i| ex(&format!("I_{i}"), &[("true", 0.6), ("false", 0.4)], json!("true")))
        .collect();
    let f = eval::fit(AnswerKind::Boolean, &under);
    assert_eq!((f.t, f.at_edge), (0.25, true));
    // Right half the time at 0.99: wants T above 4.
    let over: Vec<Example> = (0..20)
        .map(|i| {
            ex(
                &format!("I_{i}"),
                &[("true", 0.99), ("false", 0.01)],
                json!(if i % 2 == 0 { "true" } else { "false" }),
            )
        })
        .collect();
    let f = eval::fit(AnswerKind::Boolean, &over);
    assert_eq!((f.t, f.at_edge), (4.0, true));
    assert_eq!(eval::grid().len(), 76);
}

#[test]
fn area_calibration_keeps_the_chosen_set_and_the_order() {
    let raw: BTreeMap<String, f64> = [("kernel", 0.7), ("sdl", 0.5), ("renderer", 0.2), ("drivers", 0.45)]
        .iter()
        .map(|(k, v)| (k.to_string(), *v))
        .collect();
    for t in [0.25, 0.8, 1.7, 4.0] {
        let q = eval::apply_t(AnswerKind::Areas, &raw, t);
        for a in raw.keys() {
            assert_eq!(q[a] >= 0.5, raw[a] >= 0.5, "{a} at T {t}");
        }
        assert!(q["kernel"] > q["drivers"] && q["drivers"] > q["renderer"]);
    }
}

// --- Split (3.4) ----------------------------------------------------------------------------

#[test]
fn the_split_is_forty_percent_by_hash_and_adding_items_moves_none() {
    // Literals: sha256 of the id, first 15 hex over 16^15.
    assert!(eval::is_tuning("I_kwDOAbCdEf5xYz34")); // 0.0768
    assert!(!eval::is_tuning("I_kwDOissueA")); // 0.556
    assert!(!eval::is_tuning("I_kwDOissueB")); // 0.659
    assert!(eval::is_tuning("I_kwDOissueC")); // 0.0866
    let ids: Vec<String> = (0..2000).map(|i| format!("I_kwDO{i:06}")).collect();
    let share = ids.iter().filter(|i| eval::is_tuning(i)).count() as f64 / ids.len() as f64;
    assert!((0.37..0.43).contains(&share), "{share}");
}

// --- History and provenance (3.1, 3.2) ------------------------------------------------------

fn history_gh() -> Replay {
    Replay::load(&fixtures().join("history-replay")).unwrap()
}

/// `history_gh`, failing the `n`th issues page (1-based) with a network error.
struct FailingPage {
    inner: Replay,
    n: usize,
    seen: Arc<AtomicUsize>,
}

impl GitHub for FailingPage {
    fn graphql(&self, req: &issue_dashboard_core::github::Request) -> Result<Value> {
        if req.name == "history_issues" && self.seen.fetch_add(1, Ordering::SeqCst) + 1 == self.n {
            return Err(Error::GitHub("connection reset".into()));
        }
        self.inner.graphql(req)
    }
}

#[test]
fn history_reads_every_page_and_an_interrupted_read_resumes_from_its_cursor() {
    let whole = fixture_root();
    let r = history::refresh_repo(whole.path(), &history_gh(), AIRZONED, None, 730, now(), HISTORY_PAGE).unwrap();
    assert_eq!((r.complete, r.items, r.requests, r.closed_in_window), (true, 44, 9, 30));
    let all = history::load(whole.path(), AIRZONED).unwrap().unwrap();
    assert!(all.cursor.is_none());

    let t = fixture_root();
    let failing = FailingPage {
        inner: history_gh(),
        n: 4,
        seen: Arc::new(AtomicUsize::new(0)),
    };
    let r = history::refresh_repo(t.path(), &failing, AIRZONED, None, 730, now(), HISTORY_PAGE).unwrap();
    assert_eq!((r.complete, r.items, r.requests), (false, 15, 3));
    assert!(r.error.as_deref().unwrap().contains("connection reset"));
    let part = history::load(t.path(), AIRZONED).unwrap().unwrap();
    assert!(matches!(
        part.cursor,
        Some(history::Cursor::Connection { after: Some(_) })
    ));
    // An incomplete history is never evaluated.
    history::refresh_repo(
        t.path(),
        &history_gh(),
        RHAI,
        Some("author:yuvalrakavy"),
        730,
        now(),
        HISTORY_PAGE,
    )
    .unwrap();
    history::save(
        t.path(),
        &RepoHistory {
            repo: STORE.into(),
            complete: true,
            cursor: None,
            items: Vec::new(),
            ..part.clone()
        },
    )
    .unwrap();
    let c = ConfigSet::load(t.path()).unwrap();
    let m = refused(history::project_history(t.path(), &c, "home"));
    assert!(m.contains("history of yuvalrakavy/airzoned is incomplete"), "{m}");

    let r = history::refresh_repo(t.path(), &history_gh(), AIRZONED, None, 730, now(), HISTORY_PAGE).unwrap();
    assert_eq!((r.complete, r.resumed, r.items, r.requests), (true, true, 44, 9));
    let resumed = history::load(t.path(), AIRZONED).unwrap().unwrap();
    assert_eq!(resumed.items, all.items);
}

#[test]
fn a_filtered_member_s_history_is_its_search_s_issues() {
    let t = fixture_root();
    let r = history::refresh_repo(
        t.path(),
        &history_gh(),
        RHAI,
        Some("author:yuvalrakavy"),
        730,
        now(),
        HISTORY_PAGE,
    )
    .unwrap();
    assert_eq!((r.complete, r.items, r.requests), (true, 1, 2));
    let h = history::load(t.path(), RHAI).unwrap().unwrap();
    assert_eq!(h.items[0].author, "yuvalrakavy");
}

/// airzoned's history as recorded.
fn airzoned() -> RepoHistory {
    let t = fixture_root();
    history::refresh_repo(t.path(), &history_gh(), AIRZONED, None, 730, now(), HISTORY_PAGE).unwrap();
    history::load(t.path(), AIRZONED).unwrap().unwrap()
}

fn item_of(h: &RepoHistory, number: u32) -> &HistoryItem {
    let alias = format!("{}#{number}", h.repo);
    h.items.iter().find(|i| i.alias == alias).unwrap()
}

fn plus(at: &str, minutes: i64, seconds: i64) -> String {
    format_time(parse_time(at).unwrap() + Duration::minutes(minutes) + Duration::seconds(seconds))
}

#[test]
fn a_label_set_by_9_59_after_filing_is_filed_and_by_10_01_github_later() {
    let h = airzoned();
    let mut item = item_of(&h, 2).clone();
    let created = item.created_at.clone();
    let none = BTreeMap::new();
    let mut e = item.label_events[0].clone();
    let at = |e: &mut LabelChange, m, s| e.at = plus(&created, m, s);
    for (m, s, want) in [(9, 59, "filed"), (10, 0, "filed"), (10, 1, "github_later")] {
        at(&mut e, m, s);
        assert_eq!(truth::label_provenance(&item, &e, &none).unwrap(), want, "{m}:{s:02}");
    }
    // A removal is never filed; a tagged event is its review's.
    e.added = false;
    at(&mut e, 1, 0);
    assert_eq!(truth::label_provenance(&item, &e, &none).unwrap(), "github_later");
    let tags = BTreeMap::from([(e.id.clone(), "oct-review".to_string())]);
    assert_eq!(
        truth::label_provenance(&item, &e, &tags).unwrap(),
        "bulk_review:oct-review"
    );
    item.label_events = vec![e];
}

fn store_translation() -> Translation {
    Translation::for_repo(&ConfigSet::load(&fixtures().join("config")).unwrap(), STORE)
}

fn label(id: &str, name: &str, added: bool, at: &str) -> LabelChange {
    LabelChange {
        id: id.to_string(),
        label: name.to_string(),
        added,
        at: at.to_string(),
        actor: "yuvalrakavy".to_string(),
    }
}

#[test]
fn area_adds_and_removes_fold_into_the_set_left_at_the_end() {
    let h = airzoned();
    let mut item = item_of(&h, 2).clone();
    let c = item.created_at.clone();
    item.label_events = vec![
        label("LE_1", "kernel", true, &plus(&c, 1, 0)),
        label("LE_2", "sdl", true, &plus(&c, 1, 0)),
        label("LE_3", "kernel", false, &plus(&c, 60, 0)),
        label("LE_4", "renderer", true, &plus(&c, 90, 0)),
    ];
    let recs = truth::label_records(&store_translation(), &item, &BTreeMap::new()).unwrap();
    let t = truth::fold("area", &recs, &BTreeSet::new()).unwrap();
    assert_eq!(t.value, json!(["renderer", "sdl"]));
    assert_eq!(t.provenance, "github_later");
    // Without the later events, only the filed set is left.
    let filed_only = truth::fold("area", &recs, &BTreeSet::from(["github_later".to_string()])).unwrap();
    assert_eq!(
        (filed_only.value, filed_only.provenance.as_str()),
        (json!(["kernel", "sdl"]), "filed")
    );
}

#[test]
fn swapping_two_labels_of_one_type_keeps_the_type_in_either_order() {
    // airzoned#4, as GitHub recorded it: enhancement (todo) removed and todo added in one second.
    let h = airzoned();
    let item = item_of(&h, 4).clone();
    let t = Translation::for_repo(&ConfigSet::load(&fixtures().join("config")).unwrap(), AIRZONED);
    let none = BTreeMap::new();
    let fold = |item: &HistoryItem| {
        truth::fold(
            "type",
            &truth::label_records(&t, item, &none).unwrap(),
            &BTreeSet::new(),
        )
    };
    assert_eq!(fold(&item).unwrap().value, json!("todo"));
    let mut swapped = item.clone();
    let (i, j) = (
        swapped
            .label_events
            .iter()
            .position(|e| e.label == "enhancement" && !e.added)
            .unwrap(),
        swapped.label_events.iter().position(|e| e.label == "todo").unwrap(),
    );
    swapped.label_events.swap(i, j);
    assert_eq!(fold(&swapped).unwrap().value, json!("todo"));
}

// --- Replay prepare and score (3.2, 2.4) ----------------------------------------------------

/// A fixture root holding the recorded histories of airzoned and rhai and `store` as Store's
/// (Store's 730 days are not recorded), and a ledger.
fn with_history(store: Vec<HistoryItem>) -> TempRoot {
    let t = fixture_root();
    history::refresh_repo(t.path(), &history_gh(), AIRZONED, None, 730, now(), HISTORY_PAGE).unwrap();
    history::refresh_repo(
        t.path(),
        &history_gh(),
        RHAI,
        Some("author:yuvalrakavy"),
        730,
        now(),
        HISTORY_PAGE,
    )
    .unwrap();
    let mut s = history::load(t.path(), AIRZONED).unwrap().unwrap();
    s.repo = STORE.to_string();
    s.items = store;
    history::save(t.path(), &s).unwrap();
    t.root.mutate(now(), |_| Ok(())).unwrap();
    t
}

fn replay(decision: &str) -> EvalReplay {
    EvalReplay {
        project: "home".into(),
        decision: decision.into(),
        backend: BACKEND.into(),
        exclude: Vec::new(),
        repo: None,
    }
}

fn prepare_eval(t: &TempRoot, r: &EvalReplay) -> Result<eval::PrepareReport> {
    let c = ConfigSet::load(t.path()).unwrap();
    let out = t.path().join("eval-out");
    t.root
        .read(|l| eval::prepare(l.conn(), t.path(), &c, r, EvalStage::Scored, &[], &out))
}

fn manifest_of(p: &eval::PrepareReport) -> Manifest {
    serde_json::from_str(&fs::read_to_string(&p.manifest).unwrap()).unwrap()
}

/// Answers to every question of `m`: the truth at `right` when `ok(i)`, else the next answer
/// at `right`, the rest sharing what is left.
fn answers(m: &Manifest, right: f64, ok: impl Fn(usize) -> bool) -> EvalAnswers {
    let cat = Catalogue::embedded();
    let allowed = cat.allowed(&m.decision, &[]).unwrap();
    let answers = m
        .questions
        .iter()
        .enumerate()
        .map(|(i, q)| {
            let truth = q.truth.as_str().unwrap();
            let at = allowed.iter().position(|a| a == truth).unwrap();
            let top = if ok(i) { at } else { (at + 1) % allowed.len() };
            let rest = (1.0 - right) / (allowed.len() - 1) as f64;
            let probabilities: BTreeMap<String, f64> = allowed
                .iter()
                .enumerate()
                .map(|(k, a)| (a.clone(), if k == top { right } else { rest }))
                .collect();
            serde_json::from_value(json!({"question": q.id, "probabilities": probabilities, "reason": "r"})).unwrap()
        })
        .collect();
    EvalAnswers {
        backend: m.backend.clone(),
        answers,
    }
}

fn score(t: &TempRoot, r: &EvalReplay, a: &EvalAnswers) -> Result<eval::ScoreReport> {
    let c = ConfigSet::load(t.path()).unwrap();
    t.root
        .mutate(now(), |tx| eval::score(tx, t.path(), &c, r, std::slice::from_ref(a)))
}

fn type_hash() -> String {
    Catalogue::embedded().question_hash("type", &[]).unwrap()
}

#[test]
fn prepare_writes_blinded_question_files_split_by_item_and_a_manifest_of_the_truths() {
    let t = with_history(Vec::new());
    let p = prepare_eval(&t, &replay("type")).unwrap();
    let m = manifest_of(&p);
    // airzoned's 21 closed issues with a type label (of its 30 closed) and rhai's one.
    assert_eq!(p.available["tuning"] + p.available["heldout"], 22, "{p:?}");
    assert_eq!(m.questions.len(), 22);
    assert_eq!(m.question_hash, type_hash());
    for q in &m.questions {
        assert_eq!(q.split == Split::Tuning, eval::is_tuning(&q.item), "{}", q.alias);
    }
    let files: Vec<String> = ["tuning", "heldout"]
        .iter()
        .map(|s| fs::read_to_string(&p.files[*s]).unwrap())
        .collect();
    for f in &files {
        let v: Value = serde_json::from_str(f).unwrap();
        for q in v["questions"].as_array().unwrap() {
            assert!(q.get("truth").is_none() && q.get("provenance").is_none());
            let labels = q["input"]["labels"].as_array().unwrap();
            assert!(
                labels
                    .iter()
                    .all(|l| !["bug", "todo", "enhancement", "p1", "p2", "p3"].contains(&l.as_str().unwrap())),
                "{labels:?}"
            );
        }
    }
    let n: usize = files
        .iter()
        .map(|f| {
            serde_json::from_str::<Value>(f).unwrap()["questions"]
                .as_array()
                .unwrap()
                .len()
        })
        .sum();
    assert_eq!(n, 22);
}

#[test]
fn adding_items_to_history_moves_no_question_between_tuning_and_held_out() {
    let t = with_history(Vec::new());
    let before = manifest_of(&prepare_eval(&t, &replay("type")).unwrap());
    // Twelve more closed typed issues: airzoned's own, filed again under new ids.
    let mut h = history::load(t.path(), AIRZONED).unwrap().unwrap();
    let asked: BTreeSet<&str> = before.questions.iter().map(|q| q.item.as_str()).collect();
    let extra: Vec<HistoryItem> = h
        .items
        .iter()
        .filter(|i| asked.contains(i.id.as_str()))
        .take(12)
        .enumerate()
        .map(|(k, i)| HistoryItem {
            id: format!("{}x{k}", i.id),
            alias: format!("{AIRZONED}#{}", 900 + k),
            ..i.clone()
        })
        .collect();
    h.items.extend(extra);
    history::save(t.path(), &h).unwrap();
    let after = manifest_of(&prepare_eval(&t, &replay("type")).unwrap());
    assert_eq!(after.questions.len(), before.questions.len() + 12);
    let split: BTreeMap<&str, Split> = after.questions.iter().map(|q| (q.item.as_str(), q.split)).collect();
    for q in &before.questions {
        assert_eq!(split[q.item.as_str()], q.split, "{}", q.alias);
    }
}

#[test]
fn score_fits_on_tuning_freezes_the_map_and_names_it_in_force() {
    let t = with_history(Vec::new());
    let r = replay("type");
    let m = manifest_of(&prepare_eval(&t, &r).unwrap());
    // Two in three right at 0.6: a little underconfident, inside the grid.
    let s = score(&t, &r, &answers(&m, 0.6, |i| i % 3 != 2)).unwrap();
    let d = &s.evaluated;
    assert!(!d.uncalibratable && d.t > 0.25 && d.t < 4.0, "{d:?}");
    let map_id = d.map_id.clone().unwrap();
    let map: Value = serde_json::from_str(&fs::read_to_string(s.map.as_ref().unwrap()).unwrap()).unwrap();
    assert_eq!(map["t"], json!(d.t));
    assert!(
        s.map
            .as_ref()
            .unwrap()
            .ends_with(format!("type-{}-{map_id}.json", type_hash()))
    );
    let held = m.questions.iter().filter(|q| q.split == Split::HeldOut).count();
    assert_eq!((d.n, d.n_tuning, d.unanswered), (held, m.questions.len() - held, 0));
    assert!(!d.bars.evaluated, "under 100 held-out examples");
    let force = t
        .root
        .read(|l| eval::map_in_force(l.conn(), "home", BACKEND, "type", &type_hash()))
        .unwrap();
    assert_eq!(force.as_deref(), Some(map_id.as_str()));
    let rec = t
        .root
        .read(|l| ledger::events_by_kind(l.conn(), Kind::Evaluated))
        .unwrap();
    assert_eq!(rec.len(), 1);
    assert_eq!(rec[0].subject, format!("home/{BACKEND}/type/{}", type_hash()));
    // The same answers again are a repeat, not a second evaluation.
    assert!(score(&t, &r, &answers(&m, 0.6, |i| i % 3 != 2)).unwrap().repeat);
}

#[test]
fn a_fit_at_the_grid_edge_records_uncalibratable_writes_no_map_and_ends_the_map_in_force() {
    let t = with_history(Vec::new());
    let r = replay("type");
    let m = manifest_of(&prepare_eval(&t, &r).unwrap());
    let first = score(&t, &r, &answers(&m, 0.6, |i| i % 3 != 2)).unwrap();
    assert!(first.evaluated.map_id.is_some());
    // Always right at 0.6: the fit wants T below the grid.
    let s = score(&t, &r, &answers(&m, 0.6, |_| true)).unwrap();
    assert_eq!(
        (s.evaluated.uncalibratable, s.evaluated.t, &s.evaluated.map_id),
        (true, 0.25, &None)
    );
    assert!(s.map.is_none());
    let maps = fs::read_dir(eval::key_dir(t.path(), "home", BACKEND))
        .unwrap()
        .filter(|e| {
            let name = e.as_ref().unwrap().file_name().to_string_lossy().into_owned();
            name.starts_with("type-") && !name.contains("manifest")
        })
        .count();
    assert_eq!(maps, 1, "only the first evaluation's map");
    let force = t
        .root
        .read(|l| eval::map_in_force(l.conn(), "home", BACKEND, "type", &type_hash()))
        .unwrap();
    assert_eq!(force, None, "the latest evaluated event names no map");
}

#[test]
fn score_refuses_answers_once_a_truth_changes_under_the_manifest() {
    let t = with_history(Vec::new());
    let r = replay("type");
    let m = manifest_of(&prepare_eval(&t, &r).unwrap());
    let h = history::load(t.path(), AIRZONED).unwrap().unwrap();
    let q = m.questions.iter().find(|q| q.alias.starts_with(AIRZONED)).unwrap();
    let item = h.items.iter().find(|i| i.id == q.item).unwrap();
    let e = &item.label_events[0];
    let tagged = Tagged {
        event: e.id.clone(),
        item: item.id.clone(),
        alias: item.alias.clone(),
        label: e.label.clone(),
        added: e.added,
        at: e.at.clone(),
    };
    t.root
        .mutate(now(), |tx| {
            truth::record_tags(tx, "home", "oct-review", "type", &[tagged])
        })
        .unwrap();
    let m2 = refused(score(&t, &r, &answers(&m, 0.6, |i| i % 3 != 2)));
    assert!(m2.contains("no longer matches history and the ledger"), "{m2}");
    assert!(
        t.root
            .read(|l| ledger::events_by_kind(l.conn(), Kind::Evaluated))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn an_answer_to_no_question_of_the_manifest_or_from_another_backend_refuses_the_files() {
    let t = with_history(Vec::new());
    let r = replay("type");
    let m = manifest_of(&prepare_eval(&t, &r).unwrap());
    let mut a = answers(&m, 0.6, |_| true);
    a.answers[0].question = "q-00000000000000000000".into();
    let msg = refused(score(&t, &r, &a));
    assert!(
        msg.contains("q-00000000000000000000: the manifest has no such question"),
        "{msg}"
    );
    let mut a = answers(&m, 0.6, |_| true);
    a.backend = "claude-sonnet-5-5@low".into();
    let msg = refused(score(&t, &r, &a));
    assert!(
        msg.contains("but the manifest measures claude-opus-5-5@medium"),
        "{msg}"
    );
}

#[test]
fn an_excluded_provenance_s_truths_are_not_asked() {
    let t = with_history(Vec::new());
    let mut r = replay("type");
    r.exclude = vec!["filed".into()];
    let p = prepare_eval(&t, &r).unwrap();
    let m = manifest_of(&p);
    assert_eq!(m.exclusions, ["filed"]);
    assert!(m.questions.iter().all(|q| q.provenance != "filed"));
    assert!(m.questions.len() < 22, "airzoned's issues are mostly typed at filing");
}

// --- Replay cutoff (1.1) and area's eval (3.4) ----------------------------------------------

fn comment(at: &str, body: &str) -> HistoryComment {
    HistoryComment {
        author: "yuvalrakavy".into(),
        association: "OWNER".into(),
        at: at.into(),
        body: body.into(),
    }
}

/// airzoned#2 as a Store issue: its id and alias changed, `events` its label events.
fn store_item(base: &HistoryItem, n: u32, events: Vec<LabelChange>, comments: Vec<HistoryComment>) -> HistoryItem {
    HistoryItem {
        id: format!("{}s{n}", base.id),
        alias: format!("{STORE}#{n}"),
        labels: Vec::new(),
        label_events: events,
        comments,
        ..base.clone()
    }
}

#[test]
fn a_comment_after_the_item_s_priority_label_is_absent_from_its_type_input() {
    let h = airzoned();
    let base = item_of(&h, 2);
    let c = base.created_at.clone();
    let item = store_item(
        base,
        700,
        vec![
            label("LE_p", "p2", true, &plus(&c, 24 * 60, 0)),
            label("LE_t", "bug", true, &plus(&c, 3 * 24 * 60, 0)),
        ],
        vec![
            comment(&plus(&c, 12 * 60, 0), "before-every-label"),
            comment(&plus(&c, 2 * 24 * 60, 0), "after-the-priority-label"),
        ],
    );
    let t = with_history(vec![item.clone()]);
    let mut r = replay("type");
    r.repo = Some(STORE.into());
    let p = prepare_eval(&t, &r).unwrap();
    let file = fs::read_to_string(&p.files[if eval::is_tuning(&item.id) { "tuning" } else { "heldout" }]).unwrap();
    assert!(file.contains("before-every-label"), "{file}");
    assert!(!file.contains("after-the-priority-label"), "{file}");
}

#[test]
fn area_s_eval_leaves_out_an_item_whose_folded_areas_are_empty() {
    let h = airzoned();
    let base = item_of(&h, 2);
    let c = base.created_at.clone();
    let kept = store_item(
        base,
        701,
        vec![label("LE_a", "kernel", true, &plus(&c, 1, 0))],
        Vec::new(),
    );
    let emptied = store_item(
        base,
        702,
        vec![
            label("LE_b", "kernel", true, &plus(&c, 1, 0)),
            label("LE_c", "kernel", false, &plus(&c, 60, 0)),
        ],
        Vec::new(),
    );
    let t = with_history(vec![kept.clone(), emptied.clone()]);
    let mut r = replay("area");
    r.repo = Some(STORE.into());
    let m = manifest_of(&prepare_eval(&t, &r).unwrap());
    let items: Vec<&str> = m.questions.iter().map(|q| q.item.as_str()).collect();
    assert_eq!(items, [kept.id.as_str()]);
    assert_eq!(m.questions[0].truth, json!(["kernel"]));
}

#[test]
fn area_is_refused_across_repos_that_ask_it_differently() {
    let h = airzoned();
    let base = item_of(&h, 2);
    let c = base.created_at.clone();
    let store = store_item(
        base,
        703,
        vec![label("LE_a", "kernel", true, &plus(&c, 1, 0))],
        Vec::new(),
    );
    let mut rhai_like = store_item(base, 704, vec![label("LE_v", "vm", true, &plus(&c, 1, 0))], Vec::new());
    rhai_like.alias = format!("{RHAI}#704");
    let t = with_history(vec![store]);
    let mut rh = history::load(t.path(), RHAI).unwrap().unwrap();
    rh.items.push(rhai_like);
    history::save(t.path(), &rh).unwrap();
    let m = refused(prepare_eval(&t, &replay("area")));
    assert!(m.contains("area is asked differently in 2 repos"), "{m}");
    assert!(m.contains("evaluate one with --repo"), "{m}");
}

// --- Owner and closure truth (3.1) ----------------------------------------------------------

fn decided(t: &TempRoot, d: &Decision) {
    t.root
        .mutate(parse_time(&d.created_at).unwrap(), |tx| {
            decision::ingest(tx, "home", OWNER, d)
        })
        .unwrap();
}

/// The document id of the latest `decision` suggestion about `item`.
fn suggestion_doc(t: &TempRoot, item: &str, decision: &str) -> String {
    let found = t.root.read(|l| truth::suggestions(l.conn(), "home")).unwrap();
    found
        .iter()
        .filter(|(_, s)| s.item == item && s.decision == decision)
        .max_by_key(|(_, s)| s.at)
        .map(|(d, _)| d.clone())
        .unwrap_or_else(|| panic!("no {decision} suggestion of {item}"))
}

fn on_suggestion(id: &str, action: Action, doc: &str, minutes: i64, value: Option<DecisionValue>) -> Decision {
    let mut d = decision(
        id,
        OWNER,
        action,
        Target {
            suggestion: Some(doc.into()),
            ..Default::default()
        },
        value,
    );
    d.created_at = format_time(at(minutes));
    d
}

/// A triage launch over the tracked items, prepared and answered with `answer_all` (type bug
/// at 0.7, closures false).
fn triaged() -> Tracked {
    let tr = tracked();
    launched(&tr.t, "T1", LaunchKind::Triage, &tr.items, 2);
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    record(&tr.t, "T1", answer_all(&p), 4).unwrap();
    tr
}

fn owner_type_records(t: &TempRoot) -> Vec<Record> {
    t.root
        .read(|l| truth::owner_records(l.conn(), "home"))
        .unwrap()
        .into_iter()
        .filter(|r| r.decision == "type")
        .collect()
}

#[test]
fn an_apply_all_accept_is_never_truth_and_an_undone_accept_leaves_no_record() {
    let tr = triaged();
    let (a, b) = (tr.items[0].0.clone(), tr.items[1].0.clone());
    let bulk = DecisionValue {
        bulk: Some(true),
        ..Default::default()
    };
    decided(
        &tr.t,
        &on_suggestion(
            "d-bulk",
            Action::AcceptSuggestion,
            &suggestion_doc(&tr.t, &a, "type"),
            10,
            Some(bulk),
        ),
    );
    assert!(owner_type_records(&tr.t).is_empty(), "owner_bulk");
    decided(
        &tr.t,
        &on_suggestion(
            "d-one",
            Action::AcceptSuggestion,
            &suggestion_doc(&tr.t, &b, "type"),
            11,
            None,
        ),
    );
    let recs = owner_type_records(&tr.t);
    assert_eq!(recs.len(), 1);
    assert_eq!(
        (
            recs[0].item.as_str(),
            recs[0].provenance.as_str(),
            &recs[0].op,
            recs[0].from_backend.as_deref()
        ),
        (b.as_str(), "owner_accept", &Op::Set(vec!["bug".into()]), Some(BACKEND))
    );
    let mut u = undo("d-undo", "d-one");
    u.created_at = format_time(at(12));
    decided(&tr.t, &u);
    assert!(owner_type_records(&tr.t).is_empty(), "undone");
}

/// A closures launch on the first tracked item answered likely_fixed and obsolete true, and
/// duplicate true naming the second tracked item.
fn closures_suggested() -> Tracked {
    let tr = tracked();
    launched(&tr.t, "R1", LaunchKind::RecommendClosures, &tr.items[..1], 2);
    let p = prepare(&tr.t, "R1", BACKEND, Stage::First, 3).unwrap();
    let mut file = answer_all(&p);
    for a in file["answers"].as_array_mut().unwrap() {
        let q = questions(&p).iter().find(|x| x["id"] == a["question"]).unwrap();
        a["probabilities"] = json!({"true": 0.8, "false": 0.2});
        match q["decision"].as_str().unwrap() {
            "likely_fixed" => a["commit"] = json!(STORE_COMMIT),
            "duplicate" => a["related"] = json!(tr.items[1].1),
            _ => {}
        }
    }
    record(&tr.t, "R1", file, 4).unwrap();
    tr
}

fn closure_truths(t: &TempRoot) -> BTreeMap<String, (bool, String)> {
    t.root
        .read(|l| truth::closure_truth(l.conn(), "home", &BTreeSet::new()))
        .unwrap()
        .into_iter()
        .map(|c| (c.decision, (c.value, c.provenance)))
        .collect()
}

/// Observes `id` closed at `closed` (fixture minutes) with `reason`.
fn closed_at(t: &TempRoot, id: &str, closed: i64, reason: &str) {
    edit(t, id, closed, |c| {
        c["state"] = json!("closed");
        c["closed_at"] = json!(format_time(at(closed)));
        c["state_reason"] = json!(reason);
    });
}

#[test]
fn a_close_within_14_days_for_its_reason_is_truth_and_a_later_or_other_close_is_not() {
    let day = 24 * 60;
    let tr = closures_suggested();
    let id = tr.items[0].0.clone();
    closed_at(&tr.t, &id, 4 + 15 * day, "completed");
    assert!(
        !closure_truths(&tr.t).contains_key("likely_fixed"),
        "closed after 15 days"
    );

    let tr = closures_suggested();
    let id = tr.items[0].0.clone();
    closed_at(&tr.t, &id, 4 + 3 * day, "not_planned");
    let found = closure_truths(&tr.t);
    assert!(!found.contains_key("likely_fixed"), "another reason: {found:?}");
    assert_eq!(found["obsolete"], (true, "owner_accept".to_string()));

    let tr = closures_suggested();
    let id = tr.items[0].0.clone();
    closed_at(&tr.t, &id, 4 + 3 * day, "completed");
    assert_eq!(
        closure_truths(&tr.t)["likely_fixed"],
        (true, "owner_accept".to_string())
    );
}

#[test]
fn a_dismissal_with_a_reason_is_a_false_truth_and_one_without_is_none() {
    let tr = closures_suggested();
    let id = tr.items[0].0.clone();
    decided(
        &tr.t,
        &on_suggestion(
            "d-1",
            Action::DismissSuggestion,
            &suggestion_doc(&tr.t, &id, "obsolete"),
            10,
            None,
        ),
    );
    assert!(!closure_truths(&tr.t).contains_key("obsolete"));
    let mut d = on_suggestion(
        "d-2",
        Action::DismissSuggestion,
        &suggestion_doc(&tr.t, &id, "likely_fixed"),
        11,
        None,
    );
    d.reason = Some("still reproduces".into());
    decided(&tr.t, &d);
    assert_eq!(closure_truths(&tr.t)["likely_fixed"], (false, "owner_edit".to_string()));
}

fn duplicate_close(item: &str, alias: &str, of: u64, minutes: i64) -> Decision {
    let mut d = decision(
        &format!("d-close-{of}"),
        OWNER,
        Action::Close,
        Target {
            item: Some(item.into()),
            alias: Some(alias.into()),
            ..Default::default()
        },
        Some(DecisionValue {
            close: Some(CloseValue {
                reason: CloseReason::Duplicate,
                duplicate_of: Some(of),
                comment: "dup".into(),
            }),
            ..Default::default()
        }),
    );
    d.created_at = format_time(at(minutes));
    d
}

fn number(alias: &str) -> u64 {
    alias.rsplit_once('#').unwrap().1.parse().unwrap()
}

#[test]
fn a_duplicate_close_naming_another_issue_than_the_suggested_one_is_no_truth() {
    let tr = closures_suggested();
    let (id, alias) = tr.items[0].clone();
    decided(&tr.t, &duplicate_close(&id, &alias, number(&tr.items[2].1), 10));
    assert!(!closure_truths(&tr.t).contains_key("duplicate"));
    let tr = closures_suggested();
    decided(&tr.t, &duplicate_close(&id, &alias, number(&tr.items[1].1), 10));
    assert_eq!(closure_truths(&tr.t)["duplicate"], (true, "owner_accept".to_string()));
}

// --- Drift (3.5) ----------------------------------------------------------------------------

fn passing_eval(t: &TempRoot, minutes: i64, accuracy: f64) {
    let data = eval::EvaluatedData {
        project: "home".into(),
        backend: BACKEND.into(),
        decision: "type".into(),
        question_hash: type_hash(),
        mode: "replay".into(),
        map_id: None,
        uncalibratable: false,
        t: 1.0,
        n: 120,
        n_tuning: 80,
        accuracy,
        ece: 0.05,
        ece_interval: [0.03, 0.08],
        classwise_ece: BTreeMap::new(),
        false_alarm_rate: Some(0.02),
        exclusions: Vec::new(),
        bars: eval::bars(120, 0.05, accuracy),
        unanswered: 0,
        by_source: BTreeMap::new(),
    };
    let subject = eval::key_subject("home", BACKEND, "type", &type_hash());
    t.root
        .mutate(at(minutes), |tx| {
            tx.record_valid(
                Kind::Evaluated,
                &subject,
                json!({"manifest": "live", "answers": format!("{minutes:064}")}),
                serde_json::to_value(&data)?,
                ledger::Dedupe::Latest,
            )
        })
        .unwrap();
}

fn drift(t: &TempRoot) -> eval::Drift {
    t.root
        .read(|l| eval::drift(l.conn(), "home", BACKEND, "type", &type_hash()))
        .unwrap()
}

#[test]
fn drift_is_on_at_30_records_more_than_10_points_below_and_off_after_a_passing_eval() {
    let tr = triaged();
    passing_eval(&tr.t, 5, 0.9);
    let docs: Vec<String> = tr
        .items
        .iter()
        .map(|(id, _)| suggestion_doc(&tr.t, id, "type"))
        .collect();
    // 20 accepts of `bug` and 10 edits to `todo`: 2/3 agreement, 23 points below 0.9.
    let record = |i: usize| {
        let doc = &docs[i % docs.len()];
        let d = if i % 3 == 2 {
            let todo = DecisionValue {
                type_: Some("todo".into()),
                ..Default::default()
            };
            on_suggestion(
                &format!("d-{i}"),
                Action::EditSuggestion,
                doc,
                10 + i as i64,
                Some(todo),
            )
        } else {
            on_suggestion(&format!("d-{i}"), Action::AcceptSuggestion, doc, 10 + i as i64, None)
        };
        decided(&tr.t, &d);
    };
    for i in 0..29 {
        record(i);
    }
    let d = drift(&tr.t);
    assert_eq!((d.records, d.drifting, d.live_accuracy), (29, false, None));
    record(29);
    let d = drift(&tr.t);
    assert_eq!((d.records, d.drifting), (30, true));
    assert!(close(d.live_accuracy.unwrap(), 20.0 / 30.0));
    // A record before the passing eval is not counted; a new passing eval starts again.
    passing_eval(&tr.t, 100, 0.9);
    assert_eq!((drift(&tr.t).records, drift(&tr.t).drifting), (0, false));
}

#[test]
fn drift_is_off_within_10_points() {
    let tr = triaged();
    passing_eval(&tr.t, 5, 0.75);
    let docs: Vec<String> = tr
        .items
        .iter()
        .map(|(id, _)| suggestion_doc(&tr.t, id, "type"))
        .collect();
    for i in 0..30 {
        let doc = &docs[i % docs.len()];
        let d = if i % 3 == 2 {
            let todo = DecisionValue {
                type_: Some("todo".into()),
                ..Default::default()
            };
            on_suggestion(
                &format!("d-{i}"),
                Action::EditSuggestion,
                doc,
                10 + i as i64,
                Some(todo),
            )
        } else {
            on_suggestion(&format!("d-{i}"), Action::AcceptSuggestion, doc, 10 + i as i64, None)
        };
        decided(&tr.t, &d);
    }
    let d = drift(&tr.t);
    assert_eq!((d.records, d.drifting), (30, false), "0.75 − 0.667 is 8 points");
}

// --- Gate round 1 -------------------------------------------------------------------------

fn rec(at_min: i64, provenance: &str, op: Op, from_backend: Option<&str>) -> Record {
    Record {
        item: "I_1".into(),
        decision: "type".into(),
        at: at(at_min),
        provenance: provenance.into(),
        op,
        from_backend: from_backend.map(str::to_string),
        suggested: None,
        question: None,
    }
}

#[test]
fn a_single_choice_truth_is_the_latest_value_still_standing() {
    let none = BTreeSet::new();
    // The owner said bug; GitHub later added todo beside it: todo is the latest.
    let recs = vec![
        rec(1, "owner_edit", Op::Set(vec!["bug".into()]), Some(BACKEND)),
        rec(2, "github_later", Op::Add("todo".into()), None),
    ];
    let t = truth::fold("type", &recs, &none).unwrap();
    assert_eq!(
        (t.value, t.provenance.as_str(), t.from_backend),
        (json!("todo"), "github_later", None)
    );
    // Removing it uncovers the earlier value.
    let mut more = recs.clone();
    more.push(rec(3, "github_later", Op::Remove("todo".into()), None));
    assert_eq!(truth::fold("type", &more, &none).unwrap().value, json!("bug"));
    // Two type labels added in turn: the later one.
    let two = vec![
        rec(1, "filed", Op::Add("bug".into()), None),
        rec(2, "filed", Op::Add("docs".into()), None),
    ];
    assert_eq!(truth::fold("type", &two, &none).unwrap().value, json!("docs"));
    // The owner's record keeps its suggestion's backend.
    let owner = vec![rec(1, "owner_accept", Op::Set(vec!["bug".into()]), Some(BACKEND))];
    assert_eq!(
        truth::fold("type", &owner, &none).unwrap().from_backend.as_deref(),
        Some(BACKEND)
    );
}

#[test]
fn a_history_read_for_another_window_or_filter_restarts_and_is_never_evaluated() {
    let t = fixture_root();
    // An interrupted read for 30 days, its cursor somewhere no recording covers: resuming it
    // would fail; a 730-day refresh starts over.
    let stale = RepoHistory {
        repo: AIRZONED.into(),
        filter: None,
        window_days: 30,
        since: "2026-09-29T12:00:00Z".into(),
        started_at: NOW.into(),
        complete: false,
        cursor: Some(history::Cursor::Connection {
            after: Some("no-such-cursor".into()),
        }),
        requests: 1,
        items: Vec::new(),
        skipped: Vec::new(),
    };
    history::save(t.path(), &stale).unwrap();
    let r = history::refresh_repo(t.path(), &history_gh(), AIRZONED, None, 730, now(), HISTORY_PAGE).unwrap();
    assert_eq!((r.complete, r.resumed, r.items), (true, false, 44));
    // The same for a search read under another filter.
    history::save(
        t.path(),
        &RepoHistory {
            repo: RHAI.into(),
            filter: Some("author:someone-else".into()),
            window_days: 730,
            cursor: Some(history::Cursor::Search {
                ids: vec!["I_no_such_node".into()],
                read: 0,
            }),
            ..stale.clone()
        },
    )
    .unwrap();
    let r = history::refresh_repo(
        t.path(),
        &history_gh(),
        RHAI,
        Some("author:yuvalrakavy"),
        730,
        now(),
        HISTORY_PAGE,
    )
    .unwrap();
    assert_eq!((r.complete, r.resumed, r.items), (true, false, 1));
    let complete_store = |window_days: u32, filter: Option<&str>| RepoHistory {
        repo: STORE.into(),
        filter: filter.map(str::to_string),
        window_days,
        complete: true,
        cursor: None,
        ..stale.clone()
    };
    history::save(t.path(), &complete_store(730, None)).unwrap();
    let c = ConfigSet::load(t.path()).unwrap();
    assert_eq!(history::project_history(t.path(), &c, "home").unwrap().len(), 3);
    // A complete history read for another window, or under another filter, is refused.
    history::save(t.path(), &complete_store(30, None)).unwrap();
    let m = refused(history::project_history(t.path(), &c, "home"));
    assert!(m.contains("history of yuvalrakavy/Store was read for 30 days"), "{m}");
    history::save(t.path(), &complete_store(730, Some("label:bug"))).unwrap();
    let m = refused(history::project_history(t.path(), &c, "home"));
    assert!(m.contains("filtered by label:bug"), "{m}");
}

#[test]
fn score_refuses_exclusions_other_than_the_manifest_s() {
    let t = with_history(Vec::new());
    let r = replay("type");
    let m = manifest_of(&prepare_eval(&t, &r).unwrap());
    let mut other = r.clone();
    other.exclude = vec!["github_later".into()];
    let msg = refused(score(&t, &other, &answers(&m, 0.6, |_| true)));
    assert!(msg.contains("prepared excluding []"), "{msg}");
    assert!(
        t.root
            .read(|l| ledger::events_by_kind(l.conn(), Kind::Evaluated))
            .unwrap()
            .is_empty()
    );
}

#[test]
fn manifest_paths_never_collide_and_score_refuses_a_manifest_of_another_key() {
    let p = |backend: &str, repo: Option<&str>| {
        eval::manifest_path(
            std::path::Path::new("/r"),
            "home",
            backend,
            "type",
            repo,
            EvalStage::Scored,
        )
    };
    assert_ne!(p(BACKEND, Some("a/b_c")), p(BACKEND, Some("a_b/c")));
    assert_ne!(p("m:x@medium", None), p("m?x@medium", None));
    assert_ne!(p("claude/opus@medium", None), p("claude_opus@medium", None));

    let t = with_history(Vec::new());
    let r = replay("type");
    let prepared = prepare_eval(&t, &r).unwrap();
    let m = manifest_of(&prepared);
    // The type manifest under priority's name.
    let wrong = eval::manifest_path(t.path(), "home", BACKEND, "priority", None, EvalStage::Scored);
    fs::copy(&prepared.manifest, &wrong).unwrap();
    let msg = refused(score(&t, &replay("priority"), &answers(&m, 0.6, |_| true)));
    assert!(msg.contains("is the manifest of home/"), "{msg}");
    assert!(msg.contains("/type"), "{msg}");
}

#[test]
fn a_tag_is_its_project_s_and_returning_to_an_earlier_name_puts_it_back_in_force() {
    let t = with_history(Vec::new());
    let h = history::load(t.path(), AIRZONED).unwrap().unwrap();
    let item = &h.items[0];
    let e = &item.label_events[0];
    let tagged = Tagged {
        event: e.id.clone(),
        item: item.id.clone(),
        alias: item.alias.clone(),
        label: e.label.clone(),
        added: e.added,
        at: e.at.clone(),
    };
    let tag = |project: &str, name: &str| {
        t.root
            .mutate(now(), |tx| {
                truth::record_tags(tx, project, name, "type", std::slice::from_ref(&tagged))
            })
            .unwrap()
    };
    assert_eq!(tag("home", "oct-a"), 1);
    assert_eq!(tag("home", "oct-a"), 0, "a repeat");
    assert_eq!(tag("kernel", "oct-a"), 1, "another project's tag of the same event");
    let in_force = |project: &str| {
        t.root
            .read(|l| truth::tags(l.conn(), project))
            .unwrap()
            .get(&e.id)
            .cloned()
    };
    assert_eq!(in_force("kernel").as_deref(), Some("oct-a"));
    assert_eq!(tag("home", "oct-b"), 1);
    assert_eq!(tag("home", "oct-a"), 1, "back to the first name");
    assert_eq!(in_force("home").as_deref(), Some("oct-a"));
}

#[test]
fn an_excluded_closure_verdict_uncovers_the_earlier_one() {
    let tr = closures_suggested();
    let id = tr.items[0].0.clone();
    let doc = suggestion_doc(&tr.t, &id, "likely_fixed");
    decided(&tr.t, &on_suggestion("d-1", Action::AcceptSuggestion, &doc, 10, None));
    let mut d = on_suggestion("d-2", Action::DismissSuggestion, &doc, 11, None);
    d.reason = Some("still reproduces".into());
    decided(&tr.t, &d);
    assert_eq!(closure_truths(&tr.t)["likely_fixed"], (false, "owner_edit".to_string()));
    let without_edits: BTreeMap<String, (bool, String)> =
        tr.t.root
            .read(|l| truth::closure_truth(l.conn(), "home", &BTreeSet::from(["owner_edit".to_string()])))
            .unwrap()
            .into_iter()
            .map(|c| (c.decision, (c.value, c.provenance)))
            .collect();
    assert_eq!(without_edits["likely_fixed"], (true, "owner_accept".to_string()));
}

#[test]
fn a_closure_key_counts_its_verdicts_toward_drift() {
    let tr = closures_suggested();
    let hash = Catalogue::embedded().question_hash("likely_fixed", &[]).unwrap();
    let data = eval::EvaluatedData {
        project: "home".into(),
        backend: BACKEND.into(),
        decision: "likely_fixed".into(),
        question_hash: hash.clone(),
        mode: "live".into(),
        map_id: None,
        uncalibratable: false,
        t: 1.0,
        n: 120,
        n_tuning: 80,
        accuracy: 0.9,
        ece: 0.05,
        ece_interval: [0.03, 0.08],
        classwise_ece: BTreeMap::new(),
        false_alarm_rate: Some(0.02),
        exclusions: Vec::new(),
        bars: eval::bars(120, 0.05, 0.9),
        unanswered: 0,
        by_source: BTreeMap::new(),
    };
    let subject = eval::key_subject("home", BACKEND, "likely_fixed", &hash);
    tr.t.root
        .mutate(at(5), |tx| {
            tx.record_valid(
                Kind::Evaluated,
                &subject,
                json!({"manifest": "live", "answers": format!("{:064}", 5)}),
                serde_json::to_value(&data)?,
                ledger::Dedupe::Latest,
            )
        })
        .unwrap();
    let id = tr.items[0].0.clone();
    let mut d = on_suggestion(
        "d-1",
        Action::DismissSuggestion,
        &suggestion_doc(&tr.t, &id, "likely_fixed"),
        10,
        None,
    );
    d.reason = Some("still reproduces".into());
    decided(&tr.t, &d);
    let drift =
        tr.t.root
            .read(|l| eval::drift(l.conn(), "home", BACKEND, "likely_fixed", &hash))
            .unwrap();
    assert_eq!((drift.records, drift.live_accuracy, drift.drifting), (1, None, false));
}

#[test]
fn owner_truths_are_scored_apart_by_source_and_backend() {
    let allowed = strings(&["bug", "todo"]);
    let mut e = vec![
        ex("I_1", &[("bug", 0.9), ("todo", 0.1)], json!("bug")),
        ex("I_2", &[("bug", 0.9), ("todo", 0.1)], json!("todo")),
        ex("I_3", &[("bug", 0.2), ("todo", 0.8)], json!("todo")),
        ex("I_4", &[("bug", 0.2), ("todo", 0.8)], json!("todo")),
    ];
    e[0].source = eval::owner_source("owner_accept", Some(BACKEND));
    e[1].source = eval::owner_source("owner_accept", Some(BACKEND));
    e[2].source = eval::owner_source("owner_edit", Some("jev@1"));
    assert_eq!(eval::owner_source("filed", None), None);
    let by = eval::by_source(AnswerKind::Choice, &allowed, &e);
    assert_eq!(by.len(), 2, "{by:?}");
    let a = &by[&format!("owner_accept/{BACKEND}")];
    assert_eq!((a.n, a.accuracy), (2, 0.5));
    let b = &by["owner_edit/jev@1"];
    assert_eq!((b.n, b.accuracy), (1, 1.0));
}

#[test]
fn a_backend_with_a_slash_scores_and_validates() {
    let t = with_history(Vec::new());
    let mut r = replay("type");
    r.backend = "claude/opus-5-5@medium".into();
    let m = manifest_of(&prepare_eval(&t, &r).unwrap());
    let mut a = answers(&m, 0.6, |i| i % 4 != 3);
    a.backend = r.backend.clone();
    let s = score(&t, &r, &a).unwrap();
    assert_eq!(s.evaluated.backend, "claude/opus-5-5@medium");
}

#[test]
fn a_flat_loss_fits_the_lowest_t() {
    // Even odds stay even at every T: the loss is flat, and the tie goes to the lowest T.
    let e: Vec<Example> = (0..10)
        .map(|i| {
            ex(
                &format!("I_{i}"),
                &[("true", 0.5), ("false", 0.5)],
                json!(if i % 2 == 0 { "true" } else { "false" }),
            )
        })
        .collect();
    let f = eval::fit(AnswerKind::Boolean, &e);
    assert_eq!((f.t, f.at_edge), (0.25, true));
}

#[test]
fn classwise_ece_is_each_answer_against_whether_it_is_the_truth() {
    // bug: (0.8, right) in bin 8 and (0.6, wrong) in bin 6: ½·0.2 + ½·0.6 = 0.4.
    // todo: (0.2, wrong) and (0.4, right): ½·0.2 + ½·0.6 = 0.4.
    let e = [
        ex("I_1", &[("bug", 0.8), ("todo", 0.2)], json!("bug")),
        ex("I_2", &[("bug", 0.6), ("todo", 0.4)], json!("todo")),
    ];
    let c = eval::classwise_ece(AnswerKind::Choice, &strings(&["bug", "todo"]), &e);
    assert!(close(c["bug"], 0.4) && close(c["todo"], 0.4), "{c:?}");
}

#[test]
fn the_interval_is_the_5th_and_95th_percentile_of_the_resamples() {
    let e = interval_fixture();
    let allowed = strings(&["bug", "todo"]);
    // Seed 4: the 49th and 50th sorted resamples differ, so rounding p · 999 is observable.
    let (lo, hi) = eval::ece_interval(AnswerKind::Choice, &allowed, &e, 4);
    assert!(close(lo, 0.059) && close(hi, 0.227), "{lo} {hi}");
}
