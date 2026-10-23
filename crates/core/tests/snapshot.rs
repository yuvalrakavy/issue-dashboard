//! The observe step (chapter 2, 10.4 step 2) against recorded real GitHub responses (2.7).

mod common;

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use common::TempRoot;
use common::fixture::*;
use issue_dashboard_core::config::ConfigSet;
use issue_dashboard_core::cycle;
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::github::{GitHub, Replay, Request};
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

fn watermark(t: &TempRoot, repo: &str) -> Option<DateTime<Utc>> {
    let node = node_of(t, repo);
    t.root.read(|l| snapshot::watermark(l.conn(), &node)).unwrap()
}

fn observations(t: &TempRoot, id: &str) -> Vec<ObservedData> {
    t.root
        .read(|l| ledger::events_of(l.conn(), id, Kind::Observed))
        .unwrap()
        .into_iter()
        .map(|e| serde_json::from_value(e.data).unwrap())
        .collect()
}

fn gone(t: &TempRoot, id: &str) -> usize {
    t.root
        .read(|l| ledger::events_of(l.conn(), id, Kind::ItemGone))
        .unwrap()
        .len()
}

/// An open member of `repo` in `project`'s set, chosen deterministically.
fn some_member(t: &TempRoot, project: &str, repo: &str) -> String {
    members(t, project, repo)
        .into_iter()
        .find(|id| id.starts_with("I_") && observations(t, id).last().unwrap().content["state"] == "open")
        .unwrap()
}

fn fail(what: &str) -> issue_dashboard_core::Result<Value> {
    Err(Error::GitHub(format!("simulated failure: {what}")))
}

#[test]
fn a_first_read_is_full_and_records_each_set_with_its_watermark() {
    let t = fixture_root();
    let gh = gh();
    let r = snap(&t, &gh, "home", at(0));
    for repo in [STORE, AIRZONED, RHAI] {
        let x = report(&r, repo);
        assert_eq!(x.error, None, "{repo}");
        assert_eq!(x.mode.as_deref(), Some("full"), "{repo}");
        assert_eq!(x.full_reason.as_deref(), Some("first read"));
        assert_eq!(watermark(&t, repo), Some(at(0)), "{repo}");
        let m = members(&t, "home", repo);
        assert!(!m.is_empty(), "{repo} has no members");
        for id in &m {
            assert!(
                !observations(&t, id).is_empty(),
                "member {id} of {repo} was never read in full"
            );
        }
    }
    // Store: every open issue, whatever its labels.
    assert!(members(&t, "home", STORE).len() > 100);
    // rhai: the filter, plus each `also` number.
    let rhai = members(&t, "home", RHAI);
    let aliases: BTreeSet<String> = rhai
        .iter()
        .map(|id| observations(&t, id).last().unwrap().alias.clone())
        .collect();
    assert!(aliases.contains("rhaiscript/rhai#1193"), "{aliases:?}");
    assert!(aliases.contains("rhaiscript/rhai#1194"), "{aliases:?}");
    // Node ids are in the current format (the X-Github-Next-Global-ID header).
    assert!(node_of(&t, RHAI).starts_with("R_"));
}

#[test]
fn a_second_read_is_incremental_and_reads_nothing_unchanged() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let gh = gh();
    let r = snap(&t, &gh, "home", at(10));
    for repo in [STORE, AIRZONED, RHAI] {
        let x = report(&r, repo);
        assert_eq!(x.mode.as_deref(), Some("incremental"), "{repo}");
        assert_eq!(x.recorded, 0, "{repo}: {x:?}");
        assert_eq!(x.entered, 0, "{repo}");
        assert_eq!(watermark(&t, repo), Some(at(10)));
    }
    assert!(
        !gh.calls().iter().any(|c| c.starts_with("nodes:")),
        "an incremental read with nothing entering reads no node by id: {:?}",
        gh.calls()
    );
}

