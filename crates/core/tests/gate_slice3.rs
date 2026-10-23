//! Gate (mutation pass, slice 3): guards of the page fields and the vanished-member read that
//! the slice's own tests could not tell from their mutants (the fixture holds no issue with an
//! empty body or a padded one, no group named in both configs, and no incremental read of a
//! member that does not resolve). Each test names the mutation it kills.

mod common;

use std::fs;

use common::TempRoot;
use common::fixture::*;
use issue_dashboard_core::canonical::sha256_hex;
use issue_dashboard_core::cycle;
use issue_dashboard_core::github::Request;
use issue_dashboard_core::read;
use issue_dashboard_core::snapshot::{self, RepoReport};
use serde_json::{Value, json};

fn outbox(t: &TempRoot, project: &str) -> Value {
    let r = cycle::cycle(&t.root, &gh(), project, &no_pages(), &|| at(0), PAGE).unwrap();
    serde_json::from_str(&fs::read_to_string(&r.outbox).unwrap()).unwrap()
}

fn a_store_node() -> Value {
    let t = fixture_root();
    let d = outbox(&t, "home");
    let id = d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["alias"].as_str().unwrap().starts_with("yuvalrakavy/Store#"))
        .unwrap()["id"]
        .as_str()
        .unwrap()
        .to_string();
    real_node(&library(), &id).unwrap()
}

fn item_with_body(body: Option<&str>) -> Value {
    let mut n = a_store_node();
    match body {
        Some(b) => n["body"] = json!(b),
        None => {
            n.as_object_mut().unwrap().remove("body");
        }
    }
    read::item(&n).unwrap().content
}

// Kills: `if kept.is_empty() { Null }` made unconditional-false (an empty body then reads as
// ""), the trim dropped, the hash taken of the trimmed body, `<` becoming `<=`.
#[test]
fn a_body_that_is_empty_or_blank_is_null_and_the_hash_covers_the_whole_body() {
    for none in [None, Some(""), Some("  \n\t ")] {
        let c = item_with_body(none);
        assert_eq!(c["body"], Value::Null, "{none:?}");
        assert_eq!(c["body_truncated"], json!(false), "{none:?}");
    }
    assert_eq!(item_with_body(None)["body_hash"], Value::Null);
    assert_eq!(item_with_body(Some(""))["body_hash"], Value::Null);
    let blank = "  \n\t ";
    assert_eq!(
        item_with_body(Some(blank))["body_hash"],
        json!(sha256_hex(blank)),
        "a blank body still has a hash"
    );
    let padded = "  text \n";
    let c = item_with_body(Some(padded));
    assert_eq!(c["body"], json!("text"));
    assert_eq!(
        c["body_hash"],
        json!(sha256_hex(padded)),
        "the hash is of the body as GitHub has it"
    );
    assert_eq!(c["body_truncated"], json!(false));
}

// Kills: the cap off by one in either direction, and `kept.len() < trimmed.len()` going
// wrong at the boundary.
#[test]
fn a_body_of_exactly_the_cap_is_whole_and_one_more_character_is_cut() {
    let exact = "a".repeat(read::BODY_CAP);
    let c = item_with_body(Some(&exact));
    assert_eq!(c["body"], json!(exact));
    assert_eq!(c["body_truncated"], json!(false));
    let over = format!("{exact}b");
    let c = item_with_body(Some(&over));
    assert_eq!(c["body"], json!(exact));
    assert_eq!(c["body_truncated"], json!(true));
    assert_eq!(
        c["body_hash"],
        json!(sha256_hex(&over)),
        "the hash is of the whole body, not the kept part"
    );
}

