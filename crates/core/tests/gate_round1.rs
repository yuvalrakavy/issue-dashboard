//! Regression tests for the first deep gate's findings (see docs/process/gate-kpi.md).

mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use common::{ISSUE_A, ISSUE_B, OWNER, backup_lines, backup_seqs, launch_doc, set_priority, t, temp_root};
use issue_dashboard_core::config::ConfigSet;
use issue_dashboard_core::decision::{self, Status};
use issue_dashboard_core::docs::Priority;
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::launch::{self, Reservation, StartResult, Via};
use issue_dashboard_core::ledger;
use issue_dashboard_core::lock::RootLock;
use issue_dashboard_core::observe::{self, ItemKind, Observation, Observed};
use issue_dashboard_core::outcome::{self, Outcome, RecordedBy};
use issue_dashboard_core::root::Options;
use issue_dashboard_core::{Error, Result, Root};
use serde_json::{Value, json};

fn obs_at(label: &str, alias: &str, updated: DateTime<Utc>, read: DateTime<Utc>) -> Observation {
    Observation {
        node_id: ISSUE_A.into(),
        item: ItemKind::Issue,
        alias: alias.into(),
        updated_at: updated,
        read_at: read,
        read_id: format!("read-{}", read.timestamp_micros()),
        content: json!({
            "state": "open",
            "labels": [label],
            "assignees": [],
            "links": {"parent": null, "sub_issues": [], "blocked_by": [], "blocking": []},
            "timeline_ids": [],
        }),
    }
}

fn obs(label: &str, minutes: i64) -> Observation {
    obs_at(label, "example-org/server#403", t(minutes), t(minutes))
}

fn observe(root: &Root, o: &Observation) -> Observed {
    root.mutate(o.read_at, |tx| observe::record(tx, o)).unwrap()
}

fn contiguous(seqs: &[i64]) -> bool {
    seqs.iter().copied().eq(1..=seqs.len() as i64)
}

fn some_history(root: &Root) {
    for (i, l) in ["a", "b", "c", "d"].iter().enumerate() {
        observe(root, &obs(l, i as i64));
    }
}

// --- a ledger missing or behind its backup -------------------------------------------------

#[test]
fn a_missing_ledger_beside_a_backup_refuses_and_leaves_the_backup_alone() {
    let r = temp_root();
    some_history(&r.root);
    let before = backup_lines(&r.root);
    fs::remove_file(r.root.ledger_path()).unwrap();
    let err = r.root.mutate(t(10), |_| Ok(())).unwrap_err();
    assert!(matches!(&err, Error::Refused(m) if m.contains("rebuild")), "{err}");
    assert_eq!(backup_lines(&r.root), before, "the backup was touched");
    let moved = fs::read_dir(r.root.backup().dir())
        .unwrap()
        .filter(|e| e.as_ref().unwrap().path().is_dir())
        .count();
    assert_eq!(moved, 0, "the backup was moved aside");
    // ...and rebuild then restores it.
    r.root.rebuild(t(11), false).unwrap();
    r.root.mutate(t(12), |_| Ok(())).unwrap();
    assert!(contiguous(&backup_seqs(&r.root)));
}

#[test]
fn reading_a_root_without_a_ledger_does_not_create_one() {
    let r = temp_root();
    assert!(r.root.read(|l| ledger::max_seq(l.conn())).is_err());
    assert!(!r.root.ledger_path().exists());
}

#[test]
fn rebuild_never_adopts_a_stale_wal_left_beside_a_missing_ledger() {
    let r = temp_root();
    some_history(&r.root);
    let in_backup = backup_lines(&r.root).len();
    // A real WAL of another ledger, with frames not yet checkpointed, left where ours was.
    let other = temp_root();
    other.root.mutate(t(0), |_| Ok(())).unwrap();
    let conn = rusqlite::Connection::open(other.root.ledger_path()).unwrap();
    conn.execute_batch("PRAGMA wal_autocheckpoint = 0;").unwrap();
    for i in 0..20 {
        conn.execute(
            "INSERT INTO events (time, kind, subject, pred, distinct_json, identity, data) VALUES ('t', 'config', 'x', 0, '{}', ?1, '{}')",
            [format!("foreign-{i}")],
        )
        .unwrap();
    }
    let foreign_wal = format!("{}-wal", other.root.ledger_path().display());
    let ours = format!("{}-wal", r.root.ledger_path().display());
    fs::remove_file(r.root.ledger_path()).unwrap();
    fs::copy(&foreign_wal, &ours).unwrap();
    drop(conn);

    r.root.rebuild(t(10), false).unwrap();
    let after = r.root.read(|l| ledger::events_after(l.conn(), 0)).unwrap();
    assert!(
        after.iter().all(|e| !e.identity.starts_with("foreign")),
        "the foreign WAL was replayed"
    );
    assert_eq!(after.len(), in_backup);
}

