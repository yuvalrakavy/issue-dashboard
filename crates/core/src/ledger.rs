//! The SQLite ledger: an append-only log of events (10.2).
//!
//! Recording an event whose distinguishing fields repeat what is already recorded is a
//! no-op ([`Dedupe`]); that is what makes a repeated command safe (10.3).

use std::path::Path;
use std::time::Duration;

use rusqlite::{Connection, OptionalExtension, Row, Transaction, TransactionBehavior, params};
use serde_json::Value;

use crate::event::{Event, Kind, identity};
use crate::{Error, Result};

/// When a new event counts as a repeat of one already recorded.
///
/// The identity of 10.3 includes the subject's predecessor, so a value returning to an earlier
/// state is a new occurrence. On its own that cannot make a retry a no-op: a retry that comes
/// after the first attempt committed sees that attempt as its predecessor, and so computes a
/// new identity. A repeat is therefore decided by comparing distinguishing fields, as below;
/// the identity stays unique and is what the backup and `rebuild` verify.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dedupe {
    /// A repeat when the subject's LATEST event of this kind has the same distinguishing
    /// fields. A value that returns to an earlier state (A → B → A) is recorded, since it
    /// differs from the latest; a retry against an unchanged latest is not. For state-like
    /// kinds: `config`, `observed`, `exported`; and `decided`, distinguished by project and
    /// verdict (see `decision::ingest`).
    Latest,
    /// A repeat when ANY event of this subject and kind has the same distinguishing fields.
    /// For kinds whose distinguishing fields never legitimately recur: `suggested` (run id +
    /// decision), `item_gone` (the confirming read), `outcome` (one per launch).
    Any,
    /// Only an identical identity is a repeat; the caller has already decided. For `launch`,
    /// whose transitions are checked against 10.7's table before recording.
    IdentityOnly,
}

/// What [`Tx::record`] did.
#[derive(Debug, Clone, PartialEq)]
pub enum Recorded {
    New(Event),
    Repeat(Event),
}

impl Recorded {
    pub fn event(&self) -> &Event {
        match self {
            Recorded::New(e) | Recorded::Repeat(e) => e,
        }
    }

    pub fn is_new(&self) -> bool {
        matches!(self, Recorded::New(_))
    }
}

const SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS events (
    seq          INTEGER PRIMARY KEY,
    time         TEXT    NOT NULL,
    kind         TEXT    NOT NULL,
    subject      TEXT    NOT NULL,
    pred         INTEGER NOT NULL,
    distinct_json TEXT   NOT NULL,
    identity     TEXT    NOT NULL UNIQUE,
    data         TEXT    NOT NULL
);
CREATE INDEX IF NOT EXISTS events_subject_kind ON events (subject, kind, seq);
CREATE INDEX IF NOT EXISTS events_kind ON events (kind, seq);
CREATE TRIGGER IF NOT EXISTS events_no_update BEFORE UPDATE ON events
    BEGIN SELECT RAISE(ABORT, 'the ledger is append-only'); END;
CREATE TRIGGER IF NOT EXISTS events_no_delete BEFORE DELETE ON events
    BEGIN SELECT RAISE(ABORT, 'the ledger is append-only'); END;
";

/// The ledger's schema version, kept in SQLite's `user_version`.
const VERSION: i64 = 1;

/// Not an event table: the freshest (updatedAt, read time) each item was read at, kept so an
/// unchanged read still makes an older read stale (10.4 step 2). It is derived and not in the
/// backup; after `rebuild` it starts from the recorded observations.
const FRESHNESS: &str = "
CREATE TABLE IF NOT EXISTS freshness (
    node       TEXT PRIMARY KEY,
    updated_at TEXT NOT NULL,
    read_at    TEXT NOT NULL
);
";

const COLUMNS: &str = "seq, time, kind, subject, pred, distinct_json, identity, data";

/// An open ledger file.
pub struct Ledger {
    conn: Connection,
}

