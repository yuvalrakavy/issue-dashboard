//! The sync helpers as the skill runs them: no tracker root, files in and out, and the exit
//! codes of chapter 8 (0 for a planned stop, 1 for a refusal).

use std::fs;
use std::path::Path;
use std::process::{Command, Output};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_issue-dashboard");

fn run(args: &[&str]) -> Output {
    Command::new(BIN)
        .args(args)
        .env_remove("ISSUE_DASHBOARD_ROOT")
        .env("ISSUE_DASHBOARD_NOW", "2026-10-23T08:00:00Z")
        .output()
        .unwrap()
}

fn outbox(dir: &Path, seq: i64) -> String {
    let p = dir.join(format!("{seq}.json"));
    let doc = json!({
        "project": "home",
        "sequence": seq,
        "taken_at": "2026-10-23T08:00:00Z",
        "meta": {"generation": format!("gen-{seq}"), "sequence": seq, "taken_at": "2026-10-23T08:00:00Z", "repos": []},
        "issues": [{"id": "I_1", "title": "one"}],
        "acks": [], "launch_status": [], "suggestions": [], "plans": [], "forks": [], "metrics": [],
    });
    fs::write(&p, doc.to_string()).unwrap();
    p.display().to_string()
}

fn stdout(o: &Output) -> Value {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}

#[test]
fn publish_plan_runs_without_a_root_and_a_stale_outbox_is_a_stop_not_an_error() {
    let d = tempfile::tempdir().unwrap();
    let out = d.path().join("plan");
    let first = stdout(&run(&[
        "publish-plan",
        "--outbox",
        &outbox(d.path(), 7),
        "--out",
        out.to_str().unwrap(),
    ]));
    assert_eq!(first["decision"], "publish");

    // The page already shows gen-9: an outbox of 7 publishes nothing.
    let active = d.path().join("active.json");
    fs::write(&active, json!({"generation": "gen-9", "sequence": 9}).to_string()).unwrap();
    let out2 = d.path().join("plan2");
    let stop = stdout(&run(&[
        "publish-plan",
        "--outbox",
        &outbox(d.path(), 7),
        "--active",
        active.to_str().unwrap(),
        "--active-version",
        "4",
        "--out",
        out2.to_str().unwrap(),
    ]));
    assert_eq!(stop["decision"], "stop");

    // A marker for the outbox's own generation: refused, exit 1.
    let markers = d.path().join("markers");
    fs::create_dir_all(markers.join("gens")).unwrap();
    fs::write(markers.join("gens/gen-10.json"), json!({"sequence": 10}).to_string()).unwrap();
    let o = run(&[
        "publish-plan",
        "--outbox",
        &outbox(d.path(), 10),
        "--active",
        active.to_str().unwrap(),
        "--active-version",
        "4",
        "--markers",
        markers.to_str().unwrap(),
        "--out",
        d.path().join("plan3").to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", String::from_utf8_lossy(&o.stderr));
}

#[test]
fn an_active_meta_without_its_version_is_a_usage_error() {
    let d = tempfile::tempdir().unwrap();
    let active = d.path().join("active.json");
    fs::write(&active, "{}").unwrap();
    let o = run(&[
        "publish-plan",
        "--outbox",
        &outbox(d.path(), 7),
        "--active",
        active.to_str().unwrap(),
        "--out",
        d.path().join("p").to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(2));
}

#[test]
fn pages_writes_the_file_cycle_reads() {
    let d = tempfile::tempdir().unwrap();
    fs::create_dir_all(d.path().join("listing/decisions")).unwrap();
    fs::write(
        d.path().join("listing/decisions/D_1.json"),
        json!({"id": "D_1"}).to_string(),
    )
    .unwrap();
    let out = d.path().join("pages.json");
    let r = stdout(&run(&[
        "pages",
        "--dir",
        d.path().join("listing").to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]));
    assert_eq!(r["decisions"], 1);
    let p: Value = serde_json::from_str(&fs::read_to_string(&out).unwrap()).unwrap();
    assert_eq!(
        p,
        json!({"decisions": [{"id": "D_1"}], "launches": [], "views": [], "requests": []})
    );
}

#[test]
fn publish_plan_says_unchanged_and_pins_the_heartbeat_to_its_version() {
    let d = tempfile::tempdir().unwrap();
    let first = stdout(&run(&[
        "publish-plan",
        "--outbox",
        &outbox(d.path(), 7),
        "--out",
        d.path().join("p7").to_str().unwrap(),
    ]));
    assert_eq!(first["decision"], "publish");
    // The page now holds the switch's meta/active, fingerprint and all.
    let active = d.path().join("p7/docs/meta/active.json");
    // `meta/heartbeat` as read: the quiet run of `sequence`, at `version`.
    let plan = |heartbeat: Option<(i64, &str)>, out: &str| {
        let mut args = vec![
            "publish-plan".to_string(),
            "--outbox".into(),
            outbox(d.path(), 8),
            "--active".into(),
            active.display().to_string(),
            "--active-version".into(),
            "4".into(),
            "--out".into(),
            d.path().join(out).display().to_string(),
        ];
        if let Some((sequence, v)) = heartbeat {
            let file = d.path().join(format!("{out}-heartbeat.json"));
            let doc = json!({"schema": 1, "project": "home", "generation": "gen-7", "sequence": sequence,
                "synced_at": "2026-10-23T08:00:00Z", "repos": []});
            std::fs::write(&file, doc.to_string()).unwrap();
            args.extend([
                "--heartbeat".to_string(),
                file.display().to_string(),
                "--heartbeat-version".into(),
                v.to_string(),
            ]);
        }
        stdout(&run(&args.iter().map(String::as_str).collect::<Vec<_>>()))
    };
    // A quiet run newer than this outbox already confirmed the page: a stop.
    let s = plan(Some((9, "12")), "p8c");
    assert_eq!(s["decision"], "stop", "{s}");
    let s = plan(Some((7, "12")), "p8");
    assert_eq!(s["decision"], "unchanged", "{s}");
    assert_eq!(s["generation"], "gen-7");
    assert_eq!(s["heartbeat"][0]["doc_id"], "heartbeat");
    assert_eq!(s["heartbeat"][0]["if_version"], json!(12));
    let s = plan(None, "p8b");
    assert_eq!(s["heartbeat"][0].get("if_version"), None);
}