#[test]
fn two_forced_rebuilds_at_the_same_time_keep_both_old_ledgers() {
    let r = temp_root();
    some_history(&r.root);
    let a = r.root.rebuild(t(10), true).unwrap().moved_aside.unwrap();
    let b = r.root.rebuild(t(10), true).unwrap().moved_aside.unwrap();
    assert_ne!(a, b);
    assert!(a.exists() && b.exists());
}

// --- verifying the whole backup ------------------------------------------------------------

#[test]
fn a_backup_missing_a_middle_line_is_re_exported_from_sqlite() {
    let r = temp_root();
    some_history(&r.root);
    let file = r.root.backup().files().unwrap().pop().unwrap();
    let text = fs::read_to_string(&file).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    lines.remove(1);
    fs::write(&file, lines.join("\n") + "\n").unwrap();
    r.root.mutate(t(10), |_| Ok(())).unwrap();
    assert!(contiguous(&backup_seqs(&r.root)), "{:?}", backup_seqs(&r.root));
    r.root.rebuild(t(11), true).unwrap();
}

#[test]
fn a_middle_line_rewritten_with_a_valid_checksum_is_re_exported_from_sqlite() {
    use issue_dashboard_core::backup::Line;
    let r = temp_root();
    some_history(&r.root);
    let file = r.root.backup().files().unwrap().pop().unwrap();
    let text = fs::read_to_string(&file).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let mut line: Line = serde_json::from_str(&lines[0]).unwrap();
    line.event.time = "2026-10-23T09:59:59.000Z".into();
    lines[0] = Line::of(&line.event).render();
    fs::write(&file, lines.join("\n") + "\n").unwrap();
    r.root.mutate(t(10), |_| Ok(())).unwrap();
    let first: Value = serde_json::from_str(&backup_lines(&r.root)[0]).unwrap();
    assert_ne!(first["event"]["time"], "2026-10-23T09:59:59.000Z");
}

#[test]
fn an_empty_newest_file_does_not_block_repairing_the_previous_files_torn_tail() {
    let r = temp_root();
    some_history(&r.root);
    let file = r.root.backup().files().unwrap().pop().unwrap();
    let mut f = OpenOptions::new().append(true).open(&file).unwrap();
    f.write_all(b"{\"checksum\":\"to").unwrap();
    drop(f);
    fs::write(r.root.backup().dir().join("2026-11.jsonl"), b"").unwrap();
    r.root.mutate(t(10), |_| Ok(())).unwrap();
    assert!(contiguous(&backup_seqs(&r.root)), "{:?}", backup_seqs(&r.root));
}

#[test]
fn several_garbled_trailing_lines_from_one_batch_are_all_cut_and_re_exported() {
    let r = temp_root();
    some_history(&r.root);
    let file = r.root.backup().files().unwrap().pop().unwrap();
    let text = fs::read_to_string(&file).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let n = lines.len();
    lines[n - 1] = "garbage one".into();
    lines[n - 2] = "garbage two".into();
    fs::write(&file, lines.join("\n") + "\n").unwrap();
    r.root.mutate(t(10), |_| Ok(())).unwrap();
    let seqs = backup_seqs(&r.root);
    assert!(contiguous(&seqs), "{seqs:?}");
}

// --- orphans --------------------------------------------------------------------------------

