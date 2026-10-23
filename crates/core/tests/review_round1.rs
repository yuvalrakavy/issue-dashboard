//! The first pre-merge depth review's findings, one test each: which details a read keeps when
//! projects share a repo, reads that commit out of order, the last full read's boundary, an
//! entrant GitHub cannot resolve, an undo chain against a withdrawal, and what a recording
//! can serve.

mod common;

use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use common::TempRoot;
use common::fixture::*;
use common::*;
use issue_dashboard_core::config::ConfigSet;
use issue_dashboard_core::cycle::{self, CycleReport};
use issue_dashboard_core::docs::{Action, Decision, LaunchRequest, Target};
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::github::{GitHub, Replay, Request};
use issue_dashboard_core::ingest::Pages;
use issue_dashboard_core::launch::{self, State};
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

fn title(t: &TempRoot, id: &str) -> Option<String> {
    observations(t, id)
        .last()
        .map(|o| o.content["title"].as_str().unwrap().to_string())
}

/// An open Store issue the first read of `project` found.
fn open_issue(t: &TempRoot, project: &str, but_not: &BTreeSet<String>) -> String {
    members(t, project, STORE)
        .into_iter()
        .find(|id| {
            id.starts_with("I_")
                && !but_not.contains(id)
                && observations(t, id).last().unwrap().content["state"] == "open"
        })
        .unwrap()
}

fn titled(id: &str, title: &str, minutes: i64) -> Value {
    let mut n = real_node(&library(), id).unwrap();
    n["title"] = json!(title);
    n["updatedAt"] = json!(at(minutes).to_rfc3339());
    n
}

fn in_since_list(req: &Request, v: &mut Value, node: &Value) {
    if req.name == "issues_since" {
        let nodes = v["data"]["repository"]["issues"]["nodes"].as_array_mut().unwrap();
        nodes.retain(|n| n["id"] != node["id"]);
        nodes.push(node.clone());
    }
}

fn read_by_node(req: &Request, v: &mut Value, node: &Value) {
    if req.name == "nodes"
        && let Some(nodes) = v["data"]["nodes"].as_array_mut()
    {
        for n in nodes.iter_mut().filter(|n| n["id"] == node["id"]) {
            *n = node.clone();
        }
    }
}

/// Adds `id` (Store issue `number`) to the first page of kernel's open-issue search.
fn in_kernel_search(req: &Request, v: &mut Value, id: &str, number: u64) {
    let q = req.variables["q"].as_str().unwrap_or("");
    if is_search_of(req, STORE) && q.contains("is:issue is:open label:kernel") && req.variables["after"].is_null() {
        v["data"]["search"]["nodes"].as_array_mut().unwrap().push(json!({
            "__typename": "Issue", "id": id, "number": number, "closedAt": null,
            "repository": {"nameWithOwner": STORE}
        }));
    }
}

/// Kernel's config names Store by a name GitHub redirects to it (an old name).
fn kernel_names_store_old(t: &TempRoot) {
    let p = t.path().join("projects/kernel.toml");
    let text = std::fs::read_to_string(&p)
        .unwrap()
        .replace("repo   = \"yuvalrakavy/Store\"", "repo   = \"yuvalrakavy/StoreOld\"");
    assert!(text.contains("StoreOld"), "{text}");
    std::fs::write(&p, text).unwrap();
    let r = t.path().join("repos.toml");
    let text = std::fs::read_to_string(&r).unwrap() + "\n[repo.\"yuvalrakavy/StoreOld\"]\n";
    std::fs::write(&r, text).unwrap();
}

/// Answers a lookup of the old name as GitHub does: with the repo under its current name.
fn follow_old_name(req: &Request, v: issue_dashboard_core::Result<Value>) -> issue_dashboard_core::Result<Value> {
    if req.name == "repo" && req.variables["name"] == "StoreOld" {
        let mut vars = req.variables.clone();
        vars["name"] = json!("Store");
        return library().graphql(&Request {
            name: req.name,
            query: req.query,
            variables: vars,
        });
    }
    v
}

