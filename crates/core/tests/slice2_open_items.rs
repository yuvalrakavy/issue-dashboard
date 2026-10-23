//! Slice 2's open S1 items: a development ledger whose `reads` table predates `full_seq`, and a
//! pull request whose `latestOpinionatedReviews` comes back `null`.

mod common;

use common::fixture::*;
use issue_dashboard_core::cycle;
use serde_json::{Value, json};

#[test]
fn a_reads_table_without_full_seq_is_rebuilt_and_the_next_read_is_full() {
    let t = fixture_root();
    let first = cycle::snapshot(&t.root, &gh(), "home", &|| at(0), PAGE).unwrap();
    assert!(first.iter().all(|r| r.error.is_none()), "{first:?}");
    // The shape of `reads` before slice 2's gate added the column.
    let c = rusqlite::Connection::open(t.path().join("ledger.sqlite")).unwrap();
    c.execute_batch(
        "DROP TABLE reads;
         CREATE TABLE reads (project TEXT NOT NULL, repo TEXT NOT NULL, repo_node TEXT, view_hash TEXT,
             mode TEXT, attempt_at TEXT NOT NULL, good_at TEXT, full_at TEXT, error TEXT,
             PRIMARY KEY (project, repo));",
    )
    .unwrap();
    drop(c);
    let r = cycle::snapshot(&t.root, &gh(), "home", &|| at(10), PAGE).unwrap();
    for repo in &r {
        assert_eq!(repo.error, None, "{repo:?}");
        // The read state is derived: losing it costs one full read.
        assert_eq!(repo.mode.as_deref(), Some("full"), "{repo:?}");
    }
}

#[test]
fn a_null_latest_opinionated_reviews_keeps_githubs_aggregate_decision() {
    let t = fixture_root();
    let gh = gh_with(|req, v| {
        let mut v = v?;
        if req.name == "nodes"
            && let Some(nodes) = v["data"]["nodes"].as_array_mut()
        {
            for n in nodes.iter_mut().filter(|n| n["isDraft"].is_boolean()) {
                n["isDraft"] = json!(false);
                n["mergeable"] = json!("MERGEABLE");
                n["reviewDecision"] = json!("CHANGES_REQUESTED");
                n["state"] = json!("OPEN");
                n["closedAt"] = Value::Null;
                n["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = json!({"state": "SUCCESS"});
                n["latestOpinionatedReviews"] = Value::Null;
            }
        }
        Ok(v)
    });
    let r = cycle::cycle(&t.root, &gh, "home", &no_pages(), &|| at(0), PAGE).unwrap();
    let d: Value = serde_json::from_str(&std::fs::read_to_string(&r.outbox).unwrap()).unwrap();
    let prs: Vec<&Value> = d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["kind"] == "pull_request")
        .collect();
    assert!(prs.len() >= 3, "{}", prs.len());
    for pr in prs {
        assert_eq!(pr["status"], "waiting-on-author", "{pr}");
    }
}
