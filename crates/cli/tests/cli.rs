//! The CLI end to end, as separate processes: the launch lifecycle through the commands, exit
//! codes, two processes contending for the root lock, a process killed between the backup
//! append and the cursor update, and rebuild.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use issue_dashboard_core::lock::RootLock;
use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_issue-dashboard");

struct Tracker {
    dir: tempfile::TempDir,
}

impl Tracker {
    fn new(capacity: u32) -> Tracker {
        let dir = tempfile::tempdir().unwrap();
        fs::create_dir_all(dir.path().join("projects")).unwrap();
        fs::write(
            dir.path().join("repos.toml"),
            format!(
                "[repo.\"example-org/server\"]\nlaunch = \"lanes\"\n\n[launch.lanes]\ncapacity = {capacity}\nworkspace = \"w\"\nrelease = \"r\"\nland = \"l\"\ngate.quick = {{ run = \"t\", minutes = 1 }}\n"
            ),
        )
        .unwrap();
        fs::write(
            dir.path().join("projects/home.toml"),
            "schema = 1\nname = \"Home\"\nowner_id = \"user_owner\"\n[[member]]\nrepo = \"example-org/server\"\n",
        )
        .unwrap();
        Tracker { dir }
    }

    fn path(&self) -> &Path {
        self.dir.path()
    }

    fn cmd(&self, args: &[&str], env: &[(&str, &str)]) -> Output {
        let mut c = Command::new(BIN);
        c.args(args)
            .env("ISSUE_DASHBOARD_ROOT", self.path())
            .env("ISSUE_DASHBOARD_NOW", "2026-10-23T08:00:00Z");
        for (k, v) in env {
            c.env(k, v);
        }
        c.output().unwrap()
    }

    /// Runs a command that must succeed and returns its JSON output.
    fn ok(&self, args: &[&str]) -> Value {
        self.ok_env(args, &[])
    }

    fn ok_env(&self, args: &[&str], env: &[(&str, &str)]) -> Value {
        let out = self.cmd(args, env);
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "{args:?}: stdout is not JSON ({e}): {}",
                String::from_utf8_lossy(&out.stdout)
            )
        })
    }

    /// Runs a command that must fail with `code`; returns stderr.
    fn fails(&self, args: &[&str], code: i32) -> String {
        let out = self.cmd(args, &[]);
        assert_eq!(
            out.status.code(),
            Some(code),
            "{args:?}: stdout {} stderr {}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn request_file(&self, id: &str, item: &str) -> PathBuf {
        let doc = json!({
            "id": id,
            "project": "home",
            "author": "user_owner",
            "created_at": "2026-10-23T08:00:00Z",
            "kind": "fix",
            "items": [item],
            "aliases": ["example-org/server#403"],
            "recipe": "lanes",
            "routing": {"model": "claude-opus-5-5", "effort": "medium"},
        });
        let path = self.path().join(format!("{id}.json"));
        fs::write(&path, doc.to_string()).unwrap();
        path
    }

    fn reserve_request(&self, id: &str, item: &str, dispatcher: &str) -> Value {
        let file = self.request_file(id, item);
        self.ok(&[
            "reserve",
            "--request",
            file.to_str().unwrap(),
            "--dispatcher",
            dispatcher,
        ])
    }

    fn backup_seqs(&self) -> Vec<i64> {
        let mut out = Vec::new();
        let dir = self.path().join("ledger");
        let mut files: Vec<_> = fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p.extension().is_some_and(|e| e == "jsonl"))
            .collect();
        files.sort();
        for f in files {
            for line in fs::read_to_string(f).unwrap().lines() {
                out.push(
                    serde_json::from_str::<Value>(line).unwrap()["event"]["seq"]
                        .as_i64()
                        .unwrap(),
                );
            }
        }
        out
    }
}

fn contiguous(seqs: &[i64]) -> bool {
    seqs.iter().copied().eq(1..=seqs.len() as i64)
}

#[test]
fn the_launch_lifecycle_through_the_commands() {
    let t = Tracker::new(2);
    let r = t.reserve_request("L1", "I_kwDOissueA", "D1");
    assert_eq!(r["result"], "reserved", "{r}");
    let token = r["token"].as_str().unwrap().to_string();

    let s = t.ok(&["launch-started", "--token", &token, "--session", "S1"]);
    assert_eq!(s, json!({"result": "started", "launch": "L1", "repeat": false}));
    // The dispatcher retries after a lock timeout: a no-op returning the recorded result.
    let again = t.ok(&["launch-started", "--token", &token, "--session", "S1"]);
    assert_eq!(again, json!({"result": "started", "launch": "L1", "repeat": true}));
    let orphan = t.ok(&["launch-started", "--token", &token, "--session", "S2"]);
    assert_eq!(orphan["result"], "orphan");

    let e = t.ok(&["launch-ended", "--token", &token, "--state", "landed"]);
    assert_eq!(e["state"], "released");
    let o = t.ok(&[
        "record-outcome",
        "--launch",
        "L1",
        "--landed",
        "true",
        "--commit",
        "95c2531bb",
        "--gate",
        "quick=2",
        "--wall-clock-minutes",
        "40",
        "--findings-fixed",
        "1",
        "--context-tokens",
        "1000",
    ]);
    assert_eq!(o["repeat"], false);
    assert_eq!(o["missing"], json!([]));
    let o2 = t.ok(&[
        "record-outcome",
        "--launch",
        "L1",
        "--landed",
        "true",
        "--commit",
        "95c2531bb",
        "--gate",
        "quick=2",
        "--wall-clock-minutes",
        "40",
        "--findings-fixed",
        "1",
        "--context-tokens",
        "1000",
    ]);
    assert_eq!(o2["repeat"], true);
    assert!(contiguous(&t.backup_seqs()));
}