#[test]
fn a_settled_orphan_reported_again_is_a_repeat_and_holds_no_slot() {
    let r = temp_root();
    let cap = |_: &str| -> Result<u32> { Ok(1) };
    r.root
        .mutate(t(0), |tx| {
            launch::request(tx, &launch_doc("L1", "home", &[ISSUE_A], "solo"), Via::Ingest)
        })
        .unwrap();
    let token = match r.root.mutate(t(1), |tx| launch::reserve(tx, "L1", "D1", &cap)).unwrap() {
        Reservation::Reserved { token, .. } => token,
        other => panic!("{other:?}"),
    };
    r.root.mutate(t(20), |tx| launch::release(tx, &token, "D1")).unwrap();
    r.root.mutate(t(21), |tx| launch::started(tx, &token, "S9")).unwrap();
    r.root
        .mutate(t(22), |tx| launch::settle_orphan(tx, &token, "S9"))
        .unwrap();
    // Retries of both, after the settle.
    let again = r
        .root
        .mutate(t(23), |tx| launch::settle_orphan(tx, &token, "S9"))
        .unwrap();
    assert!(again.repeat);
    let report = r.root.mutate(t(24), |tx| launch::started(tx, &token, "S9")).unwrap();
    assert_eq!(
        report,
        StartResult::Orphan {
            launch: "L1".into(),
            repeat: true
        }
    );
    r.root
        .mutate(t(25), |tx| {
            launch::request(tx, &launch_doc("L2", "home", &[ISSUE_B], "solo"), Via::Ingest)
        })
        .unwrap();
    let r2 = r
        .root
        .mutate(t(26), |tx| launch::reserve(tx, "L2", "D1", &cap))
        .unwrap();
    assert!(
        matches!(r2, Reservation::Reserved { .. }),
        "a settled orphan still holds the slot: {r2:?}"
    );
}

#[test]
fn reserve_next_is_not_blocked_by_a_queued_launch_whose_recipe_is_gone() {
    let r = temp_root();
    let cap = |recipe: &str| -> Result<u32> {
        match recipe {
            "solo" => Ok(1),
            other => Err(Error::refused(format!("no launch recipe named {other}"))),
        }
    };
    // While recipe "gone" still existed (one slot): L0 took its slot and L3 queued behind it,
    // before anything queued on "solo".
    let old = |_: &str| -> Result<u32> { Ok(1) };
    r.root
        .mutate(t(0), |tx| {
            launch::request(tx, &launch_doc("L0", "home", &["I_kwDOissueZ"], "gone"), Via::Ingest)
        })
        .unwrap();
    assert!(matches!(
        r.root.mutate(t(0), |tx| launch::reserve(tx, "L0", "D1", &old)).unwrap(),
        Reservation::Reserved { .. }
    ));
    r.root
        .mutate(t(0), |tx| {
            launch::request(tx, &launch_doc("L3", "home", &["I_kwDOissueY"], "gone"), Via::Ingest)
        })
        .unwrap();
    assert!(matches!(
        r.root.mutate(t(0), |tx| launch::reserve(tx, "L3", "D1", &old)).unwrap(),
        Reservation::Queued { .. }
    ));
    // Then "gone" was removed from the config.
    r.root
        .mutate(t(0), |tx| {
            launch::request(tx, &launch_doc("L1", "home", &[ISSUE_A], "solo"), Via::Ingest)
        })
        .unwrap();
    let t1 = match r.root.mutate(t(1), |tx| launch::reserve(tx, "L1", "D1", &cap)).unwrap() {
        Reservation::Reserved { token, .. } => token,
        other => panic!("{other:?}"),
    };
    r.root
        .mutate(t(1), |tx| {
            launch::request(tx, &launch_doc("L2", "home", &[ISSUE_B], "solo"), Via::Ingest)
        })
        .unwrap();
    assert!(matches!(
        r.root.mutate(t(1), |tx| launch::reserve(tx, "L2", "D1", &cap)).unwrap(),
        Reservation::Queued { .. }
    ));
    r.root.mutate(t(2), |tx| launch::started(tx, &t1, "S1")).unwrap();
    r.root
        .mutate(t(3), |tx| launch::ended(tx, &t1, launch::EndState::Landed))
        .unwrap();
    let next = r.root.mutate(t(4), |tx| launch::reserve_next(tx, "D1", &cap)).unwrap();
    assert!(
        matches!(next, Some(Reservation::Reserved { ref launch, .. }) if launch == "L2"),
        "{next:?}"
    );
}

// --- observations ---------------------------------------------------------------------------

