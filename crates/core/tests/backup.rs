//! The JSON Lines backup: export before the lock is released, torn-tail repair, no duplicate
//! appends after a crash, the export cursor, and rebuild (10.2).

mod common;

use std::fs::{self, OpenOptions};
use std::io::Write;

use common::{assert_valid, backup_lines, backup_seqs, t, temp_root};
use issue_dashboard_core::backup::{Failpoint, export_cursor};
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::ledger::{self, Dedupe};
use issue_dashboard_core::observe::{self, ItemKind, Observation};
use issue_dashboard_core::root::Options;
use issue_dashboard_core::{Error, Root};
use serde_json::{Value, json};

/// Records an observation of one issue whose only label is `label`: a real event shape, so
/// the backup lines can be checked against the schemas.
fn record(root: &Root, minutes: i64, label: &str) {
    let obs = Observation {
        node_id: "I_kwDOissueA".into(),
        item: ItemKind::Issue,
        alias: "example-org/server#403".into(),
        updated_at: t(minutes),
        read_at: t(minutes),
        read_id: format!("read-{minutes}"),
        content: json!({
            "state": "open",
            "labels": [label],
            "assignees": [],
            "links": {"parent": null, "sub_issues": [], "blocked_by": [], "blocking": []},
            "timeline_ids": [],
        }),
    };
    root.mutate(t(minutes), |tx| observe::record(tx, &obs)).unwrap();
}

fn sqlite_seqs(root: &Root) -> Vec<i64> {
    root.read(|l| ledger::events_after(l.conn(), 0)).unwrap().iter().map(|e| e.seq).collect()
}

fn with_failpoint(dir: &std::path::Path, fp: Failpoint) -> Root {
    Root::new(dir, Options { failpoint: Some(fp), ..Options::default() })
}

#[test]
fn every_committed_event_is_in_the_backup_when_the_command_returns() {
    let r = temp_root();
    record(&r.root, 0, "A");
    record(&r.root, 1, "B");
    // Every event SQLite holds, except possibly the last `exported` marker, is in the backup.
    let in_sqlite = r.root.read(|l| ledger::events_after(l.conn(), 0)).unwrap();
    let in_backup = backup_seqs(&r.root);
    for e in &in_sqlite {
        if e.kind != Kind::Exported {
            assert!(in_backup.contains(&e.seq), "event {} ({:?}) is not in the backup", e.seq, e.kind);
        }
    }
    let expected: Vec<i64> = (1..=in_backup.len() as i64).collect();
    assert_eq!(in_backup, expected, "backup sequence numbers are not contiguous from 1");
}

#[test]
fn every_backup_line_validates_against_the_ledger_line_schema() {
    let r = temp_root();
    record(&r.root, 0, "A");
    record(&r.root, 1, "B");
    let lines = backup_lines(&r.root);
    assert!(lines.len() >= 3);
    for line in &lines {
        assert_valid("ledger-line", &serde_json::from_str::<Value>(line).unwrap());
    }
}

#[test]
fn the_export_cursor_advances_and_settles_without_churning() {
    let r = temp_root();
    record(&r.root, 0, "A");
    let cursor1 = r.root.read(|l| export_cursor(l.conn())).unwrap();
    assert_eq!(cursor1, 1, "the cursor names the last exported event");
    // A command that records nothing must not add events forever.
    let before = sqlite_seqs(&r.root).len();
    r.root.mutate(t(1), |_| Ok(())).unwrap();
    r.root.mutate(t(2), |_| Ok(())).unwrap();
    let after = sqlite_seqs(&r.root).len();
    assert_eq!(before, after, "an empty command recorded events");
    // ...and the `exported` marker itself still reaches the backup.
    assert_eq!(backup_seqs(&r.root), sqlite_seqs(&r.root));
}

#[test]
fn backup_files_are_named_by_month() {
    let r = temp_root();
    record(&r.root, 0, "A");
    let names: Vec<String> = r
        .root
        .backup()
        .files()
        .unwrap()
        .iter()
        .map(|p| p.file_name().unwrap().to_string_lossy().into_owned())
        .collect();
    assert_eq!(names, ["2026-10.jsonl"]);
}

#[test]
fn a_month_change_starts_a_new_file_and_an_earlier_clock_never_appends_to_an_older_one() {
    let r = temp_root();
    record(&r.root, 0, "A");
    let november = 60 * 24 * 10; // 2026-11-02
    record(&r.root, november, "B");
    // The clock steps back into October: the event still goes to the latest file, so the
    // backup stays in sequence order across files.
    record(&r.root, 5, "C");
    let names: Vec<String> = r.root.backup().files().unwrap().iter().map(|p| p.file_name().unwrap().to_string_lossy().into_owned()).collect();
    assert_eq!(names, ["2026-10.jsonl", "2026-11.jsonl"]);
    let seqs = backup_seqs(&r.root);
    let mut sorted = seqs.clone();
    sorted.sort();
    assert_eq!(seqs, sorted);
}

