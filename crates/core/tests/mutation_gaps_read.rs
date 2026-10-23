//! What the reader does with a response that is not the usual one: errors, limits, case,
//! nulls. Each test names the guard it keeps (the pre-merge mutation pass).

mod common;

use std::collections::BTreeSet;

use chrono::{DateTime, Utc};
use common::TempRoot;
use common::fixture::*;
use issue_dashboard_core::cycle;
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::github::Replay;
use issue_dashboard_core::ledger;
use issue_dashboard_core::observe::ObservedData;
use issue_dashboard_core::snapshot::{self, RepoReport};
use serde_json::{Value, json};

fn snap(t: &TempRoot, gh: &Replay, project: &str, now: DateTime<Utc>) -> Vec<RepoReport> {
    cycle::snapshot(&t.root, gh, project, &move || now, PAGE).unwrap()
}

fn errors(r: &[RepoReport]) -> Vec<String> {
    r.iter().filter_map(|x| x.error.clone()).collect()
}

fn members(t: &TempRoot, project: &str, repo: &str) -> BTreeSet<String> {
    let node = t
        .root
        .read(|l| snapshot::repo_node_of(l.conn(), repo))
        .unwrap()
        .unwrap();
    t.root
        .read(|l| snapshot::membership(l.conn(), project, &node))
        .unwrap()
        .map(|(_, m)| m.all())
        .unwrap_or_default()
}

fn observed(t: &TempRoot, id: &str) -> Vec<ObservedData> {
    t.root
        .read(|l| ledger::events_of(l.conn(), id, Kind::Observed))
        .unwrap()
        .into_iter()
        .map(|e| serde_json::from_value(e.data).unwrap())
        .collect()
}

/// A replay that adds `error` to every response of the request `name`.
fn with_error(name: &'static str, error: Value) -> Replay {
    gh_with(move |req, v| {
        let mut v = v?;
        if req.name == name {
            v["errors"] = json!([error]);
        }
        Ok(v)
    })
}

#[test]
fn a_not_found_error_is_tolerated_only_where_a_missing_item_is_expected() {
    // (request, the path of a NOT_FOUND that is not the expected kind)
    let wrong: Vec<(&'static str, Value)> = vec![
        ("search", json!(["search"])),
        ("repo", json!(["repository"])),
        ("number", json!(["repository"])),
        ("number", json!(["repository", "something"])),
        ("number", json!(["a", "issueOrPullRequest", "b"])),
        ("nodes", json!(["nodes"])),
        ("nodes", json!(["other", 0])),
    ];
    for (name, path) in wrong {
        let t = fixture_root();
        let gh = with_error(
            name,
            json!({"type": "NOT_FOUND", "path": path, "message": "no such thing"}),
        );
        let r = snap(&t, &gh, "home", at(0));
        assert!(
            errors(&r).iter().any(|e| e.contains("no such thing")),
            "{name} {path}: {r:?}"
        );
    }
    // The incremental reads have no place for a missing item at all.
    for name in ["issues_since", "prs_since"] {
        let t = fixture_root();
        snap(&t, &gh(), "home", at(0));
        let gh = with_error(
            name,
            json!({"type": "NOT_FOUND", "path": ["repository"], "message": "gone"}),
        );
        let r = snap(&t, &gh, "home", at(10));
        assert!(errors(&r).iter().any(|e| e.contains("gone")), "{name}: {r:?}");
    }
    // The expected kinds are tolerated.
    let t = fixture_root();
    let gh = gh_with(|req, v| {
        let mut v = v?;
        if req.name == "number" {
            v["errors"] = json!([{"type": "NOT_FOUND", "path": ["repository", "issueOrPullRequest"], "message": "x"}]);
        }
        Ok(v)
    });
    let r = snap(&t, &gh, "home", at(0));
    assert!(errors(&r).is_empty(), "{r:?}");
}

#[test]
fn a_search_matching_more_than_github_returns_fails_the_read() {
    for (count, fails) in [(1000u64, false), (1001, true), (2500, true)] {
        let t = fixture_root();
        let gh = gh_with(move |req, v| {
            let mut v = v?;
            if req.name == "search" {
                v["data"]["search"]["issueCount"] = json!(count);
            }
            Ok(v)
        });
        let r = snap(&t, &gh, "home", at(0));
        let e = errors(&r);
        if fails {
            assert!(!e.is_empty(), "{count}: {r:?}");
            assert!(e.iter().all(|m| m.contains("more than GitHub returns")), "{e:?}");
            // A read that fails leaves no set behind.
            assert!(
                t.root
                    .read(|l| snapshot::repo_node_of(l.conn(), STORE))
                    .unwrap()
                    .is_none()
                    || members(&t, "home", STORE).is_empty()
            );
        } else {
            assert!(e.is_empty(), "{count}: {e:?}");
        }
    }
}