#[test]
fn an_unchanged_fresher_read_still_makes_an_older_read_stale() {
    let r = temp_root();
    observe(&r.root, &obs_at("a", "example-org/server#403", t(0), t(0)));
    assert!(matches!(
        observe(&r.root, &obs_at("a", "example-org/server#403", t(0), t(2))),
        Observed::Unchanged(_)
    ));
    // A read taken at t(1), before the unchanged read, commits last.
    let late = obs_at("b", "example-org/server#403", t(0), t(1));
    assert!(
        matches!(observe(&r.root, &late), Observed::Stale { .. }),
        "an older read overwrote a fresher one"
    );
}

#[test]
fn a_rename_that_changes_only_the_alias_is_recorded() {
    let r = temp_root();
    observe(&r.root, &obs_at("a", "example-org/server#403", t(0), t(0)));
    let renamed = obs_at("a", "example-org/server-renamed#403", t(0), t(1));
    assert!(matches!(observe(&r.root, &renamed), Observed::Recorded(_)));
}

#[test]
fn read_times_are_compared_at_full_precision() {
    let r = temp_root();
    let base = t(0);
    let later = base + chrono::Duration::microseconds(900);
    let earlier = base + chrono::Duration::microseconds(500);
    observe(&r.root, &obs_at("a", "example-org/server#403", base, later));
    // Same millisecond, earlier read, different content: stale.
    assert!(matches!(
        observe(&r.root, &obs_at("b", "example-org/server#403", base, earlier)),
        Observed::Stale { .. }
    ));
}

// --- decisions ------------------------------------------------------------------------------

#[test]
fn a_decision_first_ingested_under_the_wrong_project_still_counts_in_its_own() {
    let r = temp_root();
    let doc = set_priority("D1", ISSUE_A, Priority::P1);
    r.root
        .mutate(t(0), |tx| decision::ingest(tx, "work", OWNER, &doc))
        .unwrap();
    r.root
        .mutate(t(1), |tx| decision::ingest(tx, "home", OWNER, &doc))
        .unwrap();
    let p = r.root.read(|l| decision::project_priorities(l.conn(), "home")).unwrap();
    assert_eq!(p.get(ISSUE_A), Some(&Priority::P1));
}

#[test]
fn a_decision_refused_under_a_wrong_owner_id_counts_once_the_config_is_fixed() {
    let r = temp_root();
    let doc = set_priority("D1", ISSUE_A, Priority::P1);
    r.root
        .mutate(t(0), |tx| decision::ingest(tx, "home", "user_typo", &doc))
        .unwrap();
    r.root
        .mutate(t(1), |tx| decision::ingest(tx, "home", OWNER, &doc))
        .unwrap();
    let s = r.root.read(|l| decision::statuses(l.conn(), "home")).unwrap();
    assert_eq!(s["D1"], Status::Applied);
    // Once accepted, it stays accepted, whatever a later ingest's owner_id says.
    r.root
        .mutate(t(2), |tx| decision::ingest(tx, "home", "user_other", &doc))
        .unwrap();
    let s = r.root.read(|l| decision::statuses(l.conn(), "home")).unwrap();
    assert_eq!(s["D1"], Status::Applied);
}

#[test]
fn a_changed_document_under_an_ingested_decision_id_is_refused() {
    let r = temp_root();
    r.root
        .mutate(t(0), |tx| {
            decision::ingest(tx, "home", OWNER, &set_priority("D1", ISSUE_A, Priority::P1))
        })
        .unwrap();
    let err = r
        .root
        .mutate(t(1), |tx| {
            decision::ingest(tx, "home", OWNER, &set_priority("D1", ISSUE_A, Priority::P0))
        })
        .unwrap_err();
    assert!(matches!(err, Error::Refused(_)), "{err}");
}

// --- schema validation of inputs ------------------------------------------------------------