#[test]
fn a_refusal_exits_1_and_names_the_reason() {
    let t = Tracker::new(2);
    let r = t.reserve_request("L1", "I_kwDOissueA", "D1");
    let token = r["token"].as_str().unwrap().to_string();
    // reserved -> ended is not in 10.7's table.
    let err = t.fails(&["launch-ended", "--token", &token, "--state", "landed"], 1);
    assert!(err.contains("only a started launch can end"), "{err}");
    let err = t.fails(
        &[
            "launch-started",
            "--token",
            "res-00000000000000000000000000000000",
            "--session",
            "S",
        ],
        1,
    );
    assert!(err.contains("no reservation has token"), "{err}");
    // A second launch for the same issue is refused while the first holds it.
    let file = t.request_file("L2", "I_kwDOissueA");
    let err = t.fails(
        &["reserve", "--request", file.to_str().unwrap(), "--dispatcher", "D1"],
        1,
    );
    assert!(err.contains("held by launch L1"), "{err}");
    let err = t.fails(&["record-outcome", "--launch", "L_none", "--landed", "false"], 1);
    assert!(err.contains("no launch L_none"), "{err}");
}

#[test]
fn reserve_queues_on_a_full_pool_and_next_takes_the_freed_slot() {
    let t = Tracker::new(1);
    let token = t.reserve_request("L1", "I_kwDOissueA", "D1")["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(t.reserve_request("L2", "I_kwDOissueB", "D1")["result"], "queued");
    assert_eq!(
        t.ok(&["reserve", "--next", "--dispatcher", "D1"]),
        json!({"result": "nothing_queued"})
    );
    t.ok(&["launch-started", "--token", &token, "--session", "S1"]);
    t.ok(&["launch-ended", "--token", &token, "--state", "stopped"]);
    let next = t.ok(&["reserve", "--next", "--dispatcher", "D1"]);
    assert_eq!(next["result"], "reserved");
    assert_eq!(next["launch"], "L2");
}

#[test]
fn release_waits_fifteen_minutes_and_needs_the_owner() {
    let t = Tracker::new(1);
    let token = t.reserve_request("L1", "I_kwDOissueA", "D1")["token"]
        .as_str()
        .unwrap()
        .to_string();
    let err = t.fails(&["release", "--token", &token, "--dispatcher", "D1"], 1);
    assert!(err.contains("15 minutes"), "{err}");
    let later = [("ISSUE_DASHBOARD_NOW", "2026-10-23T08:16:00Z")];
    let out = t.cmd(&["release", "--token", &token, "--dispatcher", "D2"], &later);
    assert_eq!(out.status.code(), Some(1), "another dispatcher released it");
    let r = t.ok_env(&["release", "--token", &token, "--dispatcher", "D1"], &later);
    assert_eq!(r["state"], "released");
}

#[test]
fn unconfirm_then_started() {
    let t = Tracker::new(1);
    let token = t.reserve_request("L1", "I_kwDOissueA", "D1")["token"]
        .as_str()
        .unwrap()
        .to_string();
    assert_eq!(
        t.ok(&["unconfirm", "--token", &token, "--dispatcher", "D1"])["state"],
        "unconfirmed"
    );
    assert_eq!(
        t.ok(&["launch-started", "--token", &token, "--session", "S1"])["result"],
        "started"
    );
}

#[test]
fn an_orphan_is_settled_with_release_orphan_session() {
    let t = Tracker::new(1);
    let token = t.reserve_request("L1", "I_kwDOissueA", "D1")["token"]
        .as_str()
        .unwrap()
        .to_string();
    t.ok(&["launch-started", "--token", &token, "--session", "S1"]);
    t.ok(&["launch-started", "--token", &token, "--session", "S2"]);
    t.ok(&["release", "--token", &token, "--orphan-session", "S2"]);
    t.fails(&["release", "--token", &token, "--orphan-session", "S2"], 1);
}

#[test]
fn a_second_process_waiting_for_the_lock_gives_up_and_names_the_holder() {
    let t = Tracker::new(1);
    t.reserve_request("L1", "I_kwDOissueA", "D1");
    // This test process holds the root lock; the CLI is a second process.
    let held = RootLock::acquire(&t.path().join("lock"), "holding-test", Duration::from_secs(1)).unwrap();
    let start = Instant::now();
    let out = t.cmd(
        &["reserve", "--launch", "L1", "--dispatcher", "D1"],
        &[("ISSUE_DASHBOARD_LOCK_WAIT_MS", "600")],
    );
    let waited = start.elapsed();
    assert_eq!(out.status.code(), Some(2));
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("holding-test") && err.contains(&format!("pid={}", std::process::id())),
        "{err}"
    );
    assert!(
        waited >= Duration::from_millis(600) && waited < Duration::from_secs(10),
        "{waited:?}"
    );
    drop(held);
    assert_eq!(
        t.ok(&["reserve", "--launch", "L1", "--dispatcher", "D1"])["repeat"],
        true
    );
}

