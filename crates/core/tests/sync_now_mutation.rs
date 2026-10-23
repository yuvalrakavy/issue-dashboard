//! Gaps a mutation pass found in the sync-now tests: each test here fails on a mutant of
//! `sync.rs` / `snapshot::plan` that `sync_now.rs` let survive.

mod common;

use chrono::{DateTime, Utc};
use common::TempRoot;
use common::fixture::*;
use issue_dashboard_core::config::ConfigSet;
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::github::Replay;
use issue_dashboard_core::ingest::Pages;
use issue_dashboard_core::sync::{self, Requested};
use issue_dashboard_core::{Result, cycle, ledger, snapshot};
use serde_json::{Value, json};

fn snap_in(t: &TempRoot, project: &str, gh: &Replay, now: DateTime<Utc>) {
    cycle::snapshot(&t.root, gh, project, &move || now, PAGE).unwrap();
}

fn ask_in(t: &TempRoot, project: &'static str, issue: Option<&str>, now: DateTime<Utc>) -> Result<Requested> {
    let dir = t.path().to_path_buf();
    t.root.mutate_with_clock(
        move || now,
        |tx| {
            let config = ConfigSet::load(&dir)?;
            config.record(tx)?;
            sync::request(tx, &config, project, issue)
        },
    )
}

fn members_of(t: &TempRoot, project: &str, repo: &str) -> snapshot::MembershipData {
    let node = t
        .root
        .read(|l| snapshot::repo_node_of(l.conn(), repo))
        .unwrap()
        .unwrap();
    t.root
        .read(|l| snapshot::membership(l.conn(), project, &node))
        .unwrap()
        .unwrap()
        .1
}

fn alias_of(t: &TempRoot, id: &str) -> String {
    let e = t
        .root
        .read(|l| ledger::latest(l.conn(), id, Kind::Observed))
        .unwrap()
        .unwrap();
    e.data["alias"].as_str().unwrap().to_string()
}

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

fn settled() -> TempRoot {
    let t = fixture_root();
    snap_in(&t, "home", &gh(), at(0));
    snap_in(&t, "home", &gh(), at(10));
    t
}

#[test]
fn malformed_aliases_with_extra_or_empty_parts_get_the_shape_message() {
    let t = settled();
    for bad in ["a/b/c#1", "/Store#1", "yuvalrakavy/#1", "yuvalrakavy/Store#", "//#1"] {
        let e = ask_in(&t, "home", Some(bad), at(15)).unwrap_err();
        assert!(e.to_string().contains("owner/repo#N"), "{bad}: {e}");
    }
}

#[test]
fn the_same_request_at_the_same_instant_is_a_repeat_and_a_new_one_is_not() {
    let t = settled();
    assert!(!ask_in(&t, "home", None, at(15)).unwrap().repeat);
    assert!(ask_in(&t, "home", None, at(15)).unwrap().repeat);
    assert!(!ask_in(&t, "home", None, at(16)).unwrap().repeat);
}

#[test]
fn a_request_is_authored_by_sync_now_not_the_owner() {
    let t = settled();
    ask_in(&t, "home", None, at(15)).unwrap();
    let all = requests(&t);
    let d = &all.last().unwrap()["document"];
    assert_eq!(d["author"], sync::AUTHOR);
    assert_eq!(sync::AUTHOR, "sync-now");
}

#[test]
fn a_pull_request_in_a_set_can_be_requested() {
    let t = settled();
    let m = members_of(&t, "home", "rhaiscript/rhai");
    let pr = m.pull_requests.first().expect("the fixture has a rhai pull request").clone();
    let r = ask_in(&t, "home", Some(&alias_of(&t, &pr)), at(15)).unwrap();
    assert_eq!(r.items, vec![pr]);
}

#[test]
fn requests_for_different_items_before_a_run_are_all_read() {
    let t = settled();
    let m = members_of(&t, "home", STORE);
    let (a, b) = (m.issues[0].clone(), m.issues[1].clone());
    ask_in(&t, "home", Some(&alias_of(&t, &a)), at(11)).unwrap();
    ask_in(&t, "home", Some(&alias_of(&t, &b)), at(12)).unwrap();
    let g = gh();
    snap_in(&t, "home", &g, at(20));
    assert_eq!(reads_by_id(&g, &a), 1, "{:?}", g.calls());
    assert_eq!(reads_by_id(&g, &b), 1, "{:?}", g.calls());
}

#[test]
fn the_same_item_asked_again_after_it_was_served_is_read_again() {
    let t = settled();
    let x = members_of(&t, "home", STORE).issues[0].clone();
    let alias = alias_of(&t, &x);
    ask_in(&t, "home", Some(&alias), at(15)).unwrap();
    snap_in(&t, "home", &gh(), at(20));
    ask_in(&t, "home", Some(&alias), at(25)).unwrap();
    let g = gh();
    snap_in(&t, "home", &g, at(30));
    assert_eq!(reads_by_id(&g, &x), 1, "{:?}", g.calls());
}

#[test]
fn another_projects_request_does_not_make_this_project_read_the_item() {
    let t = fixture_root();
    snap_in(&t, "home", &gh(), at(0));
    snap_in(&t, "kernel", &gh(), at(1));
    snap_in(&t, "home", &gh(), at(10));
    snap_in(&t, "kernel", &gh(), at(11));
    let k = members_of(&t, "kernel", STORE).issues[0].clone();
    ask_in(&t, "kernel", Some(&alias_of(&t, &k)), at(15)).unwrap();
    let g = gh();
    snap_in(&t, "home", &g, at(20));
    assert_eq!(reads_by_id(&g, &k), 0, "{:?}", g.calls());
    // and it is still waiting for its own project
    let g = gh();
    snap_in(&t, "kernel", &g, at(21));
    assert_eq!(reads_by_id(&g, &k), 1, "{:?}", g.calls());
}

#[test]
fn two_projects_may_each_ask_at_the_same_instant() {
    let t = fixture_root();
    snap_in(&t, "home", &gh(), at(0));
    snap_in(&t, "kernel", &gh(), at(1));
    let a = ask_in(&t, "home", None, at(15)).unwrap();
    let b = ask_in(&t, "kernel", None, at(15)).unwrap();
    assert!(!a.repeat && !b.repeat);
    assert_ne!(a.request, b.request);
}

#[test]
fn a_triage_request_naming_items_does_not_make_the_next_read_read_them() {
    let t = settled();
    let x = members_of(&t, "home", STORE).issues[0].clone();
    let pages = Pages {
        requests: vec![json!({
            "id": "R_triage_1",
            "project": "home",
            "author": "user_owner",
            "created_at": "2026-01-01T00:00:00Z",
            "kind": "triage",
            "items": [x],
        })],
        ..Pages::default()
    };
    let g = gh();
    let r = cycle::cycle(&t.root, &g, "home", &pages, &move || at(20), PAGE).unwrap();
    assert_eq!(r.ingest.requests, 1, "{:?}", r.ingest);
    assert_eq!(reads_by_id(&g, &x), 0, "{:?}", g.calls());
}
