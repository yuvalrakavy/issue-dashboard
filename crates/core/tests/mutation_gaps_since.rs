//! Which of the items a read returns the ledger keeps, and which repo a name stands for
//! (the pre-merge mutation pass).

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use common::TempRoot;
use common::fixture::*;
use issue_dashboard_core::config::ConfigSet;
use issue_dashboard_core::cycle;
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::github::{Replay, Request};
use issue_dashboard_core::ledger;
use issue_dashboard_core::observe::{ItemKind, ObservedData};
use issue_dashboard_core::snapshot::{self, RepoReport};
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

fn title(t: &TempRoot, id: &str) -> String {
    observations(t, id).last().unwrap().content["title"]
        .as_str()
        .unwrap()
        .to_string()
}

fn open_members(t: &TempRoot, project: &str, repo: &str) -> Vec<String> {
    members(t, project, repo)
        .into_iter()
        .filter(|id| {
            let o = observations(t, id);
            o.last()
                .is_some_and(|o| o.item == ItemKind::Issue && o.content["state"] == "open")
        })
        .collect()
}

fn sql(t: &TempRoot, sql: &str) {
    let c = rusqlite::Connection::open(t.path().join("ledger.sqlite")).unwrap();
    c.execute_batch(sql).unwrap();
}

/// Replaces the node with this id in a list response, or adds it.
fn put(list: &mut Vec<Value>, node: &Value) {
    list.retain(|n| n["id"] != node["id"]);
    list.push(node.clone());
}

fn changed(lib: &Replay, id: &str, new_title: &str, minutes: i64) -> Value {
    let mut n = real_node(lib, id).unwrap();
    n["title"] = json!(new_title);
    n["updatedAt"] = json!(at(minutes).to_rfc3339());
    n
}

#[test]
fn a_full_read_does_not_ask_again_for_what_the_update_list_already_returned_and_a_vanished_member_leaves() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let ids = open_members(&t, "home", STORE);
    let (vanishes, updated) = (ids[0].clone(), ids[1].clone());
    let lib = library();
    let new = changed(&lib, &updated, "updated while we looked", 2);
    let asked = Arc::new(Mutex::new(Vec::<String>::new()));
    let log = asked.clone();
    let gone = vanishes.clone();
    let gh = gh_with(move |req: &Request, v| {
        let mut v = v?;
        if req.name == "issues_since" {
            put(v["data"]["repository"]["issues"]["nodes"].as_array_mut().unwrap(), &new);
        }
        if req.name == "nodes" {
            let asked_ids: Vec<String> = req.variables["ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i.as_str().unwrap().to_string())
                .collect();
            log.lock().unwrap().extend(asked_ids.clone());
            let nodes = v["data"]["nodes"].as_array_mut().unwrap();
            for (i, id) in asked_ids.iter().enumerate() {
                if *id == gone {
                    nodes[i] = Value::Null;
                }
            }
        }
        Ok(v)
    });
    // The daily full read: the set is read again in full, but what the update list returned
    // is not asked for by id; a member the search still finds but GitHub cannot resolve by id
    // leaves the set, and the full read confirms it gone.
    let r = snap(&t, &gh, "home", at(24 * 60));
    let store = report(&r, STORE);
    assert_eq!(store.mode.as_deref(), Some("full"), "{store:?}");
    assert_eq!(store.error, None);
    let asked = asked.lock().unwrap();
    assert!(
        asked.contains(&vanishes),
        "the member that was not updated is read by id"
    );
    assert!(
        !asked.contains(&updated),
        "an item the update list returned is not read twice"
    );
    assert_eq!(store.gone, 1, "{store:?}");
    assert_eq!(title(&t, &updated), "updated while we looked");
    assert!(!members(&t, "home", STORE).contains(&vanishes));
}