#[test]
fn two_processes_reserving_at_once_serialise_on_the_lock() {
    let t = Tracker::new(1);
    let f1 = t.request_file("L1", "I_kwDOissueA");
    let f2 = t.request_file("L2", "I_kwDOissueB");
    let spawn = |f: &Path| {
        Command::new(BIN)
            .args(["reserve", "--request", f.to_str().unwrap(), "--dispatcher", "D1"])
            .env("ISSUE_DASHBOARD_ROOT", t.path())
            .env("ISSUE_DASHBOARD_NOW", "2026-10-23T08:00:00Z")
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap()
    };
    let (a, b) = (spawn(&f1), spawn(&f2));
    let results: Vec<Value> = [a, b]
        .into_iter()
        .map(|c| {
            let out = c.wait_with_output().unwrap();
            assert!(out.status.success());
            serde_json::from_slice(&out.stdout).unwrap()
        })
        .collect();
    let mut kinds: Vec<&str> = results.iter().map(|r| r["result"].as_str().unwrap()).collect();
    kinds.sort();
    assert_eq!(kinds, ["queued", "reserved"], "one slot: exactly one reservation");
    assert!(contiguous(&t.backup_seqs()));
}

#[test]
fn a_process_killed_between_append_and_cursor_update_loses_and_duplicates_nothing() {
    let t = Tracker::new(2);
    t.reserve_request("L1", "I_kwDOissueA", "D1");
    let before = t.backup_seqs();
    let file = t.request_file("L2", "I_kwDOissueB");
    let out = t.cmd(
        &["reserve", "--request", file.to_str().unwrap(), "--dispatcher", "D1"],
        &[("ISSUE_DASHBOARD_FAILPOINT", "after-append")],
    );
    assert!(!out.status.success(), "the failpoint did not kill the process");
    assert!(
        out.status.code().is_none(),
        "expected death by signal (abort), got {:?}",
        out.status
    );
    let after_crash = t.backup_seqs();
    assert!(
        after_crash.len() > before.len(),
        "nothing was appended before the kill: the scenario did not happen"
    );

    // The reservation was committed before the kill; a retry returns it, and the next
    // command's export appends nothing twice.
    let retry = t.ok(&["reserve", "--launch", "L2", "--dispatcher", "D1"]);
    assert_eq!(retry["repeat"], true);
    let seqs = t.backup_seqs();
    assert!(contiguous(&seqs), "{seqs:?}");
}

#[test]
fn rebuild_from_the_backup_reproduces_the_launch_state() {
    let t = Tracker::new(2);
    let token = t.reserve_request("L1", "I_kwDOissueA", "D1")["token"]
        .as_str()
        .unwrap()
        .to_string();
    t.ok(&["launch-started", "--token", &token, "--session", "S1"]);
    t.fails(&["rebuild"], 1);
    let r = t.ok(&["rebuild", "--force"]);
    assert!(r["events"].as_u64().unwrap() >= 4);
    assert!(r["moved_aside"].is_string());
    // The rebuilt ledger knows the launch is started.
    assert_eq!(
        t.ok(&["launch-started", "--token", &token, "--session", "S1"])["repeat"],
        true
    );
}

#[test]
fn rebuild_refuses_a_corrupt_middle_line_with_exit_1() {
    let t = Tracker::new(2);
    let token = t.reserve_request("L1", "I_kwDOissueA", "D1")["token"]
        .as_str()
        .unwrap()
        .to_string();
    t.ok(&["launch-started", "--token", &token, "--session", "S1"]);
    let file = t.path().join("ledger/2026-10.jsonl");
    let text = fs::read_to_string(&file).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut v: Value = serde_json::from_str(&lines[1]).unwrap();
    v["event"]["time"] = json!("2030-01-01T00:00:00.000Z");
    lines[1] = v.to_string();
    fs::write(&file, lines.join("\n") + "\n").unwrap();
    fs::remove_file(t.path().join("ledger.sqlite")).unwrap();
    let err = t.fails(&["rebuild"], 1);
    assert!(err.contains("2026-10.jsonl:2"), "{err}");
}
