//! Sync requests (7.2, 10.8): `sync-now` and the page's `sync` request make the next run read
//! the named item by node id, whatever the watermark says, until a good read of its repo has
//! started after the request.

mod common;

use std::fs;

use chrono::{DateTime, Utc};
use common::TempRoot;
use common::fixture::*;
use issue_dashboard_core::config::ConfigSet;
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::github::Replay;
use issue_dashboard_core::ingest::Pages;
use issue_dashboard_core::sync::{self, Requested};
use issue_dashboard_core::{Error, Result, cycle, ledger, snapshot};
use serde_json::{Value, json};

fn snap(t: &TempRoot, gh: &Replay, now: DateTime<Utc>) {
    cycle::snapshot(&t.root, gh, "home", &move || now, PAGE).unwrap();
}

fn ask(t: &TempRoot, issue: Option<&str>, now: DateTime<Utc>) -> Result<Requested> {
    let dir = t.path().to_path_buf();
    t.root.mutate_with_clock(
        move || now,
        |tx| {
            let config = ConfigSet::load(&dir)?;
            config.record(tx)?;
            sync::request(tx, &config, "home", issue)
        },
    )
}

fn alias_of(t: &TempRoot, id: &str) -> String {
    let e = t
        .root
        .read(|l| ledger::latest(l.conn(), id, Kind::Observed))
        .unwrap()
        .unwrap();
    e.data["alias"].as_str().unwrap().to_string()
}

/// An open Store issue in the home project's set.
fn member(t: &TempRoot) -> String {
    let node = t
        .root
        .read(|l| snapshot::repo_node_of(l.conn(), STORE))
        .unwrap()
        .unwrap();
    let (_, m) = t
        .root
        .read(|l| snapshot::membership(l.conn(), "home", &node))
        .unwrap()
        .unwrap();
    m.issues.into_iter().next().unwrap()
}

/// How many by-id reads asked for `id`.
fn reads_by_id(gh: &Replay, id: &str) -> usize {
    gh.calls()
        .iter()
        .filter(|c| c.starts_with("nodes:") && c.contains(&format!("\"{id}\"")))
        .count()
}

fn requests(t: &TempRoot) -> Vec<Value> {
    t.root
        .read(|l| ledger::events_by_kind(l.conn(), Kind::Request))
        .unwrap()
        .into_iter()
        .map(|e| e.data)
        .collect()
}

/// Two reads, so the third is incremental with a watermark past every member.
fn settled() -> (TempRoot, String) {
    let t = fixture_root();
    snap(&t, &gh(), at(0));
    snap(&t, &gh(), at(10));
    let x = member(&t);
    (t, x)
}

#[test]
fn an_unrequested_member_is_not_read_by_id_on_an_incremental_read() {
    // The control for the tests below: without a request the item is never read by id.
    let (t, x) = settled();
    let g = gh();
    snap(&t, &g, at(20));
    assert_eq!(reads_by_id(&g, &x), 0, "{:?}", g.calls());
}

#[test]
fn a_requested_item_is_read_by_id_on_the_next_read_and_not_after() {
    let (t, x) = settled();
    let r = ask(&t, Some(&alias_of(&t, &x)), at(15)).unwrap();
    assert_eq!(r.items, vec![x.clone()]);
    let g = gh();
    snap(&t, &g, at(20));
    assert_eq!(reads_by_id(&g, &x), 1, "{:?}", g.calls());
    // Served: the read started after the request.
    let g = gh();
    snap(&t, &g, at(30));
    assert_eq!(reads_by_id(&g, &x), 0, "{:?}", g.calls());
}

#[test]
fn many_requests_before_a_run_cause_one_read_of_the_item() {
    let (t, x) = settled();
    let alias = alias_of(&t, &x);
    ask(&t, Some(&alias), at(11)).unwrap();
    ask(&t, Some(&alias), at(12)).unwrap();
    ask(&t, None, at(13)).unwrap();
    ask(&t, Some(&alias.to_uppercase()), at(14)).unwrap();
    let g = gh();
    snap(&t, &g, at(20));
    assert_eq!(reads_by_id(&g, &x), 1, "{:?}", g.calls());
    let g = gh();
    snap(&t, &g, at(30));
    assert_eq!(reads_by_id(&g, &x), 0, "{:?}", g.calls());
}

