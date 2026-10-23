//! Regression tests for the second gate round (the re-gate of round 1's fixes).

mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;

use common::{ISSUE_A, ISSUE_B, OWNER, backup_seqs, launch_doc, set_priority, t, temp_root};
use issue_dashboard_core::decision;
use issue_dashboard_core::docs::Priority;
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::launch::{self, Reservation, State, Via};
use issue_dashboard_core::ledger::{self, Dedupe};
use issue_dashboard_core::observe::{self, ItemKind, Observation};
use issue_dashboard_core::{Error, Result, Root};
use serde_json::json;

fn observe_label(root: &Root, label: &str, minutes: i64) {
    let o = Observation {
        node_id: ISSUE_A.into(),
        item: ItemKind::Issue,
        alias: "example-org/server#403".into(),
        updated_at: t(minutes),
        read_at: t(minutes),
        read_id: format!("read-{minutes}"),
        content: json!({
            "state": "open", "labels": [label], "assignees": [],
            "links": {"parent": null, "sub_issues": [], "blocked_by": [], "blocking": []},
            "timeline_ids": [],
        }),
    };
    root.mutate(t(minutes), |tx| observe::record(tx, &o)).unwrap();
}

fn history(root: &Root) {
    for (i, l) in ["a", "b", "c", "d"].iter().enumerate() {
        observe_label(root, l, i as i64);
    }
}

fn garble_last_two_lines(root: &Root) {
    let file = root.backup().files().unwrap().pop().unwrap();
    let text = fs::read_to_string(&file).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let n = lines.len();
    lines[n - 1] = "garbage one".into();
    lines[n - 2] = "{\"garbage\": two}".into();
    fs::write(&file, lines.join("\n") + "\n").unwrap();
}

#[test]
fn a_validation_failure_inside_reserve_next_leaves_no_reservation_behind() {
    let r = temp_root();
    // A queued launch whose id the schema now refuses (recorded unchecked, as an older build
    // could have), queued before a valid one on another recipe.
    let mut bad = launch_doc("L1", "home", &["I_kwDOissueQ"], "other");
    bad.id = "bad.id".into();
    r.root
        .mutate(t(0), |tx| {
            tx.record(
                Kind::Launch,
                "bad.id",
                json!({"transition": "requested", "token": null}),
                json!({"transition": "requested", "request": bad, "project": "home", "via": "ingest"}),
                Dedupe::IdentityOnly,
            )?;
            tx.record(
                Kind::Launch,
                "bad.id",
                json!({"transition": "queued", "token": null}),
                json!({"transition": "queued", "dispatcher": "D1"}),
                Dedupe::IdentityOnly,
            )
        })
        .unwrap();
    let cap = |_: &str| -> Result<u32> { Ok(1) };
    r.root
        .mutate(t(1), |tx| {
            launch::request(tx, &launch_doc("L2", "home", &[ISSUE_B], "solo"), Via::Ingest)
        })
        .unwrap();
    // L2 has to be queued: fill "solo" first.
    r.root
        .mutate(t(1), |tx| {
            launch::request(tx, &launch_doc("L0", "home", &[ISSUE_A], "solo"), Via::Ingest)
        })
        .unwrap();
    let t0 = match r.root.mutate(t(1), |tx| launch::reserve(tx, "L0", "D1", &cap)).unwrap() {
        Reservation::Reserved { token, .. } => token,
        other => panic!("{other:?}"),
    };
    assert!(matches!(
        r.root.mutate(t(1), |tx| launch::reserve(tx, "L2", "D1", &cap)).unwrap(),
        Reservation::Queued { .. }
    ));
    r.root.mutate(t(2), |tx| launch::started(tx, &t0, "S0")).unwrap();
    r.root
        .mutate(t(3), |tx| launch::ended(tx, &t0, launch::EndState::Landed))
        .unwrap();

    let next = r.root.mutate(t(4), |tx| launch::reserve_next(tx, "D1", &cap));
    let bad_state = r
        .root
        .read(|l| launch::load(l.conn(), "bad.id"))
        .unwrap()
        .unwrap()
        .state;
    assert_eq!(bad_state, State::Queued, "a refused recording was committed: {next:?}");
}

#[test]
fn a_lost_ledger_with_several_garbled_trailing_lines_points_to_rebuild_and_rebuild_succeeds() {
    let r = temp_root();
    history(&r.root);
    let good = backup_seqs(&r.root).len() - 2;
    garble_last_two_lines(&r.root);
    fs::remove_file(r.root.ledger_path()).unwrap();
    let err = r.root.mutate(t(10), |_| Ok(())).unwrap_err();
    assert!(matches!(&err, Error::Refused(m) if m.contains("rebuild")), "{err}");
    let report = r.root.rebuild(t(11), false).unwrap();
    assert_eq!(report.dropped_tail_lines, 2);
    assert_eq!(report.events, good);
}