#[test]
fn crash_after_commit_before_export_is_exported_by_the_next_command() {
    let r = temp_root();
    record(&r.root, 0, "A");
    let crashing = with_failpoint(r.path(), Failpoint::AfterCommit);
    let err = crashing
        .mutate(t(1), |tx| tx.record(Kind::Config, "config", json!({"content_hash": "B"}), json!({}), Dedupe::Latest))
        .unwrap_err();
    assert!(matches!(err, Error::Failpoint("after-commit")), "{err}");
    let b_seq = *sqlite_seqs(&r.root).last().unwrap();
    assert!(!backup_seqs(&r.root).contains(&b_seq), "B reached the backup although the export never ran");

    record(&r.root, 2, "C");
    let seqs = backup_seqs(&r.root);
    assert!(seqs.contains(&b_seq), "B was never exported");
    let expected: Vec<i64> = (1..=seqs.len() as i64).collect();
    assert_eq!(seqs, expected, "duplicate or missing lines");
}

#[test]
fn crash_between_append_and_cursor_update_neither_loses_nor_duplicates() {
    let r = temp_root();
    record(&r.root, 0, "A");
    let crashing = with_failpoint(r.path(), Failpoint::AfterAppend);
    let err = crashing
        .mutate(t(1), |tx| tx.record(Kind::Config, "config", json!({"content_hash": "B"}), json!({}), Dedupe::Latest))
        .unwrap_err();
    assert!(matches!(err, Error::Failpoint("after-append")), "{err}");
    let after_crash = backup_seqs(&r.root);
    let cursor_after_crash = r.root.read(|l| export_cursor(l.conn())).unwrap();
    assert!(
        cursor_after_crash < *after_crash.last().unwrap(),
        "the cursor advanced although the crash came before it: the scenario did not happen"
    );

    // The next command: no line is appended twice, and the cursor catches up.
    record(&r.root, 2, "C");
    let seqs = backup_seqs(&r.root);
    let expected: Vec<i64> = (1..=seqs.len() as i64).collect();
    assert_eq!(seqs, expected, "a line was appended twice or lost");
    let cursor = r.root.read(|l| export_cursor(l.conn())).unwrap();
    assert!(cursor >= after_crash.len() as i64);
}

#[test]
fn a_torn_final_fragment_is_cut_and_the_event_re_exported() {
    let r = temp_root();
    record(&r.root, 0, "A");
    record(&r.root, 1, "B");
    let file = r.root.backup().files().unwrap().pop().unwrap();
    let text = fs::read_to_string(&file).unwrap();
    let last = text.trim_end().rsplit('\n').next().unwrap().to_string();
    // Tear the last line in half: the crash came mid-write.
    let torn = &text[..text.len() - last.len() / 2 - 1];
    fs::write(&file, torn).unwrap();

    let report = r.root.backup().repair_tail().unwrap();
    assert_eq!(report.cut_lines, 1);
    record(&r.root, 2, "C");
    let seqs = backup_seqs(&r.root);
    let expected: Vec<i64> = (1..=seqs.len() as i64).collect();
    assert_eq!(seqs, expected);
    for line in backup_lines(&r.root) {
        assert_valid("ledger-line", &serde_json::from_str::<Value>(&line).unwrap());
    }
}

#[test]
fn a_complete_final_line_with_a_bad_checksum_is_cut_and_re_exported() {
    let r = temp_root();
    record(&r.root, 0, "A");
    let file = r.root.backup().files().unwrap().pop().unwrap();
    let text = fs::read_to_string(&file).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let n = lines.len();
    let mut v: Value = serde_json::from_str(&lines[n - 1]).unwrap();
    v["event"]["data"] = json!({"tampered": true});
    lines[n - 1] = serde_json::to_string(&v).unwrap();
    fs::write(&file, lines.join("\n") + "\n").unwrap();

    record(&r.root, 1, "B");
    for line in backup_lines(&r.root) {
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_ne!(v["event"]["data"], json!({"tampered": true}), "the bad line survived");
    }
    let seqs = backup_seqs(&r.root);
    let expected: Vec<i64> = (1..=seqs.len() as i64).collect();
    assert_eq!(seqs, expected);
}

