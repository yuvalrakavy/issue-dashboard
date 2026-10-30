//! Triage helpers shared by the triage tests: a fixture root tracking three open Store issues,
//! launches that held a slot, and `prepare` / `record-suggestions` over them.
#![allow(dead_code)]

use std::collections::BTreeMap;
use std::fs;

use chrono::Duration;
use issue_dashboard_core::config::{ConfigSet, READ_ONLY_RECIPE};
use issue_dashboard_core::cycle;
use issue_dashboard_core::docs::{LaunchKind, LaunchRequest};
use issue_dashboard_core::event::{Event, Kind};
use issue_dashboard_core::github::GitHub;
use issue_dashboard_core::launch::{self, Via};
use issue_dashboard_core::observe::{self, Observation, ObservedData};
use issue_dashboard_core::root::parse_time;
use issue_dashboard_core::suggest;
use issue_dashboard_core::triage::{self, Stage};
use issue_dashboard_core::{Error, Result, ledger};
use serde_json::{Value, json};

use super::fixture::*;
use super::{TempRoot, launch_doc};

pub const D1: &str = "local_dispatcher_1";
/// The fixture launches' routing (`launch_doc`), as a backend.
pub const BACKEND: &str = "claude-opus-5-5@medium";

pub fn cap(recipe: &str) -> Result<u32> {
    match recipe {
        READ_ONLY_RECIPE => Ok(1),
        other => Err(Error::refused(format!("no recipe {other}"))),
    }
}

