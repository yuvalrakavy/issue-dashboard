//! What the dashboard page needs beyond slice 2's outbox: each issue's body and raw labels for
//! Details (5.x), and the project's groups and sync interval in meta.

mod common;

use std::fs;

use common::TempRoot;
use common::fixture::*;
use issue_dashboard_core::cycle;
use serde_json::{Value, json};

fn outbox(t: &TempRoot, project: &str, gh: &issue_dashboard_core::github::Replay) -> Value {
    let r = cycle::cycle(&t.root, gh, project, &no_pages(), &|| at(0), PAGE).unwrap();
    serde_json::from_str(&fs::read_to_string(&r.outbox).unwrap()).unwrap()
}

fn issue<'a>(d: &'a Value, alias_prefix: &str) -> Vec<&'a Value> {
    d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["alias"].as_str().unwrap().starts_with(alias_prefix))
        .collect()
}

#[test]
fn every_issue_carries_its_body_trimmed_and_its_raw_labels() {
    let t = fixture_root();
    let lib = library();
    let d = outbox(&t, "home", &gh());
    let mut with_body = 0;
    for i in issue(&d, "yuvalrakavy/Store#") {
        let node = real_node(&lib, i["id"].as_str().unwrap()).expect("a recorded node");
        let body = node["body"].as_str().unwrap_or("").trim();
        let cut = body.chars().count() > 8000;
        if body.is_empty() {
            assert_eq!(i["body"], Value::Null, "{}", i["alias"]);
        } else {
            with_body += 1;
            let kept: String = body.chars().take(8000).collect();
            assert_eq!(i["body"], json!(kept), "{}", i["alias"]);
        }
        assert_eq!(i["body_truncated"], json!(cut), "{}", i["alias"]);
        let labels: Vec<&str> = node["labels"]["nodes"]
            .as_array()
            .unwrap()
            .iter()
            .map(|l| l["name"].as_str().unwrap())
            .collect();
        assert_eq!(i["labels"], json!(labels), "{}", i["alias"]);
    }
    assert!(with_body > 50, "{with_body}");
}

#[test]
fn a_long_body_is_cut_at_the_cap_on_a_character_boundary_and_marked() {
    let t = fixture_root();
    let long = format!("  {}  ", "é".repeat(9000));
    let gh = gh_with(move |req, v| {
        let mut v = v?;
        if req.name == "nodes"
            && let Some(nodes) = v["data"]["nodes"].as_array_mut()
        {
            for n in nodes.iter_mut().filter(|n| n.get("body").is_some()) {
                n["body"] = json!(long);
            }
        }
        Ok(v)
    });
    let d = outbox(&t, "home", &gh);
    let store = issue(&d, "yuvalrakavy/Store#");
    assert!(!store.is_empty());
    for i in store {
        assert_eq!(i["body"], json!("é".repeat(8000)), "{}", i["alias"]);
        assert_eq!(i["body_truncated"], json!(true), "{}", i["alias"]);
    }
}

#[test]
fn meta_names_the_projects_groups_with_their_tracked_repos_and_the_sync_interval() {
    let t = fixture_root();
    fs::write(
        t.path().join("projects/page.toml"),
        r#"schema = 1
name = "Page"
owner_id = "user_owner"
sync_interval_minutes = 15

[groups]
core = ["yuvalrakavy/Store", "yuvalrakavy/airzoned", "yuvalrakavy/not-tracked"]

[[member]]
group = "core"
exclude = ["yuvalrakavy/not-tracked"]

[[member]]
repo      = "rhaiscript/rhai"
filter    = "author:yuvalrakavy"
also      = [1193, 1194]
read_only = true
"#,
    )
    .unwrap();
    let d = outbox(&t, "page", &gh());
    assert_eq!(d["meta"]["groups"], json!({"core": [STORE, AIRZONED]}));
    assert_eq!(d["meta"]["sync_interval_minutes"], json!(15));
    // A project without either: no groups, and the default interval.
    let home = outbox(&t, "home", &gh());
    assert_eq!(home["meta"]["groups"], json!({}));
    assert_eq!(
        home["meta"]["sync_interval_minutes"],
        json!(issue_dashboard_core::derive::DEFAULT_SYNC_MINUTES)
    );
}