#[test]
fn a_request_stays_waiting_while_its_repos_read_fails() {
    let (t, x) = settled();
    ask(&t, Some(&alias_of(&t, &x)), at(15)).unwrap();
    let s = Switch::new();
    s.set(|req, v| {
        if is_search_of(req, STORE) {
            Err(Error::GitHub("simulated failure: Store search".into()))
        } else {
            v
        }
    });
    snap(&t, &s.gh(), at(20));
    let g = gh();
    snap(&t, &g, at(30));
    assert_eq!(reads_by_id(&g, &x), 1, "{:?}", g.calls());
}

#[test]
fn a_request_made_after_a_read_started_waits_for_the_next_one() {
    let (t, x) = settled();
    // The read at 20 records first; a request stamped at 25 is newer than that read's start.
    snap(&t, &gh(), at(20));
    ask(&t, Some(&alias_of(&t, &x)), at(25)).unwrap();
    let g = gh();
    snap(&t, &g, at(30));
    assert_eq!(reads_by_id(&g, &x), 1, "{:?}", g.calls());
}

#[test]
fn an_untracked_issue_is_refused_and_nothing_is_recorded() {
    let (t, _) = settled();
    let before = requests(&t).len();
    for bad in ["yuvalrakavy/Store#999999", "yuvalrakavy/nonesuch#1"] {
        let e = ask(&t, Some(bad), at(15)).unwrap_err();
        assert!(matches!(e, Error::Refused(_)), "{bad}: {e:?}");
        assert!(e.to_string().contains("not tracked by project home"), "{bad}: {e}");
    }
    for bad in ["Store#1", "yuvalrakavy/Store", "yuvalrakavy/Store#x", "#12"] {
        let e = ask(&t, Some(bad), at(15)).unwrap_err();
        assert!(matches!(e, Error::Refused(_)), "{bad}: {e:?}");
        assert!(e.to_string().contains("owner/repo#N"), "{bad}: {e}");
    }
    assert_eq!(requests(&t).len(), before);
}

#[test]
fn a_global_request_is_recorded_with_no_items() {
    let (t, _) = settled();
    let r = ask(&t, None, at(15)).unwrap();
    assert!(r.items.is_empty());
    let all = requests(&t);
    let d = &all.last().unwrap()["document"];
    assert_eq!(d["kind"], "sync");
    assert!(d.get("items").is_none(), "{d}");
}

#[test]
fn the_reply_names_the_configured_task() {
    let (t, x) = settled();
    let alias = alias_of(&t, &x);
    assert_eq!(ask(&t, None, at(15)).unwrap().task, None);
    let path = t.path().join("projects/home.toml");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        text.replacen("history_days", "sync_task = \"home-sync\"\nhistory_days", 1),
    )
    .unwrap();
    assert_eq!(
        ask(&t, Some(&alias), at(16)).unwrap().task.as_deref(),
        Some("home-sync")
    );
}

fn sync_page_doc(id: &str, items: Option<Vec<String>>) -> Value {
    let mut d = json!({
        "id": id,
        "project": "home",
        "author": "user_owner",
        "created_at": "2026-01-01T00:00:00Z",
        "kind": "sync",
    });
    if let Some(items) = items {
        d["items"] = json!(items);
    }
    d
}

#[test]
fn a_page_sync_request_is_ingested_once_and_read_in_that_cycle() {
    let (t, x) = settled();
    let pages = Pages {
        requests: vec![
            sync_page_doc("R_refresh_1", Some(vec![x.clone()])),
            sync_page_doc("R_sync_all", None),
        ],
        ..Pages::default()
    };
    let g = gh();
    let r = cycle::cycle(&t.root, &g, "home", &pages, &move || at(20), PAGE).unwrap();
    assert_eq!(r.ingest.requests, 2, "{:?}", r.ingest);
    assert!(r.ingest.skipped.is_empty(), "{:?}", r.ingest.skipped);
    assert_eq!(reads_by_id(&g, &x), 1, "{:?}", g.calls());
    // The page still lists both documents next run: nothing new, and nothing read by id.
    let g = gh();
    let r = cycle::cycle(&t.root, &g, "home", &pages, &move || at(30), PAGE).unwrap();
    assert_eq!(r.ingest.requests, 0);
    assert_eq!(reads_by_id(&g, &x), 0, "{:?}", g.calls());
}