// Kills: the project's group not winning over the registry's of the same name; the registry
// fallback removed.
#[test]
fn a_projects_own_group_wins_over_the_registrys_and_a_registry_group_is_used_when_it_has_none() {
    let t = fixture_root();
    let repos = fs::read_to_string(t.path().join("repos.toml")).unwrap();
    fs::write(
        t.path().join("repos.toml"),
        format!("{repos}\n[groups]\nshared = [\"yuvalrakavy/Store\"]\nregistry_only = [\"yuvalrakavy/Store\", \"yuvalrakavy/airzoned\"]\n"),
    )
    .unwrap();
    fs::write(
        t.path().join("projects/page.toml"),
        r#"schema = 1
name = "Page"
owner_id = "user_owner"

[groups]
shared = ["yuvalrakavy/airzoned"]

[[member]]
group = "shared"

[[member]]
group = "registry_only"
"#,
    )
    .unwrap();
    let d = outbox(&t, "page");
    assert_eq!(
        d["meta"]["groups"],
        json!({"shared": ["yuvalrakavy/airzoned"], "registry_only": ["yuvalrakavy/Store", "yuvalrakavy/airzoned"]})
    );
}

fn snap(t: &TempRoot, gh: &issue_dashboard_core::github::Replay, minutes: i64) -> RepoReport {
    let r = cycle::snapshot(&t.root, gh, "home", &move || at(minutes), PAGE).unwrap();
    r.into_iter().find(|x| x.repo == STORE).unwrap()
}

fn unresolved(id: String) -> issue_dashboard_core::github::Replay {
    gh_with(move |req: &Request, v| {
        let mut v = v?;
        if req.name == "nodes" {
            let asked: Vec<String> = req.variables["ids"]
                .as_array()
                .unwrap()
                .iter()
                .map(|i| i.as_str().unwrap().to_string())
                .collect();
            let nodes = v["data"]["nodes"].as_array_mut().unwrap();
            for (i, a) in asked.iter().enumerate() {
                if *a == id {
                    nodes[i] = Value::Null;
                }
            }
        }
        Ok(v)
    })
}

fn an_open_store_member(t: &TempRoot) -> String {
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
    let lib = library();
    m.all()
        .into_iter()
        .find(|id| real_node(&lib, id).is_some_and(|n| n["__typename"] == "Issue" && n["state"] == "OPEN"))
        .unwrap()
}

// Kills: the set not losing a member the search finds but GitHub cannot resolve by id (the
// read then reports no one leaving).
#[test]
fn a_full_read_counts_the_unresolvable_member_as_leaving() {
    // A day later the history window has moved, so some members leave anyway: the control.
    let c = fixture_root();
    snap(&c, &gh(), 0);
    let control = snap(&c, &gh(), 24 * 60);
    assert_eq!(control.gone, 0, "{control:?}");
    let t = fixture_root();
    snap(&t, &gh(), 0);
    let id = an_open_store_member(&t);
    let r = snap(&t, &unresolved(id), 24 * 60);
    assert_eq!(r.mode.as_deref(), Some("full"), "{r:?}");
    assert_eq!((r.left, r.gone), (control.left + 1, 1), "{r:?} against {control:?}");
}

// Kills: `full &&` dropped from the gone record. An incremental read that meets a re-read
// member GitHub cannot resolve takes it out of the set but only a full read confirms it gone.
#[test]
fn an_incremental_read_does_not_record_an_unresolvable_member_gone() {
    let t = fixture_root();
    snap(&t, &gh(), 0);
    let id = an_open_store_member(&t);
    let node = t
        .root
        .read(|l| snapshot::repo_node_of(l.conn(), STORE))
        .unwrap()
        .unwrap();
    let c = rusqlite::Connection::open(t.path().join("ledger.sqlite")).unwrap();
    c.execute(
        "INSERT OR IGNORE INTO rereads (project, repo_node, item) VALUES ('home', ?1, ?2)",
        rusqlite::params![node, id],
    )
    .unwrap();
    drop(c);
    let r = snap(&t, &unresolved(id.clone()), 10);
    assert_eq!(r.mode.as_deref(), Some("incremental"), "{r:?}");
    assert_eq!(r.gone, 0, "an incremental read does not confirm a member gone: {r:?}");
    assert_eq!(r.left, 1, "{r:?}");
}