#[test]
fn a_project_naming_the_repo_by_an_old_name_still_has_its_members_kept_current() {
    let t = fixture_root();
    kernel_names_store_old(&t);
    snap(&t, &gh(), "home", at(0));
    snap(&t, &gh_with(follow_old_name), "kernel", at(0));
    assert_eq!(node_of(&t, "yuvalrakavy/StoreOld"), node_of(&t, STORE));
    let z = open_issue(&t, "kernel", &BTreeSet::new());
    // Home no longer holds z; kernel, which names the repo differently, still does.
    let (left, new) = (z.clone(), titled(&z, "changed after it left home", 3));
    let gh = gh_with(move |req, v| {
        let mut v = v?;
        if is_search_of(req, STORE) {
            v = drop_from_search(v, &left);
        }
        in_since_list(req, &mut v, &new);
        Ok(v)
    });
    let r = snap(&t, &gh, "home", at(10));
    assert_eq!(report(&r, STORE).mode.as_deref(), Some("incremental"));
    assert!(!members(&t, "home", STORE).contains(&z));
    assert_eq!(title(&t, &z).as_deref(), Some("changed after it left home"));
}

#[test]
fn an_item_entering_a_set_behind_another_projects_newer_read_is_read_again() {
    let t = fixture_root();
    snap(&t, &gh(), "kernel", at(0));
    let kernel_set = members(&t, "kernel", STORE);
    // x: a Store issue no set holds (home's search never returns it).
    let t0 = fixture_root();
    snap(&t0, &gh(), "home", at(0));
    let x = open_issue(&t0, "home", &kernel_set);
    let number = observations(&t0, &x)
        .last()
        .unwrap()
        .alias
        .rsplit('#')
        .next()
        .unwrap()
        .parse::<u64>()
        .unwrap();
    let id = x.clone();
    let home_gh = move |node: Option<Value>| {
        let id = id.clone();
        gh_with(move |req, v| {
            let mut v = drop_from_search(v?, &id);
            if let Some(n) = &node {
                in_since_list(req, &mut v, n);
            }
            Ok(v)
        })
    };
    snap(&t, &home_gh(None), "home", at(0));
    assert!(!members(&t, "home", STORE).contains(&x));
    let id = x.clone();
    let kernel_gh = move |node: Value| {
        let id = id.clone();
        gh_with(move |req, v| {
            let mut v = v?;
            in_kernel_search(req, &mut v, &id, number);
            read_by_node(req, &mut v, &node);
            Ok(v)
        })
    };

    // Kernel's read at 10 sees x enter; home's read at 20 commits first and moves the
    // watermark past 10, with x's change at 12 in its since-list but in no set yet.
    let config = ConfigSet::load(t.path()).unwrap();
    let plan = |project: &str, now| {
        t.root
            .read(|l| snapshot::plan(l.conn(), &config, project, now))
            .unwrap()
    };
    let k = kernel_gh(titled(&x, "as kernel read it at 10", 5));
    let a = snapshot::fetch(&k as &dyn GitHub, &plan("kernel", at(10)), &|| at(10), PAGE);
    let h = home_gh(Some(titled(&x, "changed at 12", 12)));
    let b = snapshot::fetch(&h as &dyn GitHub, &plan("home", at(20)), &|| at(20), PAGE);
    t.root.mutate(at(21), |tx| snapshot::record(tx, "home", &b)).unwrap();
    let ra = t.root.mutate(at(22), |tx| snapshot::record(tx, "kernel", &a)).unwrap();
    assert!(members(&t, "kernel", STORE).contains(&x));
    assert_eq!(title(&t, &x).as_deref(), Some("as kernel read it at 10"));
    assert_eq!(report(&ra, STORE).reread, 1, "{ra:?}");

    // No since-list from the watermark on holds the change at 12: kernel's next read asks
    // for x by node id.
    let r = snap(&t, &kernel_gh(titled(&x, "changed at 12", 12)), "kernel", at(30));
    assert_eq!(report(&r, STORE).mode.as_deref(), Some("incremental"));
    assert_eq!(title(&t, &x).as_deref(), Some("changed at 12"));
}

