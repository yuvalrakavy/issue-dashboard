//! Tests that pin guards of the cycle which no other test caught when it was removed or
//! inverted (the pre-merge mutation pass). Each test names the mechanism it keeps.

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use common::TempRoot;
use common::fixture::*;
use issue_dashboard_core::cycle;
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::github::{Replay, Request};
use issue_dashboard_core::observe::ObservedData;
use issue_dashboard_core::snapshot::{self, RepoReport};
use issue_dashboard_core::{Error, ledger};
use serde_json::{Value, json};

fn snap(t: &TempRoot, gh: &Replay, project: &str, now: DateTime<Utc>) -> Vec<RepoReport> {
    cycle::snapshot(&t.root, gh, project, &move || now, PAGE).unwrap()
}

fn report<'a>(r: &'a [RepoReport], repo: &str) -> &'a RepoReport {
    r.iter()
        .find(|x| x.repo == repo)
        .unwrap_or_else(|| panic!("no report for {repo}: {r:?}"))
}

fn node_of(t: &TempRoot, repo: &str) -> String {
    t.root
        .read(|l| snapshot::repo_node_of(l.conn(), repo))
        .unwrap()
        .unwrap_or_else(|| panic!("no node for {repo}"))
}

fn members(t: &TempRoot, project: &str, repo: &str) -> BTreeSet<String> {
    let node = node_of(t, repo);
    t.root
        .read(|l| snapshot::membership(l.conn(), project, &node))
        .unwrap()
        .map(|(_, m)| m.all())
        .unwrap_or_default()
}

fn observations(t: &TempRoot, id: &str) -> Vec<ObservedData> {
    t.root
        .read(|l| ledger::events_of(l.conn(), id, Kind::Observed))
        .unwrap()
        .into_iter()
        .map(|e| serde_json::from_value(e.data).unwrap())
        .collect()
}

fn some_members(t: &TempRoot, project: &str, repo: &str, n: usize) -> Vec<String> {
    members(t, project, repo)
        .into_iter()
        .filter(|id| id.starts_with("I_") && observations(t, id).last().unwrap().content["state"] == "open")
        .take(n)
        .collect()
}

fn fail(what: &str) -> issue_dashboard_core::Result<Value> {
    Err(Error::GitHub(format!("simulated failure: {what}")))
}

/// Runs `sql` against the root's ledger file, behind the library's back.
fn sql(t: &TempRoot, sql: &str) {
    let c = rusqlite::Connection::open(t.path().join("ledger.sqlite")).unwrap();
    c.execute_batch(sql).unwrap();
}

/// Inserts `node` into an `issues_since` response.
fn into_since_list(req: &Request, mut v: Value, node: &Value) -> Value {
    if req.name == "issues_since" {
        let nodes = v["data"]["repository"]["issues"]["nodes"].as_array_mut().unwrap();
        nodes.retain(|n| n["id"] != node["id"]);
        nodes.push(node.clone());
    }
    v
}

#[test]
fn a_read_exactly_a_day_after_the_last_full_one_is_full() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let r = snap(&t, &gh(), "home", at(24 * 60 - 1));
    assert_eq!(report(&r, STORE).mode.as_deref(), Some("incremental"));
    // The incremental read did not move the full read's time.
    let r = snap(&t, &gh(), "home", at(24 * 60));
    assert_eq!(report(&r, STORE).mode.as_deref(), Some("full"));
    assert_eq!(report(&r, STORE).full_reason.as_deref(), Some("daily"));
}

#[test]
fn the_incremental_read_overlaps_the_previous_one_by_five_minutes() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let ids = some_members(&t, "home", STORE, 2);
    let lib = library();
    // Updated 3 minutes before the previous read started (inside the overlap) and 7 minutes
    // before (outside it).
    let mut inside = real_node(&lib, &ids[0]).unwrap();
    inside["title"] = json!("inside the overlap");
    inside["updatedAt"] = json!(at(-3).to_rfc3339());
    let mut outside = real_node(&lib, &ids[1]).unwrap();
    outside["title"] = json!("outside the overlap");
    outside["updatedAt"] = json!(at(-7).to_rfc3339());
    let gh = gh_with(move |req, v| {
        let v = v?;
        Ok(into_since_list(req, into_since_list(req, v, &inside), &outside))
    });
    let r = snap(&t, &gh, "home", at(10));
    assert_eq!(report(&r, STORE).mode.as_deref(), Some("incremental"));
    assert_eq!(
        observations(&t, &ids[0]).last().unwrap().content["title"],
        "inside the overlap"
    );
    assert_ne!(
        observations(&t, &ids[1]).last().unwrap().content["title"],
        "outside the overlap"
    );
}

#[test]
fn the_history_window_defaults_to_180_days() {
    let t = fixture_root();
    let p = t.path().join("projects/home.toml");
    let text = std::fs::read_to_string(&p).unwrap().replace("history_days = 30\n", "");
    assert!(!text.contains("history_days"));
    std::fs::write(&p, text).unwrap();
    let seen = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = seen.clone();
    let gh = gh_with(move |req, v| {
        if req.name == "search" {
            log.lock()
                .unwrap()
                .push(req.variables["q"].as_str().unwrap().to_string());
        }
        v
    });
    snap(&t, &gh, "home", at(0));
    let want = format!("closed:>={}", (at(0) - chrono::Duration::days(180)).format("%Y-%m-%d"));
    let seen = seen.lock().unwrap();
    assert!(!seen.is_empty());
    for q in seen.iter().filter(|q| q.contains("is:closed")) {
        assert!(q.contains(&want), "{q} should carry {want}");
    }
}