#[test]
fn a_search_hit_is_matched_to_the_repo_without_regard_to_case() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let m0 = members(&t, "home", STORE);
    let t = fixture_root();
    let gh = gh_with(|req, v| {
        let mut v = v?;
        if req.name == "search"
            && let Some(nodes) = v["data"]["search"]["nodes"].as_array_mut()
        {
            for n in nodes.iter_mut().filter(|n| !n.is_null()) {
                if let Some(name) = n["repository"]["nameWithOwner"].as_str() {
                    n["repository"]["nameWithOwner"] = json!(name.to_uppercase());
                }
            }
        }
        Ok(v)
    });
    let r = snap(&t, &gh, "home", at(0));
    assert!(errors(&r).is_empty(), "{r:?}");
    assert_eq!(members(&t, "home", STORE), m0);
}

#[test]
fn a_null_in_a_search_result_is_skipped() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let m0 = members(&t, "home", STORE);
    let t = fixture_root();
    let gh = gh_with(|req, v| {
        let mut v = v?;
        if req.name == "search"
            && let Some(nodes) = v["data"]["search"]["nodes"].as_array_mut()
        {
            nodes.insert(0, Value::Null);
            nodes.push(Value::Null);
        }
        Ok(v)
    });
    let r = snap(&t, &gh, "home", at(0));
    assert!(errors(&r).is_empty(), "{r:?}");
    assert_eq!(members(&t, "home", STORE), m0);
}

#[test]
fn a_closed_item_belongs_to_the_window_from_the_instant_it_starts() {
    let baseline = fixture_root();
    snap(&baseline, &gh(), "home", at(0));
    let closed: Vec<String> = members(&baseline, "home", STORE)
        .into_iter()
        .filter(|id| id.starts_with("I_") && observed(&baseline, id).last().unwrap().content["state"] != "open")
        .take(2)
        .collect();
    assert_eq!(closed.len(), 2);
    let (on_the_edge, before) = (closed[0].clone(), closed[1].clone());
    let edge = at(0) - chrono::Duration::days(30);
    let gh = {
        let (e, b) = (on_the_edge.clone(), before.clone());
        gh_with(move |req, v| {
            let mut v = v?;
            if req.name == "search"
                && let Some(nodes) = v["data"]["search"]["nodes"].as_array_mut()
            {
                for n in nodes.iter_mut().filter(|n| !n.is_null()) {
                    if n["id"] == e.as_str() {
                        n["closedAt"] = json!(edge.to_rfc3339());
                    }
                    if n["id"] == b.as_str() {
                        n["closedAt"] = json!((edge - chrono::Duration::seconds(1)).to_rfc3339());
                    }
                }
            }
            Ok(v)
        })
    };
    let t = fixture_root();
    let r = snap(&t, &gh, "home", at(0));
    assert!(errors(&r).is_empty(), "{r:?}");
    let m = members(&t, "home", STORE);
    assert!(
        m.contains(&on_the_edge),
        "closed exactly at the window's start is inside it"
    );
    assert!(!m.contains(&before), "closed a second before it is outside");
}

#[test]
fn a_membership_keeps_issues_and_pull_requests_apart_whichever_way_they_got_in() {
    use issue_dashboard_core::observe::ItemKind;
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let node = t
        .root
        .read(|l| snapshot::repo_node_of(l.conn(), RHAI))
        .unwrap()
        .unwrap();
    let (_, m) = t
        .root
        .read(|l| snapshot::membership(l.conn(), "home", &node))
        .unwrap()
        .unwrap();
    // rhai lists 1193 (an issue) and 1194 (a pull request) by number, and filters the rest.
    assert!(!m.issues.is_empty() && !m.pull_requests.is_empty(), "{m:?}");
    for id in &m.issues {
        assert_eq!(observed(&t, id).last().unwrap().item, ItemKind::Issue, "{id}");
    }
    for id in &m.pull_requests {
        assert_eq!(observed(&t, id).last().unwrap().item, ItemKind::PullRequest, "{id}");
    }
    let number = |id: &String| observed(&t, id).last().unwrap().alias.clone();
    assert!(
        m.issues.iter().any(|i| number(i).ends_with("#1193")),
        "{:?}",
        m.issues.iter().map(number).collect::<Vec<_>>()
    );
    assert!(m.pull_requests.iter().any(|i| number(i).ends_with("#1194")));
}

#[test]
fn every_member_of_a_full_read_is_read_again_not_only_the_new_ones() {
    let t = fixture_root();
    let first = snap(&t, &gh(), "home", at(0));
    let before = first.iter().find(|x| x.repo == STORE).unwrap().entered;
    assert!(before > 10);
    // The daily full read, with nothing new in the set: everything is read by id again.
    let again = snap(&t, &gh(), "home", at(24 * 60));
    let n = members(&t, "home", STORE).len();
    let store = again.iter().find(|x| x.repo == STORE).unwrap();
    assert_eq!(store.mode.as_deref(), Some("full"));
    assert_eq!(store.entered, n, "{store:?}");
}

