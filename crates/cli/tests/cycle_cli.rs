//! `snapshot` and `cycle` end to end, as separate processes, over the recorded GitHub responses
//! of `crates/core/tests/fixtures` (no network).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_issue-dashboard");

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures")
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let target = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            fs::copy(e.path(), target).unwrap();
        }
    }
}

struct Tracker {
    dir: tempfile::TempDir,
    now: String,
}

impl Tracker {
    fn new() -> Tracker {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&fixtures().join("config"), dir.path());
        let meta: Value =
            serde_json::from_str(&fs::read_to_string(fixtures().join("replay-meta.json")).unwrap()).unwrap();
        Tracker {
            dir,
            now: meta["recorded_at"].as_str().unwrap().to_string(),
        }
    }

    fn cmd(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env("ISSUE_DASHBOARD_ROOT", self.dir.path())
            .env("ISSUE_DASHBOARD_NOW", &self.now)
            .env("ISSUE_DASHBOARD_REPLAY", fixtures().join("replay"))
            .env("ISSUE_DASHBOARD_PAGE_SIZE", "25")
            .env_remove("ISSUE_DASHBOARD_RECORD")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Value {
        let out = self.cmd(args);
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }
}

#[test]
fn snapshot_then_cycle_writes_an_outbox_with_the_acks() {
    let t = Tracker::new();
    let snap = t.ok(&["snapshot", "--project", "home"]);
    let repos = snap["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 3, "{snap}");
    assert!(
        repos.iter().all(|r| r["error"].is_null() && r["mode"] == "full"),
        "{snap}"
    );

    let pages = t.dir.path().join("pages.json");
    fs::write(
        &pages,
        json!({"decisions": [{
            "id": "D_cli", "project": "home", "author": "user_owner",
            "created_at": "2026-10-23T10:00:00Z", "action": "set_priority",
            "target": {"item": "I_kwDONszNXs8AAAABLDRZzA"}, "value": {"priority": "p1"}
        }]})
        .to_string(),
    )
    .unwrap();
    let c = t.ok(&["cycle", "--project", "home", "--pages", pages.to_str().unwrap()]);
    assert_eq!(c["ingest"]["decided"], 1, "{c}");
    let outbox: Value = serde_json::from_str(&fs::read_to_string(c["outbox"].as_str().unwrap()).unwrap()).unwrap();
    assert_eq!(outbox["sequence"], c["sequence"]);
    assert!(
        outbox["acks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["decision"] == "D_cli" && a["result"] == "applied")
    );
    assert!(outbox["issues"].as_array().unwrap().len() > 100);
}

#[test]
fn a_pages_file_with_an_unknown_collection_exits_1() {
    let t = Tracker::new();
    let pages = t.dir.path().join("pages.json");
    fs::write(&pages, json!({"decisions": [], "launchez": []}).to_string()).unwrap();
    let out = t.cmd(&["cycle", "--project", "home", "--pages", pages.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("launchez"));
}
