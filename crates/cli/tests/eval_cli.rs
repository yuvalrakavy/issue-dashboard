//! `eval refresh-history` and `truth tag` end to end, as separate processes, over the recorded
//! history pages of `crates/core/tests/fixtures/history-replay` (no network).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::Value;

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

/// The fixture config minus Store, as the history pages were recorded.
fn root() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    copy_dir(&fixtures().join("config"), dir.path());
    let home = dir.path().join("projects/home.toml");
    let text = fs::read_to_string(&home).unwrap();
    let store = "[[member]]\nrepo = \"yuvalrakavy/Store\"\n\n";
    assert!(text.contains(store));
    fs::write(&home, text.replace(store, "")).unwrap();
    dir
}

fn cmd(dir: &Path, args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env("ISSUE_DASHBOARD_ROOT", dir)
        .env("ISSUE_DASHBOARD_NOW", "2026-10-29T12:00:00Z")
        .env("ISSUE_DASHBOARD_REPLAY", fixtures().join("history-replay"))
        .env("ISSUE_DASHBOARD_PAGE_SIZE", "5")
        .env_remove("ISSUE_DASHBOARD_RECORD")
        .output()
        .unwrap()
}

fn ok(dir: &Path, args: &[&str]) -> Value {
    let out = cmd(dir, args);
    assert!(
        out.status.success(),
        "{args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    serde_json::from_slice(&out.stdout).unwrap()
}

/// Every ledger backup line's event kind.
fn backup_kinds(dir: &Path) -> Vec<String> {
    let Ok(entries) = fs::read_dir(dir.join("ledger")) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for e in entries {
        let p = e.unwrap().path();
        if p.extension().is_some_and(|e| e == "jsonl") {
            for line in fs::read_to_string(p).unwrap().lines() {
                let v: Value = serde_json::from_str(line).unwrap();
                out.push(v["event"]["kind"].as_str().unwrap().to_string());
            }
        }
    }
    out
}

#[test]
fn truth_tag_lists_without_yes_and_records_with_it() {
    let t = root();
    let h = ok(t.path(), &["eval", "refresh-history", "--project", "home"]);
    let repos = h["repos"].as_array().unwrap();
    assert_eq!(repos.len(), 2, "{h}");

    let tag = [
        "truth",
        "tag",
        "--project",
        "home",
        "--name",
        "oct-review",
        "--decision",
        "type",
        "--from",
        "2024-01-01T00:00:00Z",
        "--to",
        "2026-10-29T00:00:00Z",
    ];
    let listed = ok(t.path(), &tag);
    let n = listed["events"].as_array().unwrap().len();
    assert!(n > 0, "{listed}");
    assert!(listed["recorded"].is_null(), "{listed}");
    assert!(!backup_kinds(t.path()).iter().any(|k| k == "truth_tag"));

    let mut yes = tag.to_vec();
    yes.push("--yes");
    let recorded = ok(t.path(), &yes);
    assert!(!recorded["recorded"].is_null(), "{recorded}");
    assert_eq!(recorded["events"].as_array().unwrap().len(), n);
    assert_eq!(backup_kinds(t.path()).iter().filter(|k| *k == "truth_tag").count(), n);
}
