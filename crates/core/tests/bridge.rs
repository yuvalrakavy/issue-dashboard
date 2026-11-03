//! The AgentBridge connector client (runner spec 1.2): the process transport against a scripted
//! python3 child, and the typed calls against the fake.

use std::ffi::OsString;
use std::path::Path;
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use issue_dashboard_core::Error;
use issue_dashboard_core::bridge::fake::{Answer, Fake};
use issue_dashboard_core::bridge::{self, Bridge, Pin, Process, TokenStatus};
use serde_json::json;

/// A connector stand-in: `slow` answers after 0.5 s, so a later `fast` answers first; `fail`
/// answers an error; `die` exits without answering; `hang` never answers.
const CHILD: &str = r#"
import json, sys, threading, time
lock = threading.Lock()
def answer(msg):
    with lock:
        sys.stdout.write(json.dumps(msg) + "\n"); sys.stdout.flush()
for line in sys.stdin:
    req = json.loads(line)
    m, rid = req["method"], req["id"]
    if m == "slow":
        threading.Timer(0.5, answer, [{"schema_version": 1, "id": rid, "result": "slow"}]).start()
    elif m == "fast":
        answer({"schema_version": 1, "id": rid, "result": "fast"})
    elif m == "fail":
        answer({"schema_version": 1, "id": rid, "error": {"code": "busy", "message": "try later"}})
    elif m == "die":
        sys.exit(3)
    elif m == "hang":
        pass
"#;

fn child() -> Process {
    Process::spawn(Path::new("python3"), &[OsString::from("-c"), OsString::from(CHILD)]).unwrap()
}

#[test]
fn responses_route_by_id_when_they_arrive_out_of_order() {
    let p = Arc::new(child());
    let slow = {
        let p = p.clone();
        thread::spawn(move || p.call("slow", json!({})).unwrap())
    };
    thread::sleep(Duration::from_millis(100));
    let started = Instant::now();
    assert_eq!(p.call("fast", json!({})).unwrap(), "fast");
    assert!(started.elapsed() < Duration::from_millis(400), "fast waited behind slow");
    assert_eq!(slow.join().unwrap(), "slow");
}

#[test]
fn an_error_response_carries_its_code_and_message() {
    let p = child();
    match p.call("fail", json!({})) {
        Err(Error::Bridge { method, code, message }) => {
            assert_eq!((method.as_str(), code.as_deref(), message.as_str()), ("fail", Some("busy"), "try later"));
        }
        other => panic!("expected a bridge error, got {other:?}"),
    }
}

#[test]
fn a_child_that_dies_fails_the_open_request_and_every_later_one_with_no_code() {
    let p = child();
    match p.call("die", json!({})) {
        Err(Error::Bridge { code: None, .. }) => {}
        other => panic!("expected an unreachable error, got {other:?}"),
    }
    match p.call("fast", json!({})) {
        Err(Error::Bridge { code: None, .. }) => {}
        other => panic!("a dead connector answered {other:?}"),
    }
    assert!(p.stopped().is_some());
}

#[test]
fn no_answer_within_the_bound_is_unreachable() {
    let mut p = child();
    p.bound = Duration::from_millis(300);
    let started = Instant::now();
    match p.call("hang", json!({})) {
        Err(Error::Bridge { code: None, message, .. }) => assert!(message.contains("no answer"), "{message}"),
        other => panic!("expected a timeout, got {other:?}"),
    }
    assert!(started.elapsed() < Duration::from_secs(2));
    // The connector still runs: a later request answers.
    assert_eq!(p.call("fast", json!({})).unwrap(), "fast");
}

#[test]
fn shutdown_closes_input_and_the_child_exits() {
    let p = child();
    p.shutdown(Duration::from_secs(2));
    assert!(p.stopped().is_some());
}

#[test]
fn an_unknown_tracker_token_reads_as_unknown_and_other_errors_propagate() {
    let f = Fake::new();
    assert_eq!(bridge::status(&f, "op-1").unwrap(), TokenStatus::Unknown);
    f.push("status", Answer::Err("busy", "too many".into()));
    assert!(matches!(bridge::status(&f, "op-1"), Err(Error::Bridge { code: Some(_), .. })));
    f.push("status", Answer::Down);
    assert!(matches!(bridge::status(&f, "op-1"), Err(Error::Bridge { code: None, .. })));
    let body = json!({"state": "result", "never_started": false, "routing": {"model": "m", "effort": "high", "verified": true}});
    f.set_status("op-1", body.clone());
    let TokenStatus::Known(s) = bridge::status(&f, "op-1").unwrap() else {
        panic!("known token read as unknown")
    };
    assert_eq!((s.state.as_str(), &s.body), ("result", &body));
    assert_eq!(s.routing_matches("m", "high"), Some(true));
    assert_eq!(s.routing_matches("m", "low"), Some(false));
}

#[test]
fn the_handshake_refuses_a_connector_that_is_not_the_pinned_one() {
    let pin = Pin {
        package_version: "0.2.4".into(),
        contract_version: 2,
        executable_sha256: "aa".into(),
    };
    let caps = |sha: &str| {
        json!({"connector_contract_version": 2, "package_version": "0.2.4", "executable": {"status": "ok", "sha256": sha}})
    };
    let f = Fake::new();
    f.push("capabilities", Answer::Ok(caps("aa")));
    bridge::handshake(&f, &pin).unwrap();
    let require = &f.calls_to("capabilities")[0]["require"];
    assert_eq!(require["contract_version"], 2);
    assert_eq!(require["features"], json!(["bulk_transfer", "stage_results", "event_wait"]));
    f.push("capabilities", Answer::Ok(caps("bb")));
    assert!(matches!(bridge::handshake(&f, &pin), Err(Error::Refused(_))));
}

#[test]
fn a_wait_asks_for_at_most_a_minute() {
    let f = Fake::new();
    bridge::wait(&f, 4, 600_000, 10).unwrap();
    bridge::wait(&f, 4, 5_000, 10).unwrap();
    let asked: Vec<_> = f.calls_to("wait").iter().map(|p| p["timeout_ms"].clone()).collect();
    assert_eq!(asked, [json!(60_000), json!(5_000)]);
}

#[test]
fn a_harness_is_ready_only_when_health_says_ready_to_attempt_true() {
    let f = Fake::new();
    f.push(
        "health",
        Answer::Ok(json!({"harnesses": [
            {"profile": "Codex", "ready_to_attempt": true},
            {"profile": "Hermes", "ready_to_attempt": false},
            {"profile": "Claude"},
            {"profile": "OpenCode", "ready_to_attempt": "true"},
        ]})),
    );
    let names = ["Codex", "Hermes", "Claude", "OpenCode"].map(String::from);
    let h = bridge::health(&f, &names).unwrap();
    let want: std::collections::HashMap<String, bool> =
        [("Codex", true), ("Hermes", false), ("Claude", false), ("OpenCode", false)]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
    assert_eq!(h, want);
}

#[test]
fn a_start_over_the_size_limit_is_refused_before_it_is_sent() {
    let f = Fake::new();
    let s = bridge::Start {
        operation_id: "op".into(),
        launch_id: "L".into(),
        issue_id: "I".into(),
        issue_revision: "r".into(),
        profile: "Codex".into(),
        model: "m".into(),
        effort: "high".into(),
        brief: "x".repeat(bridge::START_MAX_BYTES),
        cwd: None,
        access: None,
        prepared: serde_json::Value::Null,
    };
    assert!(matches!(bridge::start(&f, &s), Err(Error::Refused(_))));
    assert!(f.calls_to("start").is_empty());
}