#[test]
fn a_member_github_cannot_resolve_is_recorded_gone_once_across_full_reads() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let vanishes = open_members(&t, "home", STORE)[0].clone();
    let gone = vanishes.clone();
    let gh = gh_with(move |req: &Request, v| {
        let mut v = v?;
        if req.name == "nodes" {
            let asked: Vec<String> = req.variables["ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i.as_str().unwrap().to_string())
                .collect();
            let nodes = v["data"]["nodes"].as_array_mut().unwrap();
            for (i, id) in asked.iter().enumerate() {
                if *id == gone {
                    nodes[i] = Value::Null;
                }
            }
        }
        Ok(v)
    });
    // The search keeps finding it: the next full read meets it as an entrant that does not
    // resolve, which is no member and is not recorded gone a second time.
    for (day, expect) in [(1, 1), (2, 0)] {
        let r = snap(&t, &gh, "home", at(day * 24 * 60));
        let store = report(&r, STORE);
        assert_eq!(store.mode.as_deref(), Some("full"), "day {day}: {store:?}");
        assert_eq!(store.gone, expect, "day {day}: {store:?}");
    }
    let events = t
        .root
        .read(|l| ledger::events_of(l.conn(), &vanishes, Kind::ItemGone))
        .unwrap();
    assert_eq!(events.len(), 1, "{events:?}");
}

#[test]
fn an_incremental_read_follows_pull_requests_as_well_as_issues() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let node = node_of(&t, RHAI);
    let (_, m) = t
        .root
        .read(|l| snapshot::membership(l.conn(), "home", &node))
        .unwrap()
        .unwrap();
    let pr = m.pull_requests.first().expect("rhai contributes pull requests").clone();
    let new = changed(&library(), &pr, "pull request edited", 2);
    let gh = gh_with(move |req: &Request, v| {
        let mut v = v?;
        if req.name == "prs_since" {
            put(
                v["data"]["repository"]["pullRequests"]["nodes"].as_array_mut().unwrap(),
                &new,
            );
        }
        Ok(v)
    });
    let r = snap(&t, &gh, "home", at(10));
    assert_eq!(report(&r, RHAI).mode.as_deref(), Some("incremental"));
    assert_eq!(title(&t, &pr), "pull request edited");
}

#[test]
fn an_item_that_left_the_set_is_not_observed_again_when_it_changes() {
    for kernel_stopped_tracking in [false, true] {
        let t = fixture_root();
        snap(&t, &gh(), "home", at(0));
        snap(&t, &gh(), "kernel", at(1));
        let kernel = open_members(&t, "kernel", STORE);
        let z = kernel[0].clone();
        let before = title(&t, &z);
        if kernel_stopped_tracking {
            // Kernel's recorded set stays in the ledger, but kernel no longer reads this repo,
            // so nothing it holds is kept up to date by home's reads.
            let p = t.path().join("projects/kernel.toml");
            let text = std::fs::read_to_string(&p).unwrap().replace(
                "repo   = \"yuvalrakavy/Store\"\nfilter = \"label:kernel\"",
                "repo   = \"yuvalrakavy/airzoned\"",
            );
            assert!(text.contains("airzoned"), "{text}");
            std::fs::write(&p, text).unwrap();
        }
        let new = changed(&library(), &z, "changed after it left", 3);
        let left = z.clone();
        let gh = gh_with(move |req: &Request, v| {
            let mut v = v?;
            if req.name == "search" {
                v["data"]["search"]["nodes"]
                    .as_array_mut()
                    .unwrap()
                    .retain(|n| n["id"] != left.as_str());
            }
            if req.name == "issues_since" {
                put(v["data"]["repository"]["issues"]["nodes"].as_array_mut().unwrap(), &new);
            }
            Ok(v)
        });
        let r = snap(&t, &gh, "home", at(10));
        let store = report(&r, STORE);
        assert_eq!(store.mode.as_deref(), Some("incremental"), "{store:?}");
        assert!(store.left >= 1, "{store:?}");
        assert!(!members(&t, "home", STORE).contains(&z));
        if kernel_stopped_tracking {
            assert_eq!(title(&t, &z), before, "no project tracks it any more");
        } else {
            // Kernel still tracks it, and the watermark this read moves is kernel's too.
            assert_eq!(title(&t, &z), "changed after it left");
        }
    }
}

#[test]
fn an_item_that_left_the_only_set_that_held_it_is_not_observed_again() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let z = open_members(&t, "home", STORE)[0].clone();
    let before = title(&t, &z);
    let new = changed(&library(), &z, "changed after it left", 3);
    let left = z.clone();
    let gh = gh_with(move |req: &Request, v| {
        let mut v = v?;
        if req.name == "search" {
            v["data"]["search"]["nodes"]
                .as_array_mut()
                .unwrap()
                .retain(|n| n["id"] != left.as_str());
        }
        if req.name == "issues_since" {
            put(v["data"]["repository"]["issues"]["nodes"].as_array_mut().unwrap(), &new);
        }
        Ok(v)
    });
    let r = snap(&t, &gh, "home", at(10));
    assert!(report(&r, STORE).left >= 1);
    assert!(!members(&t, "home", STORE).contains(&z));
    assert_eq!(title(&t, &z), before);
}