impl Ledger {
    /// Opens (creating if needed) the ledger at `path`, in WAL mode.
    pub fn open(path: &Path) -> Result<Ledger> {
        let conn = Connection::open(path)?;
        conn.busy_timeout(Duration::from_secs(5))?;
        let mode: String = conn.query_row("PRAGMA journal_mode = WAL", [], |r| r.get(0))?;
        if !mode.eq_ignore_ascii_case("wal") {
            return Err(Error::refused(format!(
                "{} could not be put in WAL mode ({mode})",
                path.display()
            )));
        }
        // FULL: a committed transaction survives a power loss, not only a process crash.
        conn.execute_batch("PRAGMA synchronous = FULL;")?;
        let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
        match version {
            0 => conn.execute_batch(&format!("{SCHEMA} PRAGMA user_version = {VERSION};"))?,
            VERSION => conn.execute_batch(SCHEMA)?,
            other => {
                return Err(Error::refused(format!(
                    "{} has ledger schema version {other}; this build knows version {VERSION}",
                    path.display()
                )));
            }
        }
        conn.execute_batch(FRESHNESS)?;
        Ok(Ledger { conn })
    }

    /// Opens an existing ledger; refuses, naming `rebuild`, when there is none.
    pub fn open_existing(path: &Path) -> Result<Ledger> {
        if !path.exists() {
            return Err(Error::refused(format!(
                "there is no ledger at {}; `issue-dashboard rebuild` recreates it from the backup",
                path.display()
            )));
        }
        Ledger::open(path)
    }

    /// Starts a write transaction; events recorded through it carry `now` as their time.
    pub fn begin(&mut self, now: &str) -> Result<Tx<'_>> {
        let tx = self.conn.transaction_with_behavior(TransactionBehavior::Immediate)?;
        Ok(Tx {
            tx,
            now: now.to_string(),
        })
    }

    pub fn conn(&self) -> &Connection {
        &self.conn
    }
}

/// A write transaction. Dropping it without [`Tx::commit`] rolls back.
pub struct Tx<'a> {
    tx: Transaction<'a>,
    now: String,
}

impl<'a> Tx<'a> {
    pub fn now(&self) -> &str {
        &self.now
    }

    pub fn conn(&self) -> &Connection {
        &self.tx
    }

    /// Records an event, or returns the existing one when it is a repeat under `dedupe`.
    pub fn record(
        &mut self,
        kind: Kind,
        subject: &str,
        distinct: Value,
        data: Value,
        dedupe: Dedupe,
    ) -> Result<Recorded> {
        let latest = latest(&self.tx, subject, kind)?;
        match dedupe {
            Dedupe::Latest => {
                if let Some(e) = &latest
                    && e.distinct == distinct
                {
                    return Ok(Recorded::Repeat(e.clone()));
                }
            }
            Dedupe::Any => {
                if let Some(e) = events_of(&self.tx, subject, kind)?
                    .into_iter()
                    .find(|e| e.distinct == distinct)
                {
                    return Ok(Recorded::Repeat(e));
                }
            }
            Dedupe::IdentityOnly => {}
        }
        let pred = latest.map_or(0, |e| e.seq);
        let identity = identity(kind, subject, pred, &distinct);
        if let Some(e) = by_identity(&self.tx, &identity)? {
            return Ok(Recorded::Repeat(e));
        }
        self.tx.execute(
            "INSERT INTO events (time, kind, subject, pred, distinct_json, identity, data)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                self.now,
                kind.as_str(),
                subject,
                pred,
                serde_json::to_string(&distinct)?,
                identity,
                serde_json::to_string(&data)?
            ],
        )?;
        let seq = self.tx.last_insert_rowid();
        Ok(Recorded::New(Event {
            seq,
            time: self.now.clone(),
            kind,
            subject: subject.to_string(),
            pred,
            distinct,
            identity,
            data,
        }))
    }

    /// [`Tx::record`], refusing a new event that does not match its kind's schema
    /// (`schemas/event-<kind>.schema.json`); the transaction is then rolled back by the caller's
    /// error. Every module records through this; plain `record` is for tests of the ledger itself.
    pub fn record_valid(
        &mut self,
        kind: Kind,
        subject: &str,
        distinct: Value,
        data: Value,
        dedupe: Dedupe,
    ) -> Result<Recorded> {
        let r = self.record(kind, subject, distinct, data, dedupe)?;
        if let Recorded::New(e) = &r {
            crate::schema::validate(
                &crate::schema::event_stem(kind),
                &format!("the {} event for {subject}", kind.as_str()),
                &serde_json::to_value(e)?,
            )?;
        }
        Ok(r)
    }

    pub fn commit(self) -> Result<()> {
        self.tx.commit()?;
        Ok(())
    }
}