#[test]
fn rebuild_recreates_the_ledger_exactly() {
    let r = temp_root();
    record(&r.root, 0, "A");
    record(&r.root, 1, "B");
    record(&r.root, 2, "A");
    r.root.mutate(t(3), |_| Ok(())).unwrap(); // export the last `exported` marker
    let before = r.root.read(|l| ledger::events_after(l.conn(), 0)).unwrap();

    let report = r.root.rebuild(t(4), true).unwrap();
    assert!(report.moved_aside.is_some());
    assert_eq!(report.events, before.len());
    let after = r.root.read(|l| ledger::events_after(l.conn(), 0)).unwrap();
    assert_eq!(before, after);

    // The rebuilt ledger keeps working: a repeat of the latest is still a no-op...
    record(&r.root, 5, "A");
    assert_eq!(r.root.read(|l| ledger::events_after(l.conn(), 0)).unwrap().len(), before.len());
    // ...and a change is recorded after it.
    record(&r.root, 6, "B");
    assert!(r.root.read(|l| ledger::events_after(l.conn(), 0)).unwrap().len() > before.len());
}

#[test]
fn rebuild_refuses_to_replace_an_existing_ledger_without_force() {
    let r = temp_root();
    record(&r.root, 0, "A");
    let err = r.root.rebuild(t(1), false).unwrap_err();
    assert!(matches!(err, Error::Refused(_)), "{err}");
}

#[test]
fn rebuild_refuses_on_a_corrupt_line_in_the_middle_naming_file_and_line() {
    let r = temp_root();
    record(&r.root, 0, "A");
    record(&r.root, 1, "B");
    record(&r.root, 2, "C");
    let file = r.root.backup().files().unwrap().pop().unwrap();
    let text = fs::read_to_string(&file).unwrap();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    assert!(lines.len() >= 4, "the corrupt line must not be the final one");
    // A changed value in a complete line, its checksum left as it was.
    let mut v: Value = serde_json::from_str(&lines[1]).unwrap();
    v["event"]["data"]["cursor"] = json!(99);
    lines[1] = serde_json::to_string(&v).unwrap();
    fs::write(&file, lines.join("\n") + "\n").unwrap();
    fs::remove_file(r.root.ledger_path()).unwrap();

    let err = r.root.rebuild(t(3), false).unwrap_err();
    match err {
        Error::CorruptBackup { file: f, line, .. } => {
            assert_eq!(f, file);
            assert_eq!(line, 2);
        }
        other => panic!("expected a corrupt-backup refusal, got {other}"),
    }
    assert!(!r.root.ledger_path().exists(), "a refused rebuild left a ledger behind");
}

#[test]
fn rebuild_refuses_a_gap_in_sequence_numbers() {
    let r = temp_root();
    record(&r.root, 0, "A");
    record(&r.root, 1, "B");
    record(&r.root, 2, "C");
    let file = r.root.backup().files().unwrap().pop().unwrap();
    let text = fs::read_to_string(&file).unwrap();
    let mut lines: Vec<&str> = text.lines().collect();
    lines.remove(1); // a whole valid line goes missing
    fs::write(&file, lines.join("\n") + "\n").unwrap();
    fs::remove_file(r.root.ledger_path()).unwrap();

    let err = r.root.rebuild(t(3), false).unwrap_err();
    assert!(matches!(err, Error::CorruptBackup { line: 2, .. }), "{err}");
}

#[test]
fn rebuild_tolerates_a_torn_final_line_and_reports_it() {
    let r = temp_root();
    record(&r.root, 0, "A");
    record(&r.root, 1, "B");
    let file = r.root.backup().files().unwrap().pop().unwrap();
    let mut f = OpenOptions::new().append(true).open(&file).unwrap();
    f.write_all(b"{\"checksum\":\"00").unwrap();
    drop(f);
    let count = backup_lines(&r.root).len() - 1;
    fs::remove_file(r.root.ledger_path()).unwrap();

    let report = r.root.rebuild(t(2), false).unwrap();
    assert_eq!(report.dropped_tail_lines, 1);
    assert_eq!(report.events, count);
}

#[test]
fn a_backup_that_disagrees_with_sqlite_is_moved_aside_and_re_exported() {
    let r = temp_root();
    record(&r.root, 0, "A");
    record(&r.root, 1, "B");
    // Replace the backup with one from a different history (same length, other content).
    let other = temp_root();
    record(&other.root, 0, "X");
    record(&other.root, 1, "Y");
    let file = r.root.backup().files().unwrap().pop().unwrap();
    let other_file = other.root.backup().files().unwrap().pop().unwrap();
    fs::copy(&other_file, &file).unwrap();

    r.root.mutate(t(2), |_| Ok(())).unwrap();
    let moved: Vec<_> = fs::read_dir(r.root.backup().dir())
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with("diverged-"))
        .collect();
    assert_eq!(moved.len(), 1, "the diverged backup was not kept aside");
    // The live backup now matches SQLite.
    let sqlite = r.root.read(|l| ledger::events_after(l.conn(), 0)).unwrap();
    for line in backup_lines(&r.root) {
        let v: Value = serde_json::from_str(&line).unwrap();
        let seq = v["event"]["seq"].as_i64().unwrap();
        let e = sqlite.iter().find(|e| e.seq == seq).unwrap();
        assert_eq!(v["event"]["identity"], json!(e.identity));
    }
}