#[test]
fn a_failed_read_that_a_newer_good_read_overtook_leaves_the_repos_status_alone() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let config = ConfigSet::load(t.path()).unwrap();
    let plan = |now| t.root.read(|l| snapshot::plan(l.conn(), &config, "home", now)).unwrap();
    let failing = gh_with(|req, v| {
        if is_search_of(req, STORE) {
            Err(Error::GitHub("simulated failure".into()))
        } else {
            v
        }
    });
    let a = snapshot::fetch(&failing as &dyn GitHub, &plan(at(10)), &|| at(10), PAGE);
    let good = gh();
    let b = snapshot::fetch(&good as &dyn GitHub, &plan(at(20)), &|| at(20), PAGE);
    t.root.mutate(at(21), |tx| snapshot::record(tx, "home", &b)).unwrap();
    let ra = t.root.mutate(at(22), |tx| snapshot::record(tx, "home", &a)).unwrap();
    assert!(report(&ra, STORE).superseded);
    let s = t
        .root
        .read(|l| snapshot::read_state(l.conn(), "home", STORE))
        .unwrap()
        .unwrap();
    assert_eq!(s.error, None, "{s:?}");
    assert_eq!(
        s.attempt_at
            .as_deref()
            .map(|a| a.starts_with(&at(20).format("%Y-%m-%dT%H:%M").to_string())),
        Some(true),
        "{s:?}"
    );
}

#[test]
fn a_full_read_checks_what_left_since_the_previous_full_read_and_no_earlier() {
    // Clock times with milliseconds: the boundary is the ledger's order, not a time compare.
    let ms = |minutes: i64, m: i64| at(minutes) + Duration::milliseconds(m);
    let t = fixture_root();
    snap(&t, &gh(), "home", ms(0, 123));
    let z = open_issue(&t, "home", &BTreeSet::new());
    let asked = Arc::new(Mutex::new(Vec::<String>::new()));
    let (left, log) = (z.clone(), asked.clone());
    let gh = gh_with(move |req, v| {
        if req.name == "nodes" {
            for id in req.variables["ids"].as_array().unwrap() {
                log.lock().unwrap().push(id.as_str().unwrap().to_string());
            }
        }
        if is_search_of(req, STORE) {
            return v.map(|v| drop_from_search(v, &left));
        }
        v
    });
    snap(&t, &gh, "home", ms(10, 456));
    let full = snap(&t, &gh, "home", ms(25 * 60, 789));
    assert_eq!(report(&full, STORE).full_reason.as_deref(), Some("daily"));
    assert!(asked.lock().unwrap().contains(&z), "z left since the last full read");
    asked.lock().unwrap().clear();
    let next = snap(&t, &gh, "home", ms(50 * 60, 1));
    assert_eq!(report(&next, STORE).full_reason.as_deref(), Some("daily"));
    assert!(
        !asked.lock().unwrap().contains(&z),
        "z left before the previous full read, which checked it"
    );
}

#[test]
fn an_entrant_github_cannot_resolve_is_not_a_member() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let ghost = "I_kwDOghostissue0001".to_string();
    let id = ghost.clone();
    let gh = gh_with(move |req, v| {
        let mut v = v?;
        let q = req.variables["q"].as_str().unwrap_or("");
        if is_search_of(req, STORE) && q.contains("is:issue is:open") && req.variables["after"].is_null() {
            v["data"]["search"]["nodes"].as_array_mut().unwrap().push(json!({
                "__typename": "Issue", "id": id, "number": 99999, "closedAt": null,
                "repository": {"nameWithOwner": STORE}
            }));
        }
        Ok(v)
    });
    let r = snap(&t, &gh, "home", at(10));
    assert_eq!(report(&r, STORE).error, None);
    assert!(!members(&t, "home", STORE).contains(&ghost));
    // The daily full read that follows has nothing of it to check or record as gone.
    let full = snap(&t, &gh, "home", at(25 * 60));
    let store = report(&full, STORE);
    assert_eq!((store.error.as_deref(), store.gone), (None, 0), "{store:?}");
    assert!(observations(&t, &ghost).is_empty());
    let gone = t
        .root
        .read(|l| ledger::events_of(l.conn(), &ghost, Kind::ItemGone))
        .unwrap();
    assert!(gone.is_empty(), "{gone:?}");
}