fn set_reads(t: &TempRoot, project: &str, repo: &str, node: &str, good_at: &str) {
    sql(
        t,
        &format!(
            "INSERT OR REPLACE INTO reads (project, repo, repo_node, good_at, attempt_at) VALUES ('{project}', '{repo}', '{node}', '{good_at}', '{good_at}')"
        ),
    );
}

#[test]
fn the_node_a_repo_name_stands_for_is_its_own_projects_then_the_latest_read_of_any() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let old = node_of(&t, STORE);
    // Another project read the name later and found another repo behind it.
    set_reads(
        &t,
        "elsewhere",
        &STORE.to_uppercase(),
        "R_newer",
        "2026-10-24T00:00:00Z",
    );
    set_reads(&t, "elsewhere2", STORE, "R_older", "2026-10-22T00:00:00Z");
    assert_eq!(
        node_of(&t, STORE),
        "R_newer",
        "the latest read wins, whatever the case of the name"
    );
    assert_eq!(node_of(&t, &STORE.to_lowercase()), "R_newer");
    // A project that has read it already keeps its own, however old.
    let config = ConfigSet::load(t.path()).unwrap();
    let plan = t
        .root
        .read(|l| snapshot::plan(l.conn(), &config, "home", at(5)))
        .unwrap();
    let home = plan.repos.iter().find(|r| r.view.repo == STORE).unwrap();
    assert_eq!(home.repo_node.as_deref(), Some(old.as_str()));
    // A project that never has, takes the latest read of any.
    let plan = t
        .root
        .read(|l| snapshot::plan(l.conn(), &config, "kernel", at(5)))
        .unwrap();
    let kernel = plan.repos.iter().find(|r| r.view.repo == STORE).unwrap();
    assert_eq!(kernel.repo_node.as_deref(), Some("R_newer"));
    assert_eq!(kernel.full_reason.as_deref(), Some("first read"));
    assert_eq!(kernel.watermark, None, "R_newer has no watermark of its own");
}

#[test]
fn without_any_read_the_node_comes_from_the_latest_observation_of_the_name() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let old = node_of(&t, STORE);
    // The same name behind another repo from now on.
    let gh = gh_with(|req: &Request, v| {
        let mut v = v?;
        if req.name == "repo" && req.variables["name"] == "Store" {
            v["data"]["repository"]["id"] = json!("R_recreated");
        }
        Ok(v)
    });
    snap(&t, &gh, "home", at(10));
    assert_eq!(node_of(&t, STORE), "R_recreated");
    sql(&t, "DELETE FROM reads");
    assert_eq!(
        node_of(&t, &STORE.to_lowercase()),
        "R_recreated",
        "the latest observation, in any case"
    );
    assert_ne!(old, "R_recreated");
    // And a plan made from it knows the repo.
    let config = ConfigSet::load(t.path()).unwrap();
    let plan = t
        .root
        .read(|l| snapshot::plan(l.conn(), &config, "home", at(20)))
        .unwrap();
    let store = plan.repos.iter().find(|r| r.view.repo == STORE).unwrap();
    assert_eq!(store.repo_node.as_deref(), Some("R_recreated"));
    assert_eq!(store.watermark, Some(at(10)));
    assert_eq!(store.full_reason.as_deref(), Some("first read"));
}

/// A replay that hides items from search results, as the test says: from every read, or from
/// every read but kernel's. Logs the ids it is asked to read by id.
struct Hidden {
    always: Arc<Mutex<BTreeSet<String>>>,
    from_home: Arc<Mutex<BTreeSet<String>>>,
    asked: Arc<Mutex<Vec<String>>>,
}

