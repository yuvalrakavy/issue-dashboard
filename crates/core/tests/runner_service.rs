//! The bridge runner's service (runner spec 1.1, 1.2, 3.4): its lock, `runner init`'s
//! refusals, the pin and the rollback stop. The tick is driven in `runner_flow.rs`, beside the
//! launch fixture it steps.

mod common;

use std::fs;

use common::TempRoot;
use common::fixture::fixture_root;
use issue_dashboard_core::Error;
use issue_dashboard_core::bridge::Pin;
use issue_dashboard_core::service::{self, Setup};
use serde_json::json;

fn pin(sha: &str) -> Pin {
    Pin {
        package_version: "0.2.4".into(),
        contract_version: 2,
        executable_sha256: sha.into(),
    }
}

/// A root set up by `runner init`, unpinned.
fn set_up() -> TempRoot {
    let t = fixture_root();
    let setup = Setup {
        connector: "/usr/local/bin/agentbridge-dashboard".into(),
        config: t.path().join("connector.toml"),
        state_dir: t.path().join("connector"),
        pin: None,
        rollback: None,
    };
    service::save(t.path(), &setup).unwrap();
    t
}

fn refusal<T: std::fmt::Debug>(r: Result<T, Error>) -> String {
    match r {
        Err(Error::Refused(m)) => m,
        other => panic!("expected a refusal, got {other:?}"),
    }
}

#[test]
fn a_second_service_on_the_same_root_is_refused_naming_the_holder_s_pid() {
    let t = fixture_root();
    let _held = service::lock(t.path()).unwrap();
    let e = service::lock(t.path()).err().expect("a second lock is refused").to_string();
    assert!(e.contains(&format!("pid={}", std::process::id())), "{e}");
    assert!(e.contains("bridge-serve"), "{e}");
}

#[test]
fn init_refuses_an_existing_setup_and_any_existing_connector_state() {
    let t = fixture_root();
    let state = t.path().join("connector");
    service::check_init(t.path(), &state).unwrap();
    for f in ["dashboard.redb", "transport/journal.redb"] {
        let p = state.join(f);
        fs::create_dir_all(p.parent().unwrap()).unwrap();
        fs::write(&p, "").unwrap();
        assert!(refusal(service::check_init(t.path(), &state)).contains(f), "{f}");
        fs::remove_file(&p).unwrap();
    }
    let t = set_up();
    assert!(refusal(service::check_init(t.path(), &t.path().join("fresh"))).contains("already set up"));
}

#[test]
fn the_service_refuses_to_start_unpinned_and_pin_records_the_connector_s_answer() {
    let t = set_up();
    assert!(refusal(service::load(t.path()).unwrap().startable()).contains("runner pin"));
    service::pin(t.path(), pin("aa"), false).unwrap();
    assert_eq!(service::load(t.path()).unwrap().startable().unwrap(), &pin("aa"));
    assert!(refusal(service::pin(t.path(), pin("bb"), true)).contains("no rollback"));
}

#[test]
fn a_rollback_stops_the_service_until_the_owner_pins_with_accept_rollback() {
    let t = set_up();
    service::pin(t.path(), pin("aa"), false).unwrap();
    let mut s = service::load(t.path()).unwrap();
    s.rollback = Some("AgentBridge no longer knows token res-1".into());
    service::save(t.path(), &s).unwrap();
    assert!(refusal(service::load(t.path()).unwrap().startable()).contains("--accept-rollback"));
    assert!(refusal(service::pin(t.path(), pin("aa"), false)).contains("--accept-rollback"));
    service::pin(t.path(), pin("aa"), true).unwrap();
    let s = service::load(t.path()).unwrap();
    assert_eq!(s.rollback, None);
    assert_eq!(s.startable().unwrap(), &pin("aa"));
}

#[test]
fn no_setup_is_refused_naming_runner_init() {
    let t = fixture_root();
    assert!(refusal(service::load(t.path())).contains("runner init"));
}

#[test]
fn the_cursor_is_the_largest_event_sequence() {
    let events = [json!({"sequence": 7}), json!({"sequence": 9}), json!({"sequence": 8})];
    assert_eq!(service::cursor(&events), Some(9));
    assert_eq!(service::cursor(&[]), None);
}