#[test]
fn a_config_the_schema_refuses_is_refused_on_load_naming_the_file() {
    let r = temp_root();
    fs::create_dir_all(r.path().join("projects")).unwrap();
    fs::write(
        r.path().join("repos.toml"),
        "[repo.\"a/b\"]\n[launch.lanes]\ncapacity = 0\nworkspace = \"w\"\nrelease = \"r\"\nland = \"l\"\ngate.quick = { run = \"t\", minutes = 1 }\n",
    )
    .unwrap();
    match ConfigSet::load(r.path()) {
        Err(Error::Config { file, reason }) => {
            assert!(file.ends_with("repos.toml"));
            assert!(reason.contains("capacity") || reason.contains("minimum"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    fs::write(r.path().join("repos.toml"), "[repo.\"a/b\"]\n").unwrap();
    fs::write(
        r.path().join("projects/home.toml"),
        "schema = 1\nname = \"H\"\nowner_id = \"u\"\n[[member]]\nrepo = \"a/b\"\nexclude = [\"a/c\"]\n",
    )
    .unwrap();
    assert!(matches!(ConfigSet::load(r.path()), Err(Error::Config { .. })));
}

#[test]
fn a_launch_document_the_schema_refuses_is_refused() {
    let r = temp_root();
    let mut doc = launch_doc("L/1", "home", &[ISSUE_A], "lanes");
    let err = r
        .root
        .mutate(t(0), |tx| launch::request(tx, &doc, Via::Reserve))
        .unwrap_err();
    assert!(matches!(err, Error::Refused(_)), "{err}");
    doc.id = "L1".into();
    doc.created_at = "yesterday".into();
    assert!(
        r.root
            .mutate(t(0), |tx| launch::request(tx, &doc, Via::Reserve))
            .is_err()
    );
}

#[test]
fn values_the_schemas_refuse_are_refused_before_they_are_recorded() {
    let r = temp_root();
    r.root
        .mutate(t(0), |tx| {
            launch::request(tx, &launch_doc("L1", "home", &[ISSUE_A], "lanes"), Via::Ingest)
        })
        .unwrap();
    let token = match r
        .root
        .mutate(t(1), |tx| launch::reserve(tx, "L1", "D1", &|_| Ok(2)))
        .unwrap()
    {
        Reservation::Reserved { token, .. } => token,
        other => panic!("{other:?}"),
    };
    assert!(
        r.root.mutate(t(2), |tx| launch::started(tx, &token, "")).is_err(),
        "an empty session id"
    );
    let bad = Outcome {
        launch: "L1".into(),
        landed: true,
        commit: Some("not-a-sha".into()),
        pull_request: None,
        wall_clock_minutes: None,
        gate_runs: None,
        findings_fixed: None,
        context_tokens: None,
        recorded_by: RecordedBy::Session,
        missing: vec![],
    };
    assert!(r.root.mutate(t(3), |tx| outcome::record(tx, &bad)).is_err());
    let none = r
        .root
        .read(|l| ledger::events_by_kind(l.conn(), Kind::Outcome))
        .unwrap();
    assert!(none.is_empty());
}

// --- the clock is read after the lock -------------------------------------------------------

#[test]
fn the_command_time_is_taken_after_the_lock_wait() {
    let r = temp_root();
    let held = RootLock::acquire(&r.root.lock_path(), "holder", Duration::from_secs(1)).unwrap();
    let read_early = Arc::new(AtomicBool::new(false));
    let released = Arc::new(AtomicBool::new(false));
    let root = Root::new(r.path(), Options::default());
    let (re, rel) = (read_early.clone(), released.clone());
    let waiter = thread::spawn(move || {
        root.mutate_with_clock(
            move || {
                if !rel.load(Ordering::SeqCst) {
                    re.store(true, Ordering::SeqCst);
                }
                t(0)
            },
            |_| Ok(()),
        )
        .unwrap();
    });
    thread::sleep(Duration::from_millis(200));
    released.store(true, Ordering::SeqCst);
    drop(held);
    waiter.join().unwrap();
    assert!(
        !read_early.load(Ordering::SeqCst),
        "the clock was read while the lock was still held by another"
    );
}

#[test]
fn a_ledger_with_an_unknown_schema_version_is_refused() {
    let r = temp_root();
    some_history(&r.root);
    let c = rusqlite::Connection::open(r.root.ledger_path()).unwrap();
    c.execute_batch("PRAGMA user_version = 99;").unwrap();
    drop(c);
    let err = r.root.mutate(t(10), |_| Ok(())).unwrap_err();
    assert!(err.to_string().contains("99"), "{err}");
}