fn hiding() -> (Hidden, Replay) {
    let h = Hidden {
        always: Arc::default(),
        from_home: Arc::default(),
        asked: Arc::default(),
    };
    let (always, from_home, asked) = (h.always.clone(), h.from_home.clone(), h.asked.clone());
    let gh = gh_with(move |req: &Request, v| {
        let mut v = v?;
        if req.name == "search" {
            let kernel = req.variables["q"].as_str().unwrap().contains("label:kernel");
            let (a, f) = (always.lock().unwrap(), from_home.lock().unwrap());
            v["data"]["search"]["nodes"].as_array_mut().unwrap().retain(|n| {
                let id = n["id"].as_str().unwrap_or_default().to_string();
                !a.contains(&id) && (kernel || !f.contains(&id))
            });
        }
        if req.name == "nodes" {
            asked.lock().unwrap().extend(
                req.variables["ids"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|i| i.as_str().unwrap().to_string()),
            );
        }
        Ok(v)
    });
    (h, gh)
}

#[test]
fn a_full_read_checks_what_left_since_the_last_full_read_and_nothing_older_or_elsewhere() {
    const DAY: i64 = 24 * 60;
    let base = fixture_root();
    snap(&base, &gh(), "home", at(0));
    snap(&base, &gh(), "kernel", at(0));
    let home_open = open_members(&base, "home", STORE);
    let kernel_open = open_members(&base, "kernel", STORE);
    let (z, w) = (home_open[0].clone(), home_open[1].clone());
    let v = kernel_open.iter().find(|k| **k != z && **k != w).unwrap().clone();

    let t = fixture_root();
    let (h, gh) = hiding();
    h.from_home.lock().unwrap().insert(v.clone());
    snap(&t, &gh, "kernel", at(0));
    // Home's first read: z, w in its set, v (kernel's alone) not.
    snap(&t, &gh, "home", at(1));
    assert!(members(&t, "home", STORE).contains(&w) && !members(&t, "home", STORE).contains(&v));
    // A day on, the full read: w has left. It is checked now, and from now on is forgotten.
    h.always.lock().unwrap().insert(w.clone());
    let r = snap(&t, &gh, "home", at(1 + DAY));
    assert_eq!(report(&r, STORE).full_reason.as_deref(), Some("daily"));
    assert!(h.asked.lock().unwrap().contains(&w));
    // An incremental read: z leaves.
    h.always.lock().unwrap().insert(z.clone());
    let r = snap(&t, &gh, "home", at(1 + DAY + 10));
    assert_eq!(report(&r, STORE).mode.as_deref(), Some("incremental"));
    // The next full read checks z, which left since the last full read, but neither w, which
    // was checked by that one, nor v, which is in no set of this project.
    h.asked.lock().unwrap().clear();
    let r = snap(&t, &gh, "home", at(1 + 2 * DAY));
    assert_eq!(report(&r, STORE).full_reason.as_deref(), Some("daily"));
    let asked = h.asked.lock().unwrap();
    assert!(
        asked.contains(&z),
        "z left at an incremental read since the last full one"
    );
    assert!(!asked.contains(&w), "w was checked by the previous full read");
    assert!(!asked.contains(&v), "v was never in this project's set");
}

#[test]
fn changing_what_a_view_filters_or_how_labels_translate_forces_a_full_read() {
    for change in ["filter", "translation"] {
        let t = fixture_root();
        snap(&t, &gh(), "home", at(0));
        let r = snap(&t, &gh(), "home", at(10));
        assert_eq!(report(&r, STORE).mode.as_deref(), Some("incremental"));
        if change == "filter" {
            let p = t.path().join("projects/home.toml");
            let text = std::fs::read_to_string(&p).unwrap().replace(
                "repo = \"yuvalrakavy/Store\"\n",
                "repo = \"yuvalrakavy/Store\"\nfilter = \"label:kernel\"\n",
            );
            assert!(text.contains("label:kernel"));
            std::fs::write(&p, text).unwrap();
        } else {
            let p = t.path().join("repos.toml");
            let text = std::fs::read_to_string(&p).unwrap().replace(
                "translate.area.kernel     = [\"kernel\"]",
                "translate.area.kernel     = [\"kernel\", \"os\"]",
            );
            assert!(text.contains("\"os\""));
            std::fs::write(&p, text).unwrap();
        }
        let r = snap(&t, &gh(), "home", at(20));
        let store = report(&r, STORE);
        assert_eq!(
            store.full_reason.as_deref(),
            Some("view or translation changed"),
            "{change}: {store:?}"
        );
        assert_eq!(store.mode.as_deref(), Some("full"), "{change}: {store:?}");
        // And it is one change, not a standing one.
        let r = snap(&t, &gh(), "home", at(30));
        assert_eq!(report(&r, STORE).mode.as_deref(), Some("incremental"), "{change}");
    }
}
