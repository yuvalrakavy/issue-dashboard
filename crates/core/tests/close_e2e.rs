//! Closing from the page end to end against real GitHub (10.10): a scratch repository the test
//! creates with `gh` and deletes again, never one of the tracked repos. It runs only with
//! `ISSUE_DASHBOARD_E2E=1` (it needs `gh` signed in with the `repo` and `delete_repo` scopes);
//! otherwise it says so and passes.

mod common;

use std::process::Command;
use std::time::{Duration, Instant};

use chrono::Utc;
use common::*;
use issue_dashboard_core::cycle::{self, CycleReport};
use issue_dashboard_core::docs::{Action, CloseReason, CloseValue, Decision, DecisionValue, Target};
use issue_dashboard_core::github::GhCli;
use issue_dashboard_core::ingest::Pages;
use issue_dashboard_core::write::GhIssues;
use serde_json::Value;

fn gh(args: &[&str]) -> String {
    let out = Command::new("gh").args(args).output().expect("gh runs");
    assert!(
        out.status.success(),
        "gh {args:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// Deletes the scratch repository however the test ends.
struct Scratch(String);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = Command::new("gh").args(["repo", "delete", &self.0, "--yes"]).status();
    }
}

fn cycle(t: &TempRoot, decisions: &[Decision]) -> (CycleReport, Value) {
    let pages = Pages {
        decisions: decisions.iter().map(|d| serde_json::to_value(d).unwrap()).collect(),
        ..Pages::default()
    };
    let r = cycle::cycle_with_writes(&t.root, &GhCli, Some(&GhIssues), "e2e", &pages, &Utc::now, 25).unwrap();
    let d = serde_json::from_str(&std::fs::read_to_string(&r.outbox).unwrap()).unwrap();
    (r, d)
}

fn issue_now(repo: &str, n: &str) -> Value {
    serde_json::from_str(&gh(&["issue", "view", n, "-R", repo, "--json", "state,stateReason,comments"])).unwrap()
}

#[test]
fn a_close_and_its_undo_against_a_scratch_repository() {
    if std::env::var("ISSUE_DASHBOARD_E2E").as_deref() != Ok("1") {
        eprintln!("skipped: set ISSUE_DASHBOARD_E2E=1 to run against a scratch GitHub repository");
        return;
    }
    let owner = gh(&["api", "user", "--jq", ".login"]);
    let repo = format!("{owner}/issue-dashboard-e2e-{}", Utc::now().format("%Y%m%d%H%M%S"));
    gh(&["repo", "create", &repo, "--private", "--description", "issue-dashboard close e2e; deleted by the test"]);
    let _scratch = Scratch(repo.clone());
    let url = gh(&["issue", "create", "-R", &repo, "-t", "e2e: close me", "-b", "Created by the close e2e test."]);
    let number = url.rsplit('/').next().unwrap().to_string();

    let t = temp_root();
    std::fs::create_dir_all(t.path().join("projects")).unwrap();
    std::fs::write(t.path().join("repos.toml"), format!("[repo.\"{repo}\"]\n")).unwrap();
    std::fs::write(
        t.path().join("projects/e2e.toml"),
        format!(
            "schema = 1\nname = \"close e2e\"\nowner_id = \"{OWNER}\"\nmaintainers = [\"{owner}\"]\n\n[[member]]\nrepo = \"{repo}\"\n"
        ),
    )
    .unwrap();

    // The membership search can lag a new issue by a little.
    let alias = format!("{repo}#{number}");
    let start = Instant::now();
    let id = loop {
        let (_, d) = cycle(&t, &[]);
        if let Some(i) = d["issues"].as_array().unwrap().iter().find(|i| i["alias"] == alias.as_str()) {
            break i["id"].as_str().unwrap().to_string();
        }
        assert!(start.elapsed() < Duration::from_secs(120), "the issue never showed up: {d}");
        std::thread::sleep(Duration::from_secs(5));
    };

    let comment = "Closed from the issue dashboard: no longer relevant.";
    let close = Decision {
        id: "D_e2e_close".into(),
        project: "e2e".into(),
        author: OWNER.into(),
        created_at: "2026-10-23T08:00:00Z".into(),
        action: Action::Close,
        target: Target {
            item: Some(id.clone()),
            alias: Some(alias.clone()),
            ..Default::default()
        },
        value: Some(DecisionValue {
            close: Some(CloseValue {
                reason: CloseReason::NotPlanned,
                duplicate_of: None,
                comment: comment.into(),
            }),
            ..Default::default()
        }),
        reason: None,
    };
    let (r, _) = cycle(&t, &[close.clone()]);
    assert_eq!(r.writes.len(), 1, "{:?}", r.writes);
    assert_eq!(r.writes[0].outcome, "closed", "{:?}", r.writes);
    let now = issue_now(&repo, &number);
    assert_eq!(now["state"], "CLOSED");
    assert_eq!(now["stateReason"], "NOT_PLANNED");
    assert!(now["comments"].as_array().unwrap().iter().any(|c| c["body"] == comment), "{now}");

    let undo = Decision {
        id: "D_e2e_undo".into(),
        action: Action::Undo,
        target: Target {
            decision: Some("D_e2e_close".into()),
            ..Default::default()
        },
        value: None,
        ..close.clone()
    };
    let (r, d) = cycle(&t, &[close, undo]);
    assert_eq!(r.writes.iter().map(|w| w.outcome.as_str()).collect::<Vec<_>>(), ["reopened"], "{:?}", r.writes);
    assert_eq!(issue_now(&repo, &number)["state"], "OPEN");
    let ack = d["acks"].as_array().unwrap().iter().find(|a| a["decision"] == "D_e2e_undo").unwrap().clone();
    assert_eq!(ack["result"], "applied", "{ack}");
}
