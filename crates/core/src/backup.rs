//! The JSON Lines backup of the ledger (10.2).
//!
//! `<root>/ledger/YYYY-MM.jsonl`, one event per line with its sequence number and the
//! sha256 of the event's canonical JSON. SQLite is the authority: the backup is brought up to
//! date from it by every mutating command before it releases the root lock, and `rebuild`
//! recreates SQLite from the backup.
//!
//! Lines are only ever appended to the latest file, so only its trailing lines (one export's
//! batch, under one fsync) can be torn by a crash. An event goes to the file of its month, or to
//! the latest file when the clock has stepped back, so the files read in name order are always
//! in sequence order.

use std::collections::HashMap;
use std::fs::{self, File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::canonical::{canonical_json, sha256_hex};
use crate::event::{Event, Kind};
use crate::ledger::{self, Dedupe, Ledger};
use crate::{Error, Result};

/// One backup line: the event and the sha256 of its canonical JSON.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Line {
    pub checksum: String,
    pub event: Event,
}

impl Line {
    pub fn of(event: &Event) -> Line {
        Line {
            checksum: checksum(event),
            event: event.clone(),
        }
    }

    /// The line's text, without the trailing newline.
    pub fn render(&self) -> String {
        canonical_json(&serde_json::to_value(self).expect("a line serialises"))
    }
}

/// sha256 of the event's canonical JSON.
pub fn checksum(event: &Event) -> String {
    sha256_hex(&canonical_json(
        &serde_json::to_value(event).expect("an event serialises"),
    ))
}

/// Parses and verifies one line: its checksum, and the event's identity.
fn parse_line(bytes: &[u8]) -> std::result::Result<Event, String> {
    let line: Line = serde_json::from_slice(bytes).map_err(|e| format!("does not parse: {e}"))?;
    if line.checksum != checksum(&line.event) {
        return Err("checksum does not match the event".into());
    }
    if line.event.identity != line.event.computed_identity() {
        return Err("identity does not match the event's fields".into());
    }
    Ok(line.event)
}

/// Points in an export where a test can simulate a crash.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Failpoint {
    /// The command's events are committed to SQLite, nothing is exported yet.
    AfterCommit,
    /// Events are appended to the backup and fsynced; the export cursor is not advanced.
    AfterAppend,
}

impl Failpoint {
    pub fn parse(s: &str) -> Option<Failpoint> {
        match s {
            "after-commit" => Some(Failpoint::AfterCommit),
            "after-append" => Some(Failpoint::AfterAppend),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Failpoint::AfterCommit => "after-commit",
            Failpoint::AfterAppend => "after-append",
        }
    }
}

/// What [`Backup::repair_tail`] did to the latest file.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TailRepair {
    /// Bytes cut from the end: an unterminated fragment and/or a final line that failed to
    /// parse or verify.
    pub cut_bytes: u64,
    pub cut_lines: usize,
}

/// What a catch-up export did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExportReport {
    pub repair: TailRepair,
    /// Events appended to the backup by this export.
    pub appended: usize,
    /// The backup did not match SQLite; its files were moved to this directory and the
    /// backup re-exported from SQLite (10.2: SQLite is the authority).
    pub diverged: Option<PathBuf>,
    /// The export cursor recorded by this export, if it recorded one.
    pub cursor: Option<i64>,
}

/// The backup directory of a tracker root.
#[derive(Debug, Clone)]
pub struct Backup {
    dir: PathBuf,
}

fn is_month_file(name: &str) -> bool {
    let b = name.as_bytes();
    name.len() == 13
        && name.ends_with(".jsonl")
        && b[..4].iter().all(u8::is_ascii_digit)
        && b[4] == b'-'
        && b[5..7].iter().all(u8::is_ascii_digit)
}

/// "2026-10" from an RFC 3339 time; refuses a time no month file can be named after (a year
/// past 9999, or text that is not a time).
fn month_of(event: &Event) -> Result<&str> {
    let m = event.time.get(..7).unwrap_or_default();
    let b = m.as_bytes();
    if b.len() == 7 && b[..4].iter().all(u8::is_ascii_digit) && b[4] == b'-' && b[5..].iter().all(u8::is_ascii_digit) {
        Ok(m)
    } else {
        Err(Error::refused(format!(
            "event {} has a time the backup cannot file under a month: {}",
            event.seq, event.time
        )))
    }
}

/// Splits a file into its lines, each without its newline; the flag says whether the last
/// one was terminated.
fn split_lines(bytes: &[u8]) -> (Vec<&[u8]>, bool) {
    if bytes.is_empty() {
        return (Vec::new(), true);
    }
    let terminated = bytes.ends_with(b"\n");
    let body = if terminated { &bytes[..bytes.len() - 1] } else { bytes };
    (body.split(|b| *b == b'\n').collect(), terminated)
}

fn sync_dir(dir: &Path) -> Result<()> {
    File::open(dir)?.sync_all()?;
    Ok(())
}