/// An events row as stored: seq, time, kind, subject, pred, distinct, identity, data.
type RawRow = (i64, String, String, String, i64, String, String, String);

fn from_row(row: &Row<'_>) -> rusqlite::Result<RawRow> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
        row.get(6)?,
        row.get(7)?,
    ))
}

fn to_event(raw: RawRow) -> Result<Event> {
    let (seq, time, kind, subject, pred, distinct, identity, data) = raw;
    let kind = Kind::parse(&kind).ok_or_else(|| Error::refused(format!("event {seq} has an unknown kind {kind}")))?;
    Ok(Event {
        seq,
        time,
        kind,
        subject,
        pred,
        distinct: serde_json::from_str(&distinct)?,
        identity,
        data: serde_json::from_str(&data)?,
    })
}

fn query(conn: &Connection, sql: &str, args: impl rusqlite::Params) -> Result<Vec<Event>> {
    let mut stmt = conn.prepare_cached(sql)?;
    let rows = stmt.query_map(args, from_row)?;
    let mut out = Vec::new();
    for row in rows {
        out.push(to_event(row?)?);
    }
    Ok(out)
}

fn query_one(conn: &Connection, sql: &str, args: impl rusqlite::Params) -> Result<Option<Event>> {
    let raw = conn.prepare_cached(sql)?.query_row(args, from_row).optional()?;
    raw.map(to_event).transpose()
}

fn by_identity(conn: &Connection, identity: &str) -> Result<Option<Event>> {
    query_one(
        conn,
        &format!("SELECT {COLUMNS} FROM events WHERE identity = ?1"),
        [identity],
    )
}

/// The subject's latest event of `kind`.
pub fn latest(conn: &Connection, subject: &str, kind: Kind) -> Result<Option<Event>> {
    query_one(
        conn,
        &format!("SELECT {COLUMNS} FROM events WHERE subject = ?1 AND kind = ?2 ORDER BY seq DESC LIMIT 1"),
        params![subject, kind.as_str()],
    )
}

/// Every event of `subject` and `kind`, in sequence order.
pub fn events_of(conn: &Connection, subject: &str, kind: Kind) -> Result<Vec<Event>> {
    query(
        conn,
        &format!("SELECT {COLUMNS} FROM events WHERE subject = ?1 AND kind = ?2 ORDER BY seq"),
        params![subject, kind.as_str()],
    )
}

/// Every event of `kind`, in sequence order.
pub fn events_by_kind(conn: &Connection, kind: Kind) -> Result<Vec<Event>> {
    query(
        conn,
        &format!("SELECT {COLUMNS} FROM events WHERE kind = ?1 ORDER BY seq"),
        [kind.as_str()],
    )
}

/// Every event with a sequence number above `after`, in order.
pub fn events_after(conn: &Connection, after: i64) -> Result<Vec<Event>> {
    query(
        conn,
        &format!("SELECT {COLUMNS} FROM events WHERE seq > ?1 ORDER BY seq"),
        [after],
    )
}

/// Every event with `after < seq <= upto`, in order.
pub fn events_between(conn: &Connection, after: i64, upto: i64) -> Result<Vec<Event>> {
    query(
        conn,
        &format!("SELECT {COLUMNS} FROM events WHERE seq > ?1 AND seq <= ?2 ORDER BY seq"),
        [after, upto],
    )
}

/// The event with this sequence number.
pub fn event_at(conn: &Connection, seq: i64) -> Result<Option<Event>> {
    query_one(conn, &format!("SELECT {COLUMNS} FROM events WHERE seq = ?1"), [seq])
}

/// The highest sequence number, 0 when the ledger is empty.
pub fn max_seq(conn: &Connection) -> Result<i64> {
    Ok(conn.query_row("SELECT COALESCE(MAX(seq), 0) FROM events", [], |r| r.get(0))?)
}

/// Inserts an event exactly as given (seq included); used only by `rebuild`.
pub fn insert_verbatim(conn: &Connection, event: &Event) -> Result<()> {
    conn.execute(
        "INSERT INTO events (seq, time, kind, subject, pred, distinct_json, identity, data)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
        params![
            event.seq,
            event.time,
            event.kind.as_str(),
            event.subject,
            event.pred,
            serde_json::to_string(&event.distinct)?,
            event.identity,
            serde_json::to_string(&event.data)?
        ],
    )?;
    Ok(())
}