#[test]
fn an_issue_leaving_a_filter_leaves_the_set_but_keeps_its_history() {
    let t = fixture_root();
    let s = Switch::new();
    snap(&t, &s.gh(), "kernel", at(0));
    let x = some_member(&t, "kernel", STORE);
    let before = observations(&t, &x).len();
    // It loses the kernel label: the filtered search stops returning it.
    let id = x.clone();
    s.set(move |req, v| {
        if is_search_of(req, STORE) {
            v.map(|v| drop_from_search(v, &id))
        } else {
            v
        }
    });
    let r = snap(&t, &s.gh(), "kernel", at(10));
    assert_eq!(report(&r, STORE).left, 1);
    assert!(!members(&t, "kernel", STORE).contains(&x));
    assert_eq!(
        observations(&t, &x).len(),
        before,
        "leaving a set is not a change of the item"
    );
    assert_eq!(gone(&t, &x), 0, "an item that left a filter is not gone");
    // ... and the project's outbox no longer carries it.
    let out = cycle::cycle(&t.root, &s.gh(), "kernel", &no_pages(), &|| at(20), PAGE).unwrap();
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&out.outbox).unwrap()).unwrap();
    assert!(doc["issues"].as_array().unwrap().iter().all(|i| i["id"] != x.as_str()));
    assert!(!doc["issues"].as_array().unwrap().is_empty());
}

#[test]
fn an_item_entering_a_set_unchanged_since_the_watermark_is_read_in_full() {
    let t = fixture_root();
    let s = Switch::new();
    // Find a kernel item, then start over with it outside the set.
    let probe = fixture_root();
    snap(&probe, &gh(), "kernel", at(0));
    let x = some_member(&probe, "kernel", STORE);
    let id = x.clone();
    s.set(move |req, v| {
        if is_search_of(req, STORE) {
            v.map(|v| drop_from_search(v, &id))
        } else {
            v
        }
    });
    snap(&t, &s.gh(), "kernel", at(0));
    assert!(
        observations(&t, &x).is_empty(),
        "the fixture must start with the item never read"
    );
    // It gains the label; it was not updated since the watermark, so no since-list returns it.
    s.clear();
    let gh = s.gh();
    let r = snap(&t, &gh, "kernel", at(10));
    assert_eq!(report(&r, STORE).mode.as_deref(), Some("incremental"));
    assert_eq!(report(&r, STORE).entered, 1);
    assert!(members(&t, "kernel", STORE).contains(&x));
    assert_eq!(observations(&t, &x).len(), 1, "an entering item is read by node id");
}