#[test]
fn lost_read_bookkeeping_forces_a_full_read_never_an_incremental_one() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    // The watermark of the repo is gone: nothing to read incrementally from.
    sql(&t, "DELETE FROM watermarks");
    let r = snap(&t, &gh(), "home", at(10));
    assert_eq!(report(&r, STORE).mode.as_deref(), Some("full"));
    assert_eq!(report(&r, STORE).full_reason.as_deref(), Some("no watermark"));
    // The time of the last full read is gone.
    sql(&t, "UPDATE reads SET full_at = NULL");
    let r = snap(&t, &gh(), "home", at(20));
    assert_eq!(report(&r, STORE).mode.as_deref(), Some("full"));
    assert_eq!(report(&r, STORE).full_reason.as_deref(), Some("no full read recorded"));
    // The view the last read used is unknown.
    sql(&t, "UPDATE reads SET view_hash = NULL");
    let r = snap(&t, &gh(), "home", at(30));
    assert_eq!(
        report(&r, STORE).full_reason.as_deref(),
        Some("view or translation changed")
    );
}

#[test]
fn a_first_read_that_failed_is_still_a_first_read() {
    let t = fixture_root();
    let r = snap(&t, &gh_with(|_, _| fail("down")), "home", at(0));
    assert!(report(&r, STORE).error.is_some());
    let r = snap(&t, &gh(), "home", at(10));
    assert_eq!(report(&r, STORE).full_reason.as_deref(), Some("first read"));
}

#[test]
fn a_name_that_now_resolves_to_another_repo_shares_nothing_with_the_old_one() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let old_node = node_of(&t, STORE);
    let m0 = members(&t, "home", STORE);
    // Deleted and created again under the same name: a new node id, the same search results.
    let gh = gh_with(|req, v| {
        let mut v = v?;
        if req.name == "repo" && req.variables["name"] == "Store" {
            v["data"]["repository"]["id"] = json!("R_recreated");
        }
        Ok(v)
    });
    let r = snap(&t, &gh, "home", at(10));
    let store = report(&r, STORE);
    assert_eq!(store.error, None, "{store:?}");
    assert_eq!(store.mode.as_deref(), Some("full"), "a new repo is read in full");
    assert_eq!(store.entered, m0.len(), "all of its items enter");
    assert!(gh.calls().iter().any(|c| c.starts_with("nodes:")), "{:?}", gh.calls());
    // Its set and watermark are its own.
    let node = t
        .root
        .read(|l| snapshot::repo_node_of(l.conn(), STORE))
        .unwrap()
        .unwrap();
    assert_eq!(node, "R_recreated");
    assert_ne!(node, old_node);
    let (_, m) = t
        .root
        .read(|l| snapshot::membership(l.conn(), "home", "R_recreated"))
        .unwrap()
        .unwrap();
    assert_eq!(m.all(), m0);
    let w = t.root.read(|l| snapshot::watermark(l.conn(), "R_recreated")).unwrap();
    assert_eq!(w, Some(at(10)));
    let w = t.root.read(|l| snapshot::watermark(l.conn(), &old_node)).unwrap();
    assert_eq!(w, Some(at(0)), "the old repo's watermark is left alone");
}

#[test]
fn a_read_keeps_what_another_project_tracks_in_the_repo_and_nothing_nobody_tracks() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    snap(&t, &gh(), "kernel", at(0));
    // z is in home's set but not in kernel's; the watermark kernel's next read advances is
    // home's too, so that read must keep z's change.
    let home = members(&t, "home", STORE);
    let kernel = members(&t, "kernel", STORE);
    let z = home
        .iter()
        .find(|id| id.starts_with("I_") && !kernel.contains(*id))
        .expect("home tracks more of Store than kernel")
        .clone();
    let lib = library();
    let mut changed = real_node(&lib, &z).unwrap();
    changed["title"] = json!("changed while kernel was reading");
    // Updated at(2): inside kernel's window (watermark at(0) less five minutes), outside home's
    // next one (watermark at(10) less five minutes).
    changed["updatedAt"] = json!(at(2).to_rfc3339());
    // A node nobody tracks, updated in the same window.
    let mut ghost = real_node(&lib, &z).unwrap();
    ghost["id"] = json!("I_nobody_tracks_this");
    ghost["updatedAt"] = json!(at(2).to_rfc3339());
    let gh = gh_with(move |req, v| {
        let v = v?;
        Ok(into_since_list(req, into_since_list(req, v, &changed), &ghost))
    });
    let r = snap(&t, &gh, "kernel", at(10));
    assert_eq!(report(&r, STORE).mode.as_deref(), Some("incremental"));
    assert_eq!(
        observations(&t, &z).last().unwrap().content["title"],
        "changed while kernel was reading"
    );
    assert!(observations(&t, "I_nobody_tracks_this").is_empty());
}
