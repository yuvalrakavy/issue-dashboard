//! CLI regression tests for the first deep gate's findings.

use std::fs;
use std::process::Command;

const BIN: &str = env!("CARGO_BIN_EXE_issue-dashboard");

#[test]
fn a_malformed_clock_override_is_an_environment_error_exit_2() {
    let dir = tempfile::tempdir().unwrap();
    let out = Command::new(BIN)
        .args([
            "launch-started",
            "--token",
            "res-00000000000000000000000000000000",
            "--session",
            "S",
        ])
        .env("ISSUE_DASHBOARD_ROOT", dir.path())
        .env("ISSUE_DASHBOARD_NOW", "yesterday")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn reserve_refuses_a_zero_capacity_recipe_naming_the_config_file() {
    let dir = tempfile::tempdir().unwrap();
    fs::create_dir_all(dir.path().join("projects")).unwrap();
    fs::write(
        dir.path().join("repos.toml"),
        "[repo.\"a/b\"]\n[launch.lanes]\ncapacity = 0\nworkspace = \"w\"\nrelease = \"r\"\nland = \"l\"\ngate.quick = { run = \"t\", minutes = 1 }\n",
    )
    .unwrap();
    let doc = serde_json::json!({
        "id": "L1", "project": "home", "author": "u", "created_at": "2026-10-23T08:00:00Z", "kind": "fix",
        "items": ["I_x"], "aliases": ["a/b#1"], "recipe": "lanes", "routing": {"model": "m", "effort": "low"}
    });
    let req = dir.path().join("L1.json");
    fs::write(&req, doc.to_string()).unwrap();
    let out = Command::new(BIN)
        .args(["reserve", "--request", req.to_str().unwrap(), "--dispatcher", "D1"])
        .env("ISSUE_DASHBOARD_ROOT", dir.path())
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("repos.toml"), "{err}");
}
