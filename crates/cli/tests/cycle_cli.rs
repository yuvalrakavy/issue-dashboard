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
fn reserve_request_retried_after_its_launch_was_withdrawn_says_withdrawn_again() {
    let t = Tracker::new();
    t.ok(&["snapshot", "--project", "home"]);
    let pages = t.dir.path().join("pages.json");
    fs::write(
        &pages,
        json!({"decisions": [{
            "id": "D_cancel", "project": "home", "author": "user_owner",
            "created_at": "2026-10-23T10:00:00Z", "action": "cancel_launch",
            "target": {"launch": "L_one"}
        }]})
        .to_string(),
    )
    .unwrap();
    t.ok(&["cycle", "--project", "home", "--pages", pages.to_str().unwrap()]);
    let request = t.dir.path().join("request.json");
    fs::write(
        &request,
        json!({
            "id": "L_one", "project": "home", "author": "user_owner",
            "created_at": "2026-10-23T10:05:00Z", "kind": "fix",
            "items": ["I_kwDONszNXs8AAAABLDRZzA"], "aliases": ["yuvalrakavy/Store#1"],
            "recipe": "lanes",
            "routing": {"model": "claude-opus-5-5", "effort": "medium"}
        })
        .to_string(),
    )
    .unwrap();
    let args = ["reserve", "--request", request.to_str().unwrap(), "--dispatcher", "D1"];
    let first = t.ok(&args);
    assert_eq!(first, json!({"result": "withdrawn", "launch": "L_one"}));
    // The answer to the first try was lost; the dispatcher asks again.
    assert_eq!(t.ok(&args), first);
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

#[test]
fn cycle_records_the_session_list_it_is_given() {
    let t = Tracker::new();
    let list = fixtures().join("sessions/list.json");
    let c = t.ok(&["cycle", "--project", "home", "--sessions", list.to_str().unwrap()]);
    assert_eq!(c["sessions"]["seen"], 4, "{c}");
    let again = t.ok(&["cycle", "--project", "home", "--sessions", list.to_str().unwrap()]);
    assert_eq!(again["sessions"]["seen"], 0, "{again}");
    let outbox: Value = serde_json::from_str(&fs::read_to_string(c["outbox"].as_str().unwrap()).unwrap()).unwrap();
    assert!(
        outbox["issues"]
            .as_array()
            .unwrap()
            .iter()
            .all(|i| i["sessions"].is_array())
    );
}

#[test]
fn a_sessions_file_without_its_limit_exits_1_and_records_nothing() {
    let t = Tracker::new();
    let list = t.dir.path().join("sessions.json");
    fs::write(
        &list,
        json!({"listed_at": "2026-10-23T16:47:00Z", "sessions": []}).to_string(),
    )
    .unwrap();
    let out = t.cmd(&["cycle", "--project", "home", "--sessions", list.to_str().unwrap()]);
    assert_eq!(out.status.code(), Some(1), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(String::from_utf8_lossy(&out.stderr).contains("sessions.json"));
    assert!(!t.dir.path().join("outbox").exists());
}

#[test]
fn sync_now_records_a_request_and_prints_the_configured_task() {
    let t = Tracker::new();
    t.ok(&["snapshot", "--project", "home"]);
    let global = t.ok(&["sync-now", "--project", "home"]);
    assert_eq!(global["items"], json!([]), "{global}");
    assert_eq!(global["task"], Value::Null, "{global}");

    let path = t.dir.path().join("projects/home.toml");
    let text = fs::read_to_string(&path).unwrap();
    fs::write(
        &path,
        text.replacen("history_days", "sync_task = \"home-sync\"\nhistory_days", 1),
    )
    .unwrap();
    let one = t.ok(&["sync-now", "--project", "home", "--issue", "yuvalrakavy/Store#8"]);
    assert_eq!(one["items"], json!(["I_kwDONszNXs8AAAABLDRZzA"]), "{one}");
    assert_eq!(one["task"], "home-sync", "{one}");
    assert!(one["request"].as_str().unwrap().starts_with("sync-"), "{one}");
}

#[test]
fn sync_now_refuses_an_untracked_issue_with_exit_1() {
    let t = Tracker::new();
    t.ok(&["snapshot", "--project", "home"]);
    let out = t.cmd(&["sync-now", "--project", "home", "--issue", "yuvalrakavy/Store#999999"]);
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("not tracked by project home"), "{err}");
}