pub fn refused<T: std::fmt::Debug>(r: Result<T>) -> String {
    match r {
        Err(Error::Refused(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

/// A fixture root after one cycle, and three of its open Store issues, each re-observed as
/// written by the owner with the labels `bug`, `p1`, `kernel`, `upstream`.
pub struct Tracked {
    pub t: TempRoot,
    pub items: Vec<(String, String)>,
}

pub fn tracked() -> Tracked {
    let t = fixture_root();
    let r = cycle::cycle(&t.root, &gh(), "home", &no_pages(), &move || at(0), PAGE).unwrap();
    let d: Value = serde_json::from_str(&fs::read_to_string(&r.outbox).unwrap()).unwrap();
    let items: Vec<(String, String)> = d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| {
            i["alias"].as_str().unwrap().starts_with(&format!("{STORE}#"))
                && i["state"] == "open"
                && i["kind"] == "issue"
        })
        .take(3)
        .map(|i| {
            (
                i["id"].as_str().unwrap().to_string(),
                i["alias"].as_str().unwrap().to_string(),
            )
        })
        .collect();
    assert_eq!(items.len(), 3, "the fixture tracks open Store issues");
    for (id, _) in &items {
        edit(&t, id, 1, |c| {
            c["author"] = json!("yuvalrakavy");
            c["author_association"] = json!("OWNER");
            c["labels"] = json!(["bug", "p1", "kernel", "upstream"]);
        });
    }
    Tracked { t, items }
}

/// Records a fresher observation of `id` at fixture minute `minutes`, its content as `f`
/// leaves it: what a later read of an edited issue leaves in the ledger.
pub fn edit(t: &TempRoot, id: &str, minutes: i64, f: impl FnOnce(&mut Value)) {
    t.root
        .mutate(at(minutes), |tx| {
            let e = ledger::latest(tx.conn(), id, Kind::Observed)?.expect("an observed issue");
            let data: ObservedData = serde_json::from_value(e.data)?;
            let mut content = data.content.clone();
            f(&mut content);
            observe::record(
                tx,
                &Observation {
                    node_id: id.to_string(),
                    item: data.item,
                    alias: data.alias.clone(),
                    updated_at: parse_time(&data.updated_at)? + Duration::seconds(1),
                    read_at: parse_time(&data.read_at)? + Duration::seconds(1),
                    read_id: format!("read-{minutes}"),
                    content,
                },
            )
        })
        .unwrap();
}

pub fn launch_of(id: &str, kind: LaunchKind, items: &[(String, String)]) -> LaunchRequest {
    let mut d = launch_doc(id, "home", &[], READ_ONLY_RECIPE);
    d.kind = kind;
    d.items = items.iter().map(|(i, _)| i.clone()).collect();
    d.aliases = items.iter().map(|(_, a)| a.clone()).collect();
    d
}

/// Requests and reserves a launch, then frees the read-only slot (its session ran), so the
/// next launch of a test can reserve too.
pub fn launched(t: &TempRoot, id: &str, kind: LaunchKind, items: &[(String, String)], minutes: i64) {
    let doc = launch_of(id, kind, items);
    t.root
        .mutate(at(minutes), |tx| {
            launch::request(tx, &doc, Via::Reserve)?;
            let token = match launch::reserve(tx, id, D1, &cap)? {
                launch::Reservation::Reserved { token, .. } => token,
                other => panic!("expected a reservation, got {other:?}"),
            };
            launch::started(tx, &token, &format!("local_{id}"))?;
            launch::ended(tx, &token, launch::EndState::Landed)
        })
        .unwrap();
}

pub fn prepare(t: &TempRoot, launch: &str, model: &str, stage: Stage, minutes: i64) -> Result<triage::Prepared> {
    let c = ConfigSet::load(t.path()).unwrap();
    t.root
        .mutate(at(minutes), |tx| triage::prepare(tx, &c, launch, model, stage))
}

pub fn record(t: &TempRoot, launch: &str, input: Value, minutes: i64) -> Result<suggest::Report> {
    record_via(t, launch, input, minutes, &commit_lookup())
}

/// [`record`], with the commits looked up through `gh`, as `record-suggestions` does.
pub fn record_via(t: &TempRoot, launch: &str, input: Value, minutes: i64, gh: &dyn GitHub) -> Result<suggest::Report> {
    let input: suggest::Input = serde_json::from_value(input).unwrap();
    let commits = commits_for(t, &input, gh);
    record_with(t, launch, &input, minutes, &commits)
}

pub fn record_with(
    t: &TempRoot,
    launch: &str,
    input: &suggest::Input,
    minutes: i64,
    commits: &suggest::Commits,
) -> Result<suggest::Report> {
    let c = ConfigSet::load(t.path()).unwrap();
    t.root
        .mutate(at(minutes), |tx| suggest::record(tx, &c, launch, input, commits))
}

pub fn events(t: &TempRoot, kind: Kind) -> Vec<Event> {
    t.root.read(|l| ledger::events_by_kind(l.conn(), kind)).unwrap()
}

pub fn questions(p: &triage::Prepared) -> &Vec<Value> {
    p.file["questions"].as_array().unwrap()
}

/// The decisions a prepared file asks about `alias`, sorted.
pub fn asked_about(p: &triage::Prepared, alias: &str) -> Vec<String> {
    let mut d: Vec<String> = questions(p)
        .iter()
        .filter(|q| q["item"] == alias)
        .map(|q| q["decision"].as_str().unwrap().to_string())
        .collect();
    d.sort();
    d
}

/// An answer to every question of `p`: the first allowed answer at 0.7 (the rest share 0.3),
/// a closure false at 0.8, the first area at 0.8 and every other at 0.1.
pub fn answer_all(p: &triage::Prepared) -> Value {
    let answers: Vec<Value> = questions(p)
        .iter()
        .map(|q| {
            let allowed: Vec<&str> = q["answers"]
                .as_array()
                .unwrap()
                .iter()
                .map(|a| a.as_str().unwrap())
                .collect();
            let probabilities: BTreeMap<&str, f64> = match q["kind"].as_str().unwrap() {
                "boolean" => [("true", 0.2), ("false", 0.8)].into(),
                "areas" => allowed
                    .iter()
                    .enumerate()
                    .map(|(i, a)| (*a, if i == 0 { 0.8 } else { 0.1 }))
                    .collect(),
                _ => {
                    let rest = 0.3 / (allowed.len() - 1) as f64;
                    allowed
                        .iter()
                        .enumerate()
                        .map(|(i, a)| (*a, if i == 0 { 0.7 } else { rest }))
                        .collect()
                }
            };
            json!({"question": q["id"], "probabilities": probabilities, "reason": "from the body"})
        })
        .collect();
    json!({"backend": BACKEND, "answers": answers})
}

pub fn suggestion_of<'a>(evs: &'a [Event], item: &str, decision: &str) -> &'a Value {
    &evs.iter()
        .rev()
        .find(|e| e.subject == item && e.distinct["decision"] == decision)
        .unwrap_or_else(|| panic!("no {decision} suggestion for {item}"))
        .data["suggestion"]
}
