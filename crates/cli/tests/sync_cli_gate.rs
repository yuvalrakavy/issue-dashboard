//! Gate (mutation pass, slice 3): what the sync subcommands do with their arguments, which the
//! core tests cannot see: the version read reaches the pin, the marker listing reaches the clean
//! list, the clock reaches the abandon test, and `cleanup-plan` runs at all.

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

fn stdout(o: &Output) -> Value {
    assert!(o.status.success(), "{}", String::from_utf8_lossy(&o.stderr));
    serde_json::from_slice(&o.stdout).unwrap()
}

fn read(path: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn write(path: &Path, v: Value) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, v.to_string()).unwrap();
}

fn marker(seq: i64, created: &str) -> Value {
    json!({"generation": format!("gen-{seq}"), "sequence": seq, "created_at": created,
           "counts": {"issues": 1, "acks": 0, "launch_status": 0, "suggestions": 0, "plans": 0, "forks": 0, "metrics": 0}})
}

fn plan_with_markers(d: &Path) -> Value {
    let outbox = d.join("outbox.json");
    write(
        &outbox,
        json!({
            "project": "home", "sequence": 7,
            "meta": {"generation": "gen-7", "sequence": 7},
            "issues": [{"id": "I_1"}],
            "acks": [], "launch_status": [], "suggestions": [], "plans": [], "forks": [], "metrics": [],
        }),
    );
    let active = d.join("active.json");
    write(&active, json!({"generation": "gen-5", "sequence": 5}));
    let markers = d.join("markers");
    write(&markers.join("gens/gen-5.json"), marker(5, "2026-10-23T07:50:00Z")); // active
    write(&markers.join("gens/gen-3.json"), marker(3, "2026-10-23T07:55:00Z")); // older than active
    write(&markers.join("gens/gen-6.json"), marker(6, "2026-10-23T07:50:00Z")); // another run's, 10 minutes old
    write(&markers.join("gens/gen-6b.json"), marker(6, "2026-10-23T05:00:00Z")); // key does not name its sequence
    fs::write(markers.join("gens/readme.txt"), "not a marker").unwrap();
    stdout(&run(&[
        "publish-plan",
        "--outbox",
        outbox.to_str().unwrap(),
        "--active",
        active.to_str().unwrap(),
        "--active-version",
        "4",
        "--markers",
        markers.to_str().unwrap(),
        "--out",
        d.join("plan").to_str().unwrap(),
    ]))
}

// Kills: `version: *version` lost on the way to the pin; the markers listing ignored, read
// without its `.json` filter, or keyed by anything but the file stem.
#[test]
fn publish_plan_pins_the_version_read_and_cleans_from_the_marker_listing() {
    let d = tempfile::tempdir().unwrap();
    let s = plan_with_markers(d.path());
    assert_eq!(s["decision"], "publish", "{s}");
    let switch = read(s["switch"][0].as_str().unwrap());
    assert_eq!(switch[0]["if_version"], json!(4));
    assert_eq!(s["clean"], json!([{"generation": "gen-3", "list": ["gens/gen-3/issues"]}]));
}

// Kills: the clock (ISSUE_DASHBOARD_NOW) not reaching the abandon test.
#[test]
fn publish_plan_judges_abandoned_generations_by_the_clock() {
    let d = tempfile::tempdir().unwrap();
    plan_with_markers(d.path());
    let out = d.path().join("later");
    let o = Command::new(BIN)
        .args([
            "publish-plan",
            "--outbox",
            d.path().join("outbox.json").to_str().unwrap(),
            "--active",
            d.path().join("active.json").to_str().unwrap(),
            "--active-version",
            "4",
            "--markers",
            d.path().join("markers").to_str().unwrap(),
            "--out",
            out.to_str().unwrap(),
        ])
        .env_remove("ISSUE_DASHBOARD_ROOT")
        .env("ISSUE_DASHBOARD_NOW", "2026-10-23T09:00:00Z")
        .output()
        .unwrap();
    let s = stdout(&o);
    // Ten minutes old at 08:00, over an hour old at 09:00: gen-6 is now abandoned.
    let names: Vec<&str> = s["clean"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["generation"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["gen-3", "gen-6"], "{s}");
}

// Kills: `cleanup-plan` dispatching to nothing, or not passing its three paths through.
#[test]
fn cleanup_plan_deletes_what_the_listing_holds_for_the_generations_the_plan_names() {
    let d = tempfile::tempdir().unwrap();
    plan_with_markers(d.path());
    let listing = d.path().join("listing");
    write(&listing.join("gens/gen-3/issues/I_1.json"), json!({"id": "I_1"}));
    write(&listing.join("gens/gen-5/issues/I_keep.json"), json!({"id": "I_keep"}));
    let out = d.path().join("clean-out");
    let r = stdout(&run(&[
        "cleanup-plan",
        "--plan",
        d.path().join("plan/plan.json").to_str().unwrap(),
        "--listing",
        listing.to_str().unwrap(),
        "--out",
        out.to_str().unwrap(),
    ]));
    assert_eq!(r["generations"], json!(["gen-3"]));
    let batch = read(r["batches"][0].as_str().unwrap());
    let ids: Vec<(&str, &str)> = batch
        .as_array()
        .unwrap()
        .iter()
        .map(|w| (w["collection"].as_str().unwrap(), w["doc_id"].as_str().unwrap()))
        .collect();
    assert_eq!(ids, [("gens/gen-3/issues", "I_1"), ("gens", "gen-3")]);
    assert!(batch.as_array().unwrap().iter().all(|w| w["op"] == "delete" && w["if_version"] == json!(1)));
}

// Kills: `cleanup-plan` accepting a plan that stopped.
#[test]
fn cleanup_plan_refuses_a_plan_that_did_not_publish_with_exit_one() {
    let d = tempfile::tempdir().unwrap();
    let plan = d.path().join("plan.json");
    write(&plan, json!({"decision": "stop", "reason": "older"}));
    let o = run(&[
        "cleanup-plan",
        "--plan",
        plan.to_str().unwrap(),
        "--listing",
        d.path().to_str().unwrap(),
        "--out",
        d.path().join("o").to_str().unwrap(),
    ]);
    assert_eq!(o.status.code(), Some(1), "{}", String::from_utf8_lossy(&o.stderr));
}
