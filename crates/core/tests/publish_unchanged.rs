//! Skipping a publish when nothing changed (7.2, 10.6): the outbox's fingerprint leaves out
//! what differs between two runs over the same state, the published `meta/active` carries it,
//! and an outbox whose fingerprint equals the active generation's writes no generation, only a
//! pinned `meta/heartbeat` naming the sync time.

mod common;

use std::collections::BTreeMap;
use std::fs;

use chrono::{DateTime, Utc};
use common::fixture::*;
use common::*;
use issue_dashboard_core::cycle::{self, CycleReport};
use issue_dashboard_core::docs::Priority;
use issue_dashboard_core::ingest::Pages;
use issue_dashboard_core::publish::{self, Active};
use serde_json::{Value, json};

fn run(t: &TempRoot, p: &Pages, minutes: i64) -> CycleReport {
    cycle::cycle(&t.root, &gh(), "home", p, &move || at(minutes), PAGE).unwrap()
}

fn doc(r: &CycleReport) -> Value {
    serde_json::from_str(&fs::read_to_string(&r.outbox).unwrap()).unwrap()
}

fn read(path: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn now() -> DateTime<Utc> {
    at(60)
}

/// A named edit of an outbox.
type OutboxEdit = (&'static str, Box<dyn Fn(&mut Value)>);

/// Two outboxes of the same state: the fixture's home cycle, then another ten minutes later.
fn two_quiet_cycles() -> (Value, Value) {
    let t = fixture_root();
    let a = doc(&run(&t, &no_pages(), 0));
    let b = doc(&run(&t, &no_pages(), 10));
    (a, b)
}

/// `meta/active` as the page holds it after `outbox` was published.
fn published(outbox: &Value, version: u64) -> Active {
    let out = tempfile::tempdir().unwrap();
    let s = publish::plan(outbox, None, &BTreeMap::new(), now(), out.path()).unwrap();
    let switch = read(s["switch"][0].as_str().unwrap());
    Active {
        meta: read(switch[0]["file_path"].as_str().unwrap()),
        version,
    }
}

#[test]
fn two_cycles_over_the_same_state_have_the_same_fingerprint() {
    let (a, b) = two_quiet_cycles();
    // The runs differ in every per-run field, so the test is not comparing a file with itself.
    assert_ne!(a["sequence"], b["sequence"]);
    assert_ne!(a["meta"]["taken_at"], b["meta"]["taken_at"]);
    assert_ne!(a["meta"]["generation"], b["meta"]["generation"]);
    let read_at = |d: &Value| {
        d["meta"]["repos"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["read_at"].clone())
            .collect::<Vec<_>>()
    };
    assert_ne!(
        read_at(&a),
        read_at(&b),
        "each repo is read again, so its read_at moves"
    );
    assert_eq!(publish::fingerprint(&a), publish::fingerprint(&b));
}

#[test]
fn the_fingerprint_leaves_out_each_per_run_field_and_the_order_of_documents() {
    let (a, _) = two_quiet_cycles();
    let base = publish::fingerprint(&a);
    let edits: Vec<OutboxEdit> = vec![
        ("sequence", Box::new(|d| d["sequence"] = json!(999))),
        ("taken_at", Box::new(|d| d["taken_at"] = json!("2027-01-01T00:00:00Z"))),
        (
            "meta.generation",
            Box::new(|d| d["meta"]["generation"] = json!("gen-999")),
        ),
        ("meta.sequence", Box::new(|d| d["meta"]["sequence"] = json!(999))),
        (
            "meta.taken_at",
            Box::new(|d| d["meta"]["taken_at"] = json!("2027-01-01T00:00:00Z")),
        ),
        (
            "meta.fingerprint",
            Box::new(|d| d["meta"]["fingerprint"] = json!("0".repeat(64))),
        ),
        (
            "meta.repos[].read_at",
            Box::new(|d| d["meta"]["repos"][0]["read_at"] = json!("2027-01-01T00:00:00Z")),
        ),
        (
            "meta.repos[].read",
            Box::new(|d| {
                let r = &mut d["meta"]["repos"][0]["read"];
                *r = json!(if r == "full" { "incremental" } else { "full" });
            }),
        ),
        (
            "the order of issues",
            Box::new(|d| d["issues"].as_array_mut().unwrap().reverse()),
        ),
    ];
    for (what, edit) in edits {
        let mut b = a.clone();
        edit(&mut b);
        assert_ne!(a, b, "{what}: the edit changed nothing");
        assert_eq!(
            publish::fingerprint(&b),
            base,
            "{what} is per-run and must not change the fingerprint"
        );
    }
}

#[test]
fn any_real_change_changes_the_fingerprint() {
    let (a, _) = two_quiet_cycles();
    let base = publish::fingerprint(&a);
    let edits: Vec<OutboxEdit> = vec![
        (
            "an issue's title",
            Box::new(|d| d["issues"][0]["title"] = json!("edited on GitHub")),
        ),
        (
            "an idle age crossing a day",
            Box::new(|d| {
                let n = d["issues"][0]["idle_days"].as_i64().unwrap();
                d["issues"][0]["idle_days"] = json!(n + 1);
            }),
        ),
        (
            "a decision ack",
            Box::new(|d| {
                d["acks"]
                    .as_array_mut()
                    .unwrap()
                    .push(json!({"id": "D_x", "decision": "D_x", "cycle": 3, "result": "applied"}))
            }),
        ),
        (
            "a launch status",
            Box::new(|d| d["launch_status"] = json!([{"id": "L_1", "launch": "L_1", "state": "started"}])),
        ),
        ("a suggestion", Box::new(|d| d["suggestions"] = json!([{"id": "S_1"}]))),
        (
            "a repo going stale",
            Box::new(|d| d["meta"]["repos"][0]["stale"] = json!(true)),
        ),
        (
            "a repo's read error",
            Box::new(|d| d["meta"]["repos"][0]["error"] = json!("rate limited")),
        ),
        ("the summary", Box::new(|d| d["meta"]["summary"]["open"] = json!(0))),
        (
            "an issue gone",
            Box::new(|d| {
                d["issues"].as_array_mut().unwrap().pop();
            }),
        ),
    ];
    for (what, edit) in edits {
        let mut b = a.clone();
        edit(&mut b);
        assert_ne!(publish::fingerprint(&b), base, "{what} is a real change");
    }
}

#[test]
fn a_decision_acknowledged_by_a_cycle_changes_the_fingerprint() {
    let t = fixture_root();
    let a = doc(&run(&t, &no_pages(), 0));
    let x = a["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["state"] == "open")
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    // Not the owner's: refused, so its ack is the cycle's only change.
    let mut d = set_priority("D_set", &x, Priority::P0);
    d.author = "user_someone_else".into();
    let p = Pages {
        decisions: vec![serde_json::to_value(d).unwrap()],
        ..Pages::default()
    };
    let b = doc(&run(&t, &p, 10));
    assert_eq!(
        b["acks"].as_array().unwrap().len(),
        a["acks"].as_array().unwrap().len() + 1
    );
    assert_eq!(a["issues"], b["issues"], "the refused decision changed no issue");
    assert_ne!(publish::fingerprint(&a), publish::fingerprint(&b));
}

#[test]
fn the_switch_publishes_the_outbox_fingerprint_on_meta_active() {
    let (a, _) = two_quiet_cycles();
    let active = published(&a, 1);
    assert_eq!(active.meta["fingerprint"], json!(publish::fingerprint(&a)));
    assert_valid("doc-meta", &active.meta);
    // Everything else on meta/active is the outbox's meta, unchanged.
    let mut rest = active.meta.clone();
    rest.as_object_mut().unwrap().remove("fingerprint");
    assert_eq!(rest, a["meta"]);
}

#[test]
fn an_unchanged_outbox_writes_no_generation_and_a_heartbeat_pinned_to_the_version_read() {
    let (a, b) = two_quiet_cycles();
    let active = published(&a, 7);
    let out = tempfile::tempdir().unwrap();
    let s = publish::plan_with_heartbeat(&b, Some(&active), Some(3), &BTreeMap::new(), now(), out.path()).unwrap();
    assert_eq!(s["decision"], "unchanged", "{s}");
    assert_eq!(
        s["generation"], a["meta"]["generation"],
        "the generation the page keeps showing"
    );
    for k in ["batches", "switch", "clean"] {
        assert!(s.get(k).is_none(), "an unchanged run has no {k}: {s}");
    }
    assert!(
        !out.path().join("batches").exists(),
        "no generation document was planned"
    );
    assert!(!out.path().join("switch.json").exists());
    assert_eq!(read(out.path().join("plan.json").to_str().unwrap()), s);

    // The heartbeat entry is in the summary itself, so the skill passes it without a Read.
    let hb = s["heartbeat"].as_array().unwrap();
    assert_eq!(hb.len(), 1);
    let w = &hb[0];
    assert_eq!(w["op"], "set");
    assert_eq!(w["collection"], "meta");
    assert_eq!(w["doc_id"], "heartbeat");
    assert_eq!(w["if_version"], json!(3));
    let h = read(w["file_path"].as_str().unwrap());
    assert_valid("doc-heartbeat", &h);
    assert_eq!(h["generation"], a["meta"]["generation"]);
    assert_eq!(h["sequence"], b["sequence"]);
    assert_eq!(h["synced_at"], b["meta"]["taken_at"]);
    assert_eq!(h["project"], b["project"]);
    let repos: Vec<Value> = b["meta"]["repos"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| json!({"repo": r["repo"], "read_at": r["read_at"], "read": r["read"]}))
        .collect();
    assert_eq!(
        h["repos"],
        json!(repos),
        "the per-run fields the fingerprint leaves out"
    );
}

#[test]
fn the_first_heartbeat_is_an_unpinned_create() {
    let (a, b) = two_quiet_cycles();
    let active = published(&a, 7);
    let out = tempfile::tempdir().unwrap();
    let s = publish::plan_with_heartbeat(&b, Some(&active), None, &BTreeMap::new(), now(), out.path()).unwrap();
    assert_eq!(s["decision"], "unchanged");
    assert_eq!(s["heartbeat"][0].get("if_version"), None);
}

#[test]
fn a_generation_published_before_fingerprints_or_with_another_one_is_published_in_full() {
    let (a, b) = two_quiet_cycles();
    let mut old = published(&a, 7);
    old.meta.as_object_mut().unwrap().remove("fingerprint");
    let mut other = published(&a, 7);
    other.meta["fingerprint"] = json!("f".repeat(64));
    for (what, active) in [("no fingerprint", old), ("another fingerprint", other)] {
        let out = tempfile::tempdir().unwrap();
        let s = publish::plan_with_heartbeat(&b, Some(&active), Some(3), &BTreeMap::new(), now(), out.path()).unwrap();
        assert_eq!(s["decision"], "publish", "{what}: {s}");
        assert!(s.get("heartbeat").is_none(), "{what}: a publish writes no heartbeat");
        assert!(!out.path().join("docs/meta/heartbeat.json").exists());
    }
}

#[test]
fn a_stop_still_wins_over_unchanged() {
    let (a, b) = two_quiet_cycles();
    let active = published(&b, 7);
    let out = tempfile::tempdir().unwrap();
    // `a` is older than the active generation and has its content: still a stop.
    let s = publish::plan_with_heartbeat(&a, Some(&active), Some(3), &BTreeMap::new(), now(), out.path()).unwrap();
    assert_eq!(s["decision"], "stop", "{s}");
    assert!(s.get("heartbeat").is_none());
}

#[test]
fn cleanup_refuses_an_unchanged_plan() {
    let (a, b) = two_quiet_cycles();
    let active = published(&a, 7);
    let out = tempfile::tempdir().unwrap();
    publish::plan_with_heartbeat(&b, Some(&active), Some(3), &BTreeMap::new(), now(), out.path()).unwrap();
    let e = publish::cleanup(
        &out.path().join("plan.json"),
        &out.path().join("listing"),
        &out.path().join("clean"),
    );
    assert!(e.is_err(), "nothing was switched, so nothing is cleaned");
}