#[test]
fn a_read_that_fails_mid_pagination_changes_nothing_but_the_error() {
    let t = fixture_root();
    let s = Switch::new();
    snap(&t, &s.gh(), "home", at(0));
    let m0 = members(&t, "home", STORE);
    let events0 = t.root.read(|l| ledger::max_seq(l.conn())).unwrap();
    // The second page of Store's open-issue search fails.
    s.set(|req: &Request, v| {
        if is_search_of(req, STORE) && !req.variables["after"].is_null() {
            return fail("second page");
        }
        v
    });
    let r = snap(&t, &s.gh(), "home", at(10));
    let store = report(&r, STORE);
    assert!(
        store.error.as_deref().is_some_and(|e| e.contains("second page")),
        "{store:?}"
    );
    assert_eq!(store.mode, None);
    assert_eq!(
        watermark(&t, STORE),
        Some(at(0)),
        "a partial read never advances the watermark"
    );
    assert_eq!(members(&t, "home", STORE), m0, "a partial read never changes the set");
    let state = t
        .root
        .read(|l| snapshot::read_state(l.conn(), "home", STORE))
        .unwrap()
        .unwrap();
    assert!(
        state
            .good_at
            .unwrap()
            .starts_with(&at(0).format("%Y-%m-%dT%H:%M").to_string())
    );
    assert!(state.error.is_some());
    // The other repos were read.
    assert_eq!(report(&r, AIRZONED).error, None);
    assert_eq!(watermark(&t, AIRZONED), Some(at(10)));
    // Nothing of Store was recorded.
    let new: Vec<_> = t
        .root
        .read(|l| ledger::events_after(l.conn(), events0))
        .unwrap()
        .into_iter()
        .filter(|e| e.kind != Kind::Config && e.kind != Kind::Observed)
        .collect();
    assert!(new.is_empty(), "{new:?}");

    // The outbox keeps Store's last good items, marked stale.
    let out = cycle::cycle(&t.root, &s.gh(), "home", &no_pages(), &|| at(15), PAGE).unwrap();
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(&out.outbox).unwrap()).unwrap();
    let meta = doc["meta"]["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["repo"] == STORE)
        .unwrap()
        .clone();
    assert_eq!(meta["stale"], true);
    assert!(meta["error"].is_string());
    let store_items: Vec<&Value> = doc["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["alias"].as_str().unwrap().starts_with("yuvalrakavy/Store#"))
        .collect();
    assert!(store_items.len() > 100, "a failed read never removes issues");
    assert!(store_items.iter().all(|i| i["stale"] == true));
}

#[test]
fn a_failed_repo_query_keeps_the_repo_and_reports_it() {
    let t = fixture_root();
    let s = Switch::new();
    snap(&t, &s.gh(), "home", at(0));
    s.set(|req: &Request, v| {
        if req.name == "repo" && req.variables["name"] == "airzoned" {
            fail("repo")
        } else {
            v
        }
    });
    let r = snap(&t, &s.gh(), "home", at(10));
    assert!(report(&r, AIRZONED).error.is_some());
    assert!(!members(&t, "home", AIRZONED).is_empty());
    assert_eq!(watermark(&t, AIRZONED), Some(at(0)));
}

/// Inserts `node` (a real node, edited) into a since-list response of `repo`'s issues.
fn with_in_since_list(req: &Request, v: Value, node: &Value) -> Value {
    let mut v = v;
    if req.name == "issues_since" {
        let nodes = v["data"]["repository"]["issues"]["nodes"].as_array_mut().unwrap();
        nodes.retain(|n| n["id"] != node["id"]);
        nodes.push(node.clone());
    }
    v
}

#[test]
fn of_two_projects_reading_one_repo_the_stale_fetch_is_discarded() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    snap(&t, &gh(), "kernel", at(0));
    let x = some_member(&t, "kernel", STORE);
    let lib = library();
    let mut older = real_node(&lib, &x).unwrap();
    older["title"] = json!("read by kernel at 10");
    older["updatedAt"] = json!(at(9).to_rfc3339());
    let mut newer = older.clone();
    newer["title"] = json!("read by home at 20");
    newer["updatedAt"] = json!(at(19).to_rfc3339());

    let config = ConfigSet::load(t.path()).unwrap();
    let fetch = |project: &str, now: DateTime<Utc>, node: Value| {
        let gh = gh_with(move |req, v| v.map(|v| with_in_since_list(req, v, &node)));
        let plan = t
            .root
            .read(|l| snapshot::plan(l.conn(), &config, project, now))
            .unwrap();
        snapshot::fetch(&gh as &dyn GitHub, &plan, &move || now, PAGE)
    };
    let a = fetch("kernel", at(10), older);
    let b = fetch("home", at(20), newer);
    // The newer fetch commits first; the older one commits after it.
    t.root.mutate(at(21), |tx| snapshot::record(tx, "home", &b)).unwrap();
    let ra = t.root.mutate(at(22), |tx| snapshot::record(tx, "kernel", &a)).unwrap();
    assert!(report(&ra, STORE).stale >= 1, "{ra:?}");
    assert_eq!(
        observations(&t, &x).last().unwrap().content["title"],
        "read by home at 20"
    );
    assert_eq!(
        watermark(&t, STORE),
        Some(at(20)),
        "an older read never moves the watermark back"
    );
}

#[test]
fn an_older_read_of_the_same_project_does_not_replace_its_set() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let y = some_member(&t, "home", STORE);
    let config = ConfigSet::load(t.path()).unwrap();
    let id = y.clone();
    let without_y = gh_with(move |req, v| {
        if is_search_of(req, STORE) {
            v.map(|v| drop_from_search(v, &id))
        } else {
            v
        }
    });
    let plan = |now| t.root.read(|l| snapshot::plan(l.conn(), &config, "home", now)).unwrap();
    let old = snapshot::fetch(&without_y as &dyn GitHub, &plan(at(10)), &|| at(10), PAGE);
    let new = snapshot::fetch(&gh() as &dyn GitHub, &plan(at(20)), &|| at(20), PAGE);
    t.root.mutate(at(21), |tx| snapshot::record(tx, "home", &new)).unwrap();
    let r = t.root.mutate(at(22), |tx| snapshot::record(tx, "home", &old)).unwrap();
    assert!(report(&r, STORE).superseded);
    assert!(
        members(&t, "home", STORE).contains(&y),
        "the older read's set must not win"
    );
}