#[test]
fn a_response_with_an_error_or_without_data_fails_the_read() {
    // Any error that is not an expected NOT_FOUND fails the read, whatever else came with it.
    for name in ["search", "repo", "nodes"] {
        let t = fixture_root();
        let gh = with_error(name, json!({"type": "RATE_LIMITED", "message": "boom"}));
        let r = snap(&t, &gh, "home", at(0));
        assert!(errors(&r).iter().any(|e| e.contains("boom")), "{name}: {r:?}");
    }
    // An error with no type at all is not a NOT_FOUND, even on a path where one is expected.
    let t = fixture_root();
    let typeless = with_error(
        "number",
        json!({"path": ["repository", "issueOrPullRequest"], "message": "typeless"}),
    );
    let r = snap(&t, &typeless, "home", at(0));
    assert!(errors(&r).iter().any(|e| e.contains("typeless")), "{r:?}");
    // No data is a failure, not an empty answer: a search that returned nothing at all must not
    // empty the set.
    for name in ["search", "repo"] {
        let t = fixture_root();
        let gh = gh_with(move |req, v| {
            let v = v?;
            Ok(if req.name == name { json!({}) } else { v })
        });
        let r = snap(&t, &gh, "home", at(0));
        assert!(errors(&r).iter().any(|e| e.contains("no data")), "{name}: {r:?}");
    }
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let before = members(&t, "home", STORE);
    let down = gh_with(|req, v| {
        let v = v?;
        Ok(if req.name == "search" { json!({"data": null}) } else { v })
    });
    let r = snap(&t, &down, "home", at(10));
    assert!(errors(&r).iter().any(|e| e.contains("no data")), "{r:?}");
    assert_eq!(members(&t, "home", STORE), before, "a failed read changes no set");
}

#[test]
fn a_repo_or_item_with_more_labels_than_were_asked_for_is_not_read() {
    for (count, fails) in [(100u64, false), (101, true)] {
        let t = fixture_root();
        let gh = gh_with(move |req, v| {
            let mut v = v?;
            if req.name == "repo" {
                v["data"]["repository"]["labels"]["totalCount"] = json!(count);
            }
            Ok(v)
        });
        let e = errors(&snap(&t, &gh, "home", at(0)));
        assert_eq!(
            e.iter().any(|m| m.contains("more than 100 labels")),
            fails,
            "{count}: {e:?}"
        );
    }
    for (count, fails) in [(50u64, false), (51, true)] {
        let t = fixture_root();
        let gh = gh_with(move |req, v| {
            let mut v = v?;
            if req.name == "nodes"
                && let Some(nodes) = v["data"]["nodes"].as_array_mut()
            {
                for n in nodes.iter_mut().filter(|n| n["labels"].is_object()) {
                    n["labels"]["totalCount"] = json!(count);
                }
            }
            Ok(v)
        });
        let e = errors(&snap(&t, &gh, "home", at(0)));
        assert_eq!(
            e.iter().any(|m| m.contains("more than 50 labels")),
            fails,
            "{count}: {e:?}"
        );
    }
}

#[test]
fn a_repo_that_does_not_exist_or_is_not_owner_slash_name_is_said_so() {
    let t = fixture_root();
    let gone = gh_with(|req, v| {
        let mut v = v?;
        if req.name == "repo" {
            v["data"]["repository"] = Value::Null;
        }
        Ok(v)
    });
    let r = snap(&t, &gone, "home", at(0));
    assert!(errors(&r).iter().all(|e| e.contains("no such repository")), "{r:?}");
    assert_eq!(errors(&r).len(), 3);
    let live = gh();
    let reader = issue_dashboard_core::read::Reader::new(&live, PAGE);
    let e = reader.repo("noslash").unwrap_err().to_string();
    assert!(e.contains("owner/name"), "{e}");
}

#[test]
fn what_is_recorded_of_a_repo_includes_its_default_branch() {
    let t = fixture_root();
    snap(&t, &gh(), "home", at(0));
    let node = t
        .root
        .read(|l| snapshot::repo_node_of(l.conn(), STORE))
        .unwrap()
        .unwrap();
    let o = observed(&t, &node);
    assert!(
        o.last().unwrap().content["default_branch"].is_string(),
        "{:?}",
        o.last()
    );
}

#[test]
fn a_node_id_that_is_not_in_the_current_format_fails_the_read() {
    for bad in ["bad id", "abc=", "", "a/b"] {
        let t = fixture_root();
        let gh = gh_with(move |req, v| {
            let mut v = v?;
            if req.name == "search"
                && let Some(n) = v["data"]["search"]["nodes"].as_array_mut().and_then(|n| n.first_mut())
            {
                n["id"] = json!(bad);
            }
            Ok(v)
        });
        let r = snap(&t, &gh, "home", at(0));
        assert!(
            errors(&r).iter().any(|e| e.contains("not in the current format")),
            "{bad:?}: {r:?}"
        );
    }
}