impl Backup {
    pub fn new(dir: impl Into<PathBuf>) -> Backup {
        Backup { dir: dir.into() }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The month files, oldest first.
    pub fn files(&self) -> Result<Vec<PathBuf>> {
        if !self.dir.exists() {
            return Ok(Vec::new());
        }
        let mut out = Vec::new();
        for entry in fs::read_dir(&self.dir)? {
            let entry = entry?;
            let name = entry.file_name().to_string_lossy().into_owned();
            if entry.file_type()?.is_file() && is_month_file(&name) {
                out.push(entry.path());
            }
        }
        out.sort();
        Ok(out)
    }

    /// Cuts a torn tail off the latest non-empty file (10.2). One export writes several lines
    /// under one fsync, so a crash can garble more than the last: every trailing line that does
    /// not parse or verify is cut, back to the last good one; SQLite holds their events and the
    /// next export appends them again. An unterminated line that verifies is kept and
    /// terminated. Corruption before a good line is not a torn tail: [`Backup::catch_up`]'s
    /// full verification finds it.
    pub fn repair_tail(&self) -> Result<TailRepair> {
        let mut path = None;
        for p in self.files()?.into_iter().rev() {
            if fs::metadata(&p)?.len() > 0 {
                path = Some(p);
                break;
            }
        }
        let Some(path) = path else {
            return Ok(TailRepair::default());
        };
        let bytes = fs::read(&path)?;
        let (lines, terminated) = split_lines(&bytes);
        let mut keep = bytes.len();
        let mut report = TailRepair::default();
        let mut remaining = lines.len();

        if !terminated {
            let last = lines[lines.len() - 1];
            if parse_line(last).is_ok() {
                let mut f = OpenOptions::new().append(true).open(&path)?;
                f.write_all(b"\n")?;
                f.sync_all()?;
                return Ok(report);
            }
            keep -= last.len();
            report.cut_lines += 1;
            remaining -= 1;
        }
        while remaining > 0 && parse_line(lines[remaining - 1]).is_err() {
            keep -= lines[remaining - 1].len() + 1;
            report.cut_lines += 1;
            remaining -= 1;
        }
        if keep < bytes.len() {
            let f = OpenOptions::new().write(true).open(&path)?;
            f.set_len(keep as u64)?;
            f.sync_all()?;
            report.cut_bytes = (bytes.len() - keep) as u64;
        }
        Ok(report)
    }

    /// The last event in the backup, after [`Backup::repair_tail`].
    pub fn last_event(&self) -> Result<Option<Event>> {
        for path in self.files()?.iter().rev() {
            let bytes = fs::read(path)?;
            let (lines, _) = split_lines(&bytes);
            if let Some(last) = lines.last() {
                return parse_line(last).map(Some).map_err(|reason| Error::CorruptBackup {
                    file: path.clone(),
                    line: lines.len(),
                    reason,
                });
            }
        }
        Ok(None)
    }

    /// Brings the backup up to date with SQLite: repairs the tail, verifies the whole backup
    /// against SQLite, appends every event past the backup's last one (fsynced), then records the
    /// export cursor. Never appends an event whose sequence number the backup already holds.
    ///
    /// - A backup AHEAD of SQLite means the ledger is missing, replaced or older than its
    ///   backup: refused, naming `rebuild`, since re-exporting would discard the newer history.
    /// - A backup that differs from SQLite anywhere (a lost, changed or corrupt line) is moved to
    ///   `diverged-<time>/` and re-exported from SQLite, the authority (10.2).
    ///
    /// The verification reads the whole backup and the whole ledger: its cost grows with the
    /// history (10.10 records this as open).
    pub fn catch_up(
        &self,
        ledger: &mut Ledger,
        now: &str,
        failpoint: &mut dyn FnMut(Failpoint) -> Result<()>,
    ) -> Result<ExportReport> {
        let mut report = ExportReport {
            repair: self.repair_tail()?,
            ..ExportReport::default()
        };

        let sqlite_max = ledger::max_seq(ledger.conn())?;
        let last_seq = self.last_event()?.map_or(0, |e| e.seq);
        if last_seq > sqlite_max {
            return Err(behind(last_seq, sqlite_max));
        }
        let matches = match self.read_all() {
            Ok((events, tail)) if tail.cut_lines == 0 => {
                events.len() as i64 == last_seq && ledger::events_between(ledger.conn(), 0, last_seq)? == events
            }
            _ => false,
        };
        let file_max = if matches {
            last_seq
        } else {
            report.diverged = Some(self.move_aside(now)?);
            0
        };

        let pending = ledger::events_after(ledger.conn(), file_max)?;
        report.appended = self.append(&pending)?;
        if report.appended > 0 {
            failpoint(Failpoint::AfterAppend)?;
        }

        let new_cursor = pending.last().map_or(file_max, |e| e.seq);
        let cursor = export_cursor(ledger.conn())?;
        let unmarked: i64 = ledger.conn().query_row(
            "SELECT COUNT(*) FROM events WHERE seq > ?1 AND seq <= ?2 AND kind != 'exported'",
            [cursor, new_cursor],
            |r| r.get(0),
        )?;
        if unmarked > 0 {
            let mut tx = ledger.begin(now)?;
            tx.record_valid(
                Kind::Exported,
                "ledger",
                json!({"cursor": new_cursor}),
                json!({"cursor": new_cursor}),
                Dedupe::Latest,
            )?;
            tx.commit()?;
            report.cursor = Some(new_cursor);
        }
        Ok(report)
    }

    /// Appends `events` (already in sequence order), each to its month's file or to the latest
    /// file if that is later, and fsyncs every file it wrote.
    fn append(&self, events: &[Event]) -> Result<usize> {
        if events.is_empty() {
            return Ok(0);
        }
        if !self.dir.exists() {
            fs::create_dir_all(&self.dir)?;
            if let Some(parent) = self.dir.parent() {
                sync_dir(parent)?;
            }
        }
        let mut month: Option<String> = self
            .files()?
            .last()
            .map(|p| p.file_name().unwrap().to_string_lossy()[..7].to_string());
        let mut open: Option<(String, File)> = None;
        let mut created = false;
        for event in events {
            let own = month_of(event)?;
            let m = match &month {
                Some(latest) if latest.as_str() >= own => latest.clone(),
                _ => own.to_string(),
            };
            month = Some(m.clone());
            if open.as_ref().map(|(name, _)| name != &m).unwrap_or(true) {
                if let Some((_, f)) = open.take() {
                    f.sync_all()?;
                }
                let path = self.dir.join(format!("{m}.jsonl"));
                created |= !path.exists();
                open = Some((m.clone(), OpenOptions::new().create(true).append(true).open(path)?));
            }
            let (_, f) = open.as_mut().expect("a file is open");
            let mut text = Line::of(event).render();
            text.push('\n');
            f.write_all(text.as_bytes())?;
        }
        if let Some((_, f)) = open {
            f.sync_all()?;
        }
        if created {
            sync_dir(&self.dir)?;
        }
        Ok(events.len())
    }

    /// Moves every month file into a new `diverged-<time>` directory.
    fn move_aside(&self, now: &str) -> Result<PathBuf> {
        let stamp: String = now
            .chars()
            .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
            .collect();
        let mut dir = self.dir.join(format!("diverged-{stamp}"));
        let mut n = 1;
        while dir.exists() {
            n += 1;
            dir = self.dir.join(format!("diverged-{stamp}-{n}"));
        }
        fs::create_dir_all(&dir)?;
        for f in self.files()? {
            fs::rename(&f, dir.join(f.file_name().unwrap()))?;
        }
        sync_dir(&self.dir)?;
        Ok(dir)
    }

    /// Reads and verifies every line of every file: checksum, identity, contiguous sequence
    /// numbers from 1, and each event's predecessor. A corrupt line refuses with its file and
    /// line number, except an unterminated or unverifiable FINAL line of the LATEST file, which
    /// is a torn write and is reported in the second value.
    pub fn read_all(&self) -> Result<(Vec<Event>, TailRepair)> {
        let files = self.files()?;
        let mut events: Vec<Event> = Vec::new();
        let mut tail = TailRepair::default();
        let mut last_of: HashMap<(String, Kind), i64> = HashMap::new();
        for (fi, path) in files.iter().enumerate() {
            let bytes = fs::read(path)?;
            let (lines, _) = split_lines(&bytes);
            let last_file = fi + 1 == files.len();
            for (li, line) in lines.iter().enumerate() {
                let final_line = last_file && li + 1 == lines.len();
                let corrupt = |reason: String| Error::CorruptBackup {
                    file: path.clone(),
                    line: li + 1,
                    reason,
                };
                let event = match parse_line(line) {
                    Ok(e) => e,
                    Err(_) if final_line => {
                        tail.cut_lines += 1;
                        tail.cut_bytes += line.len() as u64;
                        continue;
                    }
                    Err(reason) => return Err(corrupt(reason)),
                };
                let expected = events.last().map_or(1, |e| e.seq + 1);
                if event.seq != expected {
                    return Err(corrupt(format!(
                        "expected sequence number {expected}, found {}",
                        event.seq
                    )));
                }
                let key = (event.subject.clone(), event.kind);
                let pred = last_of.get(&key).copied().unwrap_or(0);
                if event.pred != pred {
                    return Err(corrupt(format!(
                        "predecessor is {}, but the subject's previous {} event is {pred}",
                        event.pred,
                        event.kind.as_str()
                    )));
                }
                last_of.insert(key, event.seq);
                events.push(event);
            }
        }
        Ok((events, tail))
    }
}

/// The refusal for a ledger that is missing or behind its backup.
pub fn behind(backup_seq: i64, ledger_seq: i64) -> Error {
    Error::refused(format!(
        "the backup holds events up to seq {backup_seq} but the ledger only up to seq {ledger_seq}: \
         ledger.sqlite is missing, replaced or older than its backup. `issue-dashboard rebuild` \
         recreates it from the backup (with --force when a ledger.sqlite exists; it is kept beside)"
    ))
}

/// The cursor recorded by the latest `exported` event; 0 when there is none.
pub fn export_cursor(conn: &rusqlite::Connection) -> Result<i64> {
    Ok(ledger::latest(conn, "ledger", Kind::Exported)?
        .and_then(|e| e.data["cursor"].as_i64())
        .unwrap_or(0))
}
