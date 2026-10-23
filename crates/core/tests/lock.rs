//! The root lock (10.4): exclusive, bounded wait, names the holder. The two-process case is
//! in the CLI's tests; these hold the lock through a second open of the same file, which
//! conflicts the same way.

mod common;

use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use common::{t, temp_root_with};
use issue_dashboard_core::Error;
use issue_dashboard_core::lock::RootLock;
use issue_dashboard_core::root::Options;

fn quick(command: &str) -> Options {
    Options {
        command: command.into(),
        lock_wait: Duration::from_millis(400),
        ..Options::default()
    }
}

#[test]
fn a_waiter_gives_up_after_the_bounded_wait_and_names_the_holder() {
    let r = temp_root_with(quick("waiter"));
    let held = RootLock::acquire(&r.root.lock_path(), "holder-command", Duration::from_secs(1)).unwrap();
    let start = Instant::now();
    let err = r.root.mutate(t(0), |_| Ok(())).unwrap_err();
    let waited = start.elapsed();
    match &err {
        Error::LockTimeout { holder: Some(h), .. } => {
            assert!(h.contains("holder-command"), "holder not named: {h}");
            assert!(
                h.contains(&format!("pid={}", std::process::id())),
                "holder pid not named: {h}"
            );
        }
        other => panic!("expected a lock timeout naming the holder, got {other}"),
    }
    assert!(waited >= Duration::from_millis(400), "gave up early: {waited:?}");
    assert!(waited < Duration::from_secs(3), "the wait is not bounded: {waited:?}");
    assert!(err.to_string().contains("holder-command"));
    drop(held);
    r.root.mutate(t(0), |_| Ok(())).unwrap();
}

#[test]
fn a_waiter_proceeds_as_soon_as_the_holder_releases() {
    let r = temp_root_with(Options {
        lock_wait: Duration::from_secs(10),
        ..quick("waiter")
    });
    let held = RootLock::acquire(&r.root.lock_path(), "holder", Duration::from_secs(1)).unwrap();
    let (tx, rx) = mpsc::channel();
    let root = r.root.clone();
    let waiter = thread::spawn(move || {
        let start = Instant::now();
        root.mutate(t(0), |_| Ok(())).unwrap();
        tx.send(start.elapsed()).unwrap();
    });
    thread::sleep(Duration::from_millis(300));
    assert!(rx.try_recv().is_err(), "the waiter ran while the lock was held");
    drop(held);
    let waited = rx.recv_timeout(Duration::from_secs(5)).expect("the waiter never ran");
    waiter.join().unwrap();
    assert!(waited >= Duration::from_millis(300));
}

#[test]
fn the_lock_is_released_when_a_command_fails() {
    let r = temp_root_with(quick("x"));
    let err: Result<(), _> = r.root.mutate(t(0), |_| Err(Error::refused("no")));
    assert!(err.is_err());
    RootLock::acquire(&r.root.lock_path(), "after", Duration::from_millis(100)).expect("the lock leaked");
}

#[test]
fn concurrent_commands_serialise_and_none_is_lost() {
    use issue_dashboard_core::event::Kind;
    use issue_dashboard_core::ledger::{self, Dedupe};
    use serde_json::json;

    let r = temp_root_with(Options {
        lock_wait: Duration::from_secs(20),
        ..quick("worker")
    });
    let handles: Vec<_> = (0..8)
        .map(|i| {
            let root = r.root.clone();
            thread::spawn(move || {
                root.mutate(t(i), |tx| {
                    tx.record(
                        Kind::Suggested,
                        &format!("I_{i}"),
                        json!({"run_id": "r", "decision": "type"}),
                        json!({}),
                        Dedupe::Any,
                    )
                })
                .unwrap();
            })
        })
        .collect();
    for h in handles {
        h.join().unwrap();
    }
    let n = r
        .root
        .read(|l| ledger::events_by_kind(l.conn(), Kind::Suggested))
        .unwrap()
        .len();
    assert_eq!(n, 8);
    let seqs = common::backup_seqs(&r.root);
    let expected: Vec<i64> = (1..=seqs.len() as i64).collect();
    assert_eq!(seqs, expected, "concurrent exports duplicated or lost a line");
}
