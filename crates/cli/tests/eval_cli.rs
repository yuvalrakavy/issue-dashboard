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

fn tag_args<'a>(decision: &'a str, from: &'a str, to: &'a str) -> Vec<&'a str> {
    let mut v = vec![
        "truth",
        "tag",
        "--project",
        "home",
        "--name",
        "oct-review",
        "--decision",
    ];
    v.extend([decision, "--from", from, "--to", to]);
    v
}

fn events(v: &Value) -> Vec<Value> {
    v["events"].as_array().unwrap().clone()
}

fn tags_recorded(dir: &Path) -> usize {
    backup_kinds(dir).iter().filter(|k| *k == "truth_tag").count()
}

#[test]
fn truth_tag_filters_by_decision_window_and_issue_and_a_repeat_records_nothing() {
    let t = root();
    ok(t.path(), &["eval", "refresh-history", "--project", "home"]);
    let (lo, hi) = ("2024-01-01T00:00:00Z", "2026-10-29T00:00:00Z");

    // Decision: priority events carry only p-labels, type events none.
    let prio = events(&ok(t.path(), &tag_args("priority", lo, hi)));
    let typ = events(&ok(t.path(), &tag_args("type", lo, hi)));
    assert!(!prio.is_empty() && !typ.is_empty());
    assert!(
        prio.iter().all(|e| e["label"].as_str().unwrap().starts_with('p')),
        "{prio:?}"
    );
    assert!(
        typ.iter().all(|e| !e["label"].as_str().unwrap().starts_with('p')),
        "{typ:?}"
    );

    // Window: the end bound excludes later events (rhai#1188 is on 2026-10-17, the rest 10-23).
    let early = events(&ok(t.path(), &tag_args("type", lo, "2026-10-20T00:00:00Z")));
    assert!(!early.is_empty() && early.len() < typ.len());
    assert!(
        early.iter().all(|e| e["at"].as_str().unwrap() <= "2026-10-20T00:00:00"),
        "{early:?}"
    );

    // Issues filter.
    let mut one = tag_args("type", lo, hi);
    one.extend(["--issues", "yuvalrakavy/airzoned#4"]);
    let only = events(&ok(t.path(), &one));
    assert!(!only.is_empty() && only.len() < typ.len());
    assert!(only.iter().all(|e| e["alias"] == "yuvalrakavy/airzoned#4"), "{only:?}");

    // A repeat --yes records nothing new.
    let mut yes = tag_args("type", lo, hi);
    yes.push("--yes");
    ok(t.path(), &yes);
    assert_eq!(tags_recorded(t.path()), typ.len());
    ok(t.path(), &yes);
    assert_eq!(tags_recorded(t.path()), typ.len());
    // A -> B -> A: the return to the first name is recorded, so the first is in force again.
    let mut b = yes.clone();
    b[5] = "other-review";
    ok(t.path(), &b);
    ok(t.path(), &yes);
    assert_eq!(tags_recorded(t.path()), 3 * typ.len());
}