#[test]
fn a_renamed_repo_keeps_its_items_and_their_history() {
    let t = fixture_root();
    let s = Switch::new();
    snap(&t, &s.gh(), "home", at(0));
    let m0 = members(&t, "home", STORE);
    let x = some_member(&t, "home", STORE);
    let old_alias = observations(&t, &x).last().unwrap().alias.clone();
    // GitHub follows the old name to the renamed repo, and reports the new name everywhere.
    let lib = library();
    s.set(move |req: &Request, v| {
        let v = match v {
            Ok(v) => v,
            // A request under the new name: serve the recording made under the old one.
            Err(_) => {
                let mut vars = req.variables.clone();
                rename(&mut vars, "StoreRenamed", "Store");
                if let Some(q) = vars["q"].as_str() {
                    vars["q"] = json!(q.replace("yuvalrakavy/StoreRenamed", STORE));
                }
                lib.graphql(&Request {
                    name: req.name,
                    query: req.query,
                    variables: vars,
                })?
            }
        };
        let mut v = v;
        rename(&mut v, STORE, "yuvalrakavy/StoreRenamed");
        Ok(v)
    });
    let r = snap(&t, &s.gh(), "home", at(10));
    assert_eq!(report(&r, STORE).error, None, "{r:?}");
    assert_eq!(
        members(&t, "home", STORE),
        m0,
        "the renamed repo's items are the same items"
    );
    // A change of alias alone is recorded on the next read of the item; its history stays on
    // the same node id.
    s.set({
        let lib = library();
        let x = x.clone();
        move |req: &Request, v| {
            let v = match v {
                Ok(v) => v,
                Err(_) => {
                    let mut vars = req.variables.clone();
                    rename(&mut vars, "StoreRenamed", "Store");
                    if let Some(q) = vars["q"].as_str() {
                        vars["q"] = json!(q.replace("yuvalrakavy/StoreRenamed", STORE));
                    }
                    lib.graphql(&Request {
                        name: req.name,
                        query: req.query,
                        variables: vars,
                    })?
                }
            };
            let mut node = real_node(&lib, &x).unwrap();
            node["updatedAt"] = json!(at(19).to_rfc3339());
            let mut v = with_in_since_list(req, v, &node);
            rename(&mut v, STORE, "yuvalrakavy/StoreRenamed");
            Ok(v)
        }
    });
    snap(&t, &s.gh(), "home", at(20));
    let obs = observations(&t, &x);
    assert_eq!(obs.first().unwrap().alias, old_alias);
    assert_eq!(obs.last().unwrap().alias, old_alias.replace("Store#", "StoreRenamed#"));
}

#[test]
fn a_transferred_issue_keeps_its_history_by_node_id() {
    let t = fixture_root();
    let s = Switch::new();
    snap(&t, &s.gh(), "home", at(0));
    let x = some_member(&t, "home", STORE);
    let old_alias = observations(&t, &x).last().unwrap().alias.clone();
    // Moved to airzoned as #999: Store's search no longer returns it, and reading it by node
    // id names its new repo.
    let id = x.clone();
    s.set(move |req: &Request, v| {
        let mut v = v?;
        if is_search_of(req, STORE) {
            v = drop_from_search(v, &id);
        }
        if req.name == "nodes"
            && let Some(nodes) = v["data"]["nodes"].as_array_mut()
        {
            for n in nodes.iter_mut().filter(|n| n["id"] == id.as_str()) {
                n["repository"]["nameWithOwner"] = json!(AIRZONED);
                n["number"] = json!(999);
            }
        }
        Ok(v)
    });
    // The daily full read checks every item that left the set.
    let r = snap(&t, &s.gh(), "home", at(25 * 60));
    assert_eq!(report(&r, STORE).mode.as_deref(), Some("full"));
    assert_eq!(report(&r, STORE).full_reason.as_deref(), Some("daily"));
    assert_eq!(gone(&t, &x), 0, "a transferred issue is not gone");
    let obs = observations(&t, &x);
    assert_eq!(obs.first().unwrap().alias, old_alias);
    assert_eq!(obs.last().unwrap().alias, "yuvalrakavy/airzoned#999");
}

