//! The runner's live status row and its hold reasons (runner spec 1.3, 4.2).

mod common;

use chrono::Duration;
use common::{t, temp_root};
use issue_dashboard_core::runner_status::{self, Held};
use serde_json::json;

#[test]
fn a_restart_replaces_the_process_and_keeps_the_cursor() {
    let r = temp_root();
    r.root.mutate(t(0), |tx| runner_status::started(tx.conn(), 11, t(0), "0.2.4", 2)).unwrap();
    let health = json!({"Codex": "healthy"});
    r.root.mutate(t(1), |tx| runner_status::ticked(tx.conn(), t(1), Some(40), &health)).unwrap();
    r.root.mutate(t(2), |tx| runner_status::started(tx.conn(), 12, t(2), "0.2.4", 2)).unwrap();
    let s = r.root.read(|l| runner_status::status(l.conn())).unwrap().unwrap();
    assert_eq!((s.pid, s.cursor, s.version.as_str(), s.contract), (12, Some(40), "0.2.4", 2));
    assert_eq!(s.started_at, "2026-10-23T08:02:00.000Z");
}

#[test]
fn a_tick_older_than_sixty_seconds_reads_as_down() {
    let r = temp_root();
    assert!(runner_status::is_down(None, t(0)).unwrap(), "no status is down");
    r.root.mutate(t(0), |tx| runner_status::started(tx.conn(), 11, t(0), "0.2.4", 2)).unwrap();
    r.root.mutate(t(0), |tx| runner_status::ticked(tx.conn(), t(0), None, &json!({}))).unwrap();
    let s = r.root.read(|l| runner_status::status(l.conn())).unwrap();
    assert!(!runner_status::is_down(s.as_ref(), t(1)).unwrap(), "60 s old is not down");
    assert!(runner_status::is_down(s.as_ref(), t(1) + Duration::seconds(1)).unwrap());
}

#[test]
fn a_hold_keeps_its_since_until_the_reason_changes_and_unhold_clears_it() {
    let r = temp_root();
    let held = || r.root.read(|l| runner_status::held(l.conn())).unwrap();
    r.root.mutate(t(0), |tx| runner_status::hold(tx.conn(), "L1", "kind disabled", t(0))).unwrap();
    r.root.mutate(t(1), |tx| runner_status::hold(tx.conn(), "L1", "kind disabled", t(1))).unwrap();
    assert_eq!(
        held(),
        [Held {
            launch: "L1".into(),
            reason: "kind disabled".into(),
            since: "2026-10-23T08:00:00.000Z".into()
        }]
    );
    r.root.mutate(t(2), |tx| runner_status::hold(tx.conn(), "L1", "harness Codex unhealthy", t(2))).unwrap();
    assert_eq!(held()[0].since, "2026-10-23T08:02:00.000Z");
    r.root.mutate(t(3), |tx| runner_status::unhold(tx.conn(), "L1")).unwrap();
    assert!(held().is_empty());
}