fn pages(decisions: &[Decision], launches: &[LaunchRequest]) -> Pages {
    Pages {
        decisions: decisions.iter().map(|d| serde_json::to_value(d).unwrap()).collect(),
        launches: launches.iter().map(|l| serde_json::to_value(l).unwrap()).collect(),
        ..Pages::default()
    }
}

fn run(t: &TempRoot, p: &Pages, minutes: i64) -> CycleReport {
    cycle::cycle(&t.root, &gh(), "home", p, &move || at(minutes), PAGE).unwrap()
}

fn ack(r: &CycleReport, id: &str) -> Value {
    let d: Value = serde_json::from_str(&std::fs::read_to_string(&r.outbox).unwrap()).unwrap();
    d["acks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["decision"] == id)
        .cloned()
        .unwrap_or_else(|| panic!("no ack for {id}: {}", d["acks"]))
}

fn cancel(id: &str, launch: &str) -> Decision {
    decision(
        id,
        OWNER,
        Action::CancelLaunch,
        Target {
            launch: Some(launch.into()),
            ..Default::default()
        },
        None,
    )
}

#[test]
fn an_undo_that_would_revive_an_undo_of_a_cancel_that_took_effect_is_too_late() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let x = open_issue(&t, "home", &BTreeSet::new());
    // The cancel, undone and the undo undone: the cancel stands, and withdraws the launch when
    // it arrives.
    run(
        &t,
        &pages(&[cancel("D_c", "L_one"), undo("D_u", "D_c"), undo("D_v", "D_u")], &[]),
        10,
    );
    run(
        &t,
        &pages(&[], &[launch_doc("L_one", "home", &[x.as_str()], "lanes")]),
        20,
    );
    let state = || t.root.read(|l| launch::load(l.conn(), "L_one")).unwrap().unwrap().state;
    assert_eq!(state(), State::Withdrawn);
    // Undoing D_v would revive D_u, which undoes the cancel that already withdrew it.
    let r = run(&t, &pages(&[undo("D_w", "D_v")], &[]), 30);
    assert_eq!(ack(&r, "D_w")["result"], "refused", "{}", ack(&r, "D_w"));
    assert!(ack(&r, "D_w")["reason"].as_str().unwrap().contains("too late"));
    assert_eq!(ack(&r, "D_c")["result"], "applied");
    assert_eq!(state(), State::Withdrawn);
}

/// A recorded search with a `closed:>=` date, and that date.
fn recorded_closed_search() -> (Value, String) {
    for e in std::fs::read_dir(fixtures().join("replay")).unwrap() {
        let rec: Value = serde_json::from_str(&std::fs::read_to_string(e.unwrap().path()).unwrap()).unwrap();
        if rec["name"] == "search"
            && let Some(d) = rec["variables"]["q"]
                .as_str()
                .unwrap()
                .split(' ')
                .find_map(|w| w.strip_prefix("closed:>="))
        {
            return (rec["variables"].clone(), d.to_string());
        }
    }
    panic!("no recorded search of closed items")
}

#[test]
fn a_recording_serves_its_own_history_window_or_a_shorter_one_never_a_longer_one() {
    let lib = library();
    let (vars, date) = recorded_closed_search();
    let ask = |d: &str| {
        let mut v = vars.clone();
        v["q"] = json!(
            v["q"]
                .as_str()
                .unwrap()
                .replace(&format!("closed:>={date}"), &format!("closed:>={d}"))
        );
        lib.graphql(&Request {
            name: "search",
            query: "",
            variables: v,
        })
    };
    let day = |n: i64| {
        (chrono::NaiveDate::parse_from_str(&date, "%Y-%m-%d").unwrap() + Duration::days(n))
            .format("%Y-%m-%d")
            .to_string()
    };
    assert!(ask(&date).is_ok());
    assert!(
        ask(&day(1)).is_ok(),
        "a shorter window is a subset of what was recorded"
    );
    let err = ask(&day(-1)).unwrap_err().to_string();
    assert!(err.contains("earlier window"), "{err}");
}