#[test]
fn a_search_hit_that_now_belongs_to_another_repo_is_not_a_member() {
    let t = fixture_root();
    let s = Switch::new();
    snap(&t, &s.gh(), "home", at(0));
    let x = some_member(&t, "home", STORE);
    // Transferred, but the search index still lists it under Store.
    let id = x.clone();
    s.set(move |req: &Request, v| {
        let mut v = v?;
        if is_search_of(req, STORE)
            && let Some(nodes) = v["data"]["search"]["nodes"].as_array_mut()
        {
            for n in nodes.iter_mut().filter(|n| n["id"] == id.as_str()) {
                assert!(
                    n["repository"]["nameWithOwner"].is_string(),
                    "the search reads each hit's repo: {n}"
                );
                n["repository"]["nameWithOwner"] = json!(AIRZONED);
            }
        }
        Ok(v)
    });
    let r = snap(&t, &s.gh(), "home", at(10));
    assert_eq!(report(&r, STORE).left, 1);
    assert!(!members(&t, "home", STORE).contains(&x));
}

#[test]
fn the_daily_full_read_records_item_gone_and_an_incremental_one_does_not() {
    let t = fixture_root();
    let s = Switch::new();
    snap(&t, &s.gh(), "home", at(0));
    let x = some_member(&t, "home", STORE);
    // Deleted: no search returns it and its node id resolves to nothing.
    let lib = library();
    let id = x.clone();
    s.set(move |req: &Request, v| {
        if req.name == "nodes" {
            let mut req2 = req.variables.clone();
            req2["ids"].as_array_mut().unwrap().retain(|i| i != id.as_str());
            let asked: Vec<String> = req.variables["ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i.as_str().unwrap().to_string())
                .collect();
            let nodes: Vec<Value> = asked
                .iter()
                .map(|a| {
                    if a == &id {
                        Value::Null
                    } else {
                        real_node(&lib, a).unwrap_or(Value::Null)
                    }
                })
                .collect();
            let pos = asked.iter().position(|a| a == &id);
            let mut resp = json!({"data": {"nodes": nodes}});
            if let Some(p) = pos {
                resp["errors"] = json!([{"type": "NOT_FOUND", "path": ["nodes", p], "message": "gone"}]);
            }
            return Ok(resp);
        }
        if is_search_of(req, STORE) {
            return v.map(|v| drop_from_search(v, &id));
        }
        v
    });
    snap(&t, &s.gh(), "home", at(10));
    assert_eq!(gone(&t, &x), 0, "an incremental read does not look for gone items");
    let r = snap(&t, &s.gh(), "home", at(25 * 60));
    assert_eq!(report(&r, STORE).full_reason.as_deref(), Some("daily"));
    // Left at the incremental read, so the daily read still checks it: previous - set is empty
    // by then, which is why the plan carries what left since the last full read.
    assert_eq!(gone(&t, &x), 1, "the daily full read confirms the item is gone");
}

#[test]
fn an_also_number_that_does_not_exist_is_skipped_and_reported() {
    let t = fixture_root();
    let s = Switch::new();
    s.set(|req: &Request, v| {
        if req.name == "number" && req.variables["number"] == 1194 {
            return Ok(json!({
                "data": {"repository": {"issueOrPullRequest": null}},
                "errors": [{"type": "NOT_FOUND", "path": ["repository", "issueOrPullRequest"], "message": "no such number"}]
            }));
        }
        v
    });
    let r = snap(&t, &s.gh(), "home", at(0));
    assert_eq!(report(&r, RHAI).error, None);
    assert_eq!(report(&r, RHAI).skipped_also, vec![1194]);
}

#[test]
fn a_view_change_forces_a_full_read() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let p = t.path().join("projects/home.toml");
    let text = std::fs::read_to_string(&p)
        .unwrap()
        .replace("also      = [1193, 1194]", "also      = [1193]");
    std::fs::write(&p, text).unwrap();
    let r = snap(&t, &gh(), "home", at(10));
    assert_eq!(report(&r, RHAI).mode.as_deref(), Some("full"));
    assert_eq!(
        report(&r, RHAI).full_reason.as_deref(),
        Some("view or translation changed")
    );
    assert_eq!(report(&r, STORE).mode.as_deref(), Some("incremental"));
}

#[test]
fn a_project_that_tracks_no_repo_is_refused() {
    let t = fixture_root();
    let r = cycle::snapshot(&t.root, &gh(), "nosuch", &|| at(0), PAGE);
    assert!(matches!(r, Err(Error::Refused(_))), "{r:?}");
}