#[test]
fn rebuild_tolerates_a_torn_tail_in_the_file_before_an_empty_newest_one() {
    let r = temp_root();
    history(&r.root);
    let file = r.root.backup().files().unwrap().pop().unwrap();
    let mut f = OpenOptions::new().append(true).open(&file).unwrap();
    f.write_all(b"{\"checksum\":\"to").unwrap();
    drop(f);
    fs::write(r.root.backup().dir().join("2026-11.jsonl"), b"").unwrap();
    fs::remove_file(r.root.ledger_path()).unwrap();
    let report = r.root.rebuild(t(10), false).unwrap();
    assert_eq!(report.dropped_tail_lines, 1);
}

#[test]
fn rebuild_still_refuses_corruption_followed_by_good_lines() {
    let r = temp_root();
    history(&r.root);
    let file = r.root.backup().files().unwrap().pop().unwrap();
    let text = fs::read_to_string(&file).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    lines[2] = "garbage".into();
    fs::write(&file, lines.join("\n") + "\n").unwrap();
    fs::remove_file(r.root.ledger_path()).unwrap();
    assert!(matches!(
        r.root.rebuild(t(10), false),
        Err(Error::CorruptBackup { line: 3, .. })
    ));
}

#[cfg(unix)]
#[test]
fn an_unreadable_backup_file_is_an_error_not_a_divergence() {
    use std::os::unix::fs::PermissionsExt;
    let r = temp_root();
    history(&r.root);
    // An older month file the command cannot read (a permissions problem, a failing disk).
    let older = r.root.backup().dir().join("2026-09.jsonl");
    fs::write(&older, b"").unwrap();
    fs::set_permissions(&older, fs::Permissions::from_mode(0o000)).unwrap();
    let result = r.root.mutate(t(10), |_| Ok(()));
    let _ = fs::set_permissions(&older, fs::Permissions::from_mode(0o644));
    assert!(matches!(result, Err(Error::Io(_))), "{result:?}");
    let moved = fs::read_dir(r.root.backup().dir())
        .unwrap()
        .filter(|e| e.as_ref().unwrap().path().is_dir())
        .count();
    assert_eq!(moved, 0, "a read error moved the backup aside");
}

// --- round 3 ----------------------------------------------------------------------------------

#[test]
fn a_forced_rebuild_refuses_while_a_reader_keeps_the_wal_from_being_checkpointed() {
    let r = temp_root();
    history(&r.root);
    // A reader holds an old snapshot; later commits stay in the WAL past its mark.
    let reader = rusqlite::Connection::open(r.root.ledger_path()).unwrap();
    reader.execute_batch("BEGIN; SELECT COUNT(*) FROM events;").unwrap();
    let _: i64 = reader
        .query_row("SELECT COUNT(*) FROM events", [], |row| row.get(0))
        .unwrap();
    observe_label(&r.root, "e", 10);
    let before = r.root.read(|l| ledger::max_seq(l.conn())).unwrap();
    let result = r.root.rebuild(t(11), true);
    assert!(
        matches!(&result, Err(Error::Refused(m)) if m.contains("in use")),
        "{result:?}"
    );
    drop(reader);
    assert_eq!(r.root.read(|l| ledger::max_seq(l.conn())).unwrap(), before);
}

#[cfg(unix)]
#[test]
fn a_missing_ledger_beside_an_unreadable_backup_is_an_io_error_not_a_rebuild_hint() {
    use std::os::unix::fs::PermissionsExt;
    let r = temp_root();
    history(&r.root);
    fs::remove_file(r.root.ledger_path()).unwrap();
    let file = r.root.backup().files().unwrap().pop().unwrap();
    fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
    let result = r.root.mutate(t(10), |_| Ok(()));
    let _ = fs::set_permissions(&file, fs::Permissions::from_mode(0o644));
    assert!(matches!(result, Err(Error::Io(_))), "{result:?}");
}

#[test]
fn an_identical_refusal_is_a_repeat_whatever_another_project_ingested_in_between() {
    let r = temp_root();
    let doc = set_priority("D1", ISSUE_A, Priority::P1);
    let ingest = |project: &str| {
        r.root
            .mutate(t(0), |tx| decision::ingest(tx, project, OWNER, &doc))
            .unwrap()
            .is_new()
    };
    assert!(ingest("other"));
    assert!(ingest("third"));
    assert!(!ingest("other"), "an identical refusal was recorded again");
    let n = r
        .root
        .read(|l| ledger::events_by_kind(l.conn(), Kind::Decided))
        .unwrap()
        .len();
    assert_eq!(n, 2);
}
