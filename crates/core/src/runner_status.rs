//! The bridge runner's live status (runner spec 1.3) and the reason each launch it holds is
//! waiting (4.2). Not event tables: both describe the running process, not history, so they are
//! derived, not in the backup, and a `rebuild` starts them empty; the runner rewrites them on
//! its next tick.

use chrono::{DateTime, Duration, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;
use serde_json::Value;

use crate::Result;
use crate::root::{format_time, parse_time};

pub(crate) const TABLES: &str = "
CREATE TABLE IF NOT EXISTS runner_status (
    id         INTEGER PRIMARY KEY CHECK (id = 1),
    pid        INTEGER NOT NULL,
    started_at TEXT NOT NULL,
    version    TEXT NOT NULL,
    contract   INTEGER NOT NULL,
    cursor     INTEGER,
    tick_at    TEXT NOT NULL,
    health     TEXT NOT NULL
);
CREATE TABLE IF NOT EXISTS runner_reasons (
    launch TEXT PRIMARY KEY,
    reason TEXT NOT NULL,
    since  TEXT NOT NULL
);
";

/// A tick older than this reads as "runner down" (1.3).
pub const DOWN_AFTER_SECONDS: i64 = 60;

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct RunnerStatus {
    pub pid: u32,
    pub started_at: String,
    /// The AgentBridge package version the runner was pinned to.
    pub version: String,
    /// The connector contract version it was pinned to.
    pub contract: u32,
    /// The connector's event cursor: the `after` of the next `wait`.
    pub cursor: Option<i64>,
    pub tick_at: String,
    /// The last health snapshot, per harness.
    pub health: Value,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Held {
    pub launch: String,
    pub reason: String,
    pub since: String,
}

/// The runner started: its process and pin replace the row's, and the cursor is kept, since
/// it is the connector's and outlives the process.
pub fn started(conn: &Connection, pid: u32, at: DateTime<Utc>, version: &str, contract: u32) -> Result<()> {
    conn.execute(
        "INSERT INTO runner_status (id, pid, started_at, version, contract, cursor, tick_at, health)
         VALUES (1, ?1, ?2, ?3, ?4, NULL, ?2, '{}')
         ON CONFLICT (id) DO UPDATE SET pid = ?1, started_at = ?2, version = ?3, contract = ?4, tick_at = ?2",
        params![pid, format_time(at), version, contract],
    )?;
    Ok(())
}

/// One loop tick: the time, the cursor reached (`None` keeps the last) and the health read.
/// Before [`started`] there is no row, and a tick changes nothing.
pub fn ticked(conn: &Connection, at: DateTime<Utc>, cursor: Option<i64>, health: &Value) -> Result<()> {
    conn.execute(
        "UPDATE runner_status SET tick_at = ?1, cursor = COALESCE(?2, cursor), health = ?3 WHERE id = 1",
        params![format_time(at), cursor, health.to_string()],
    )?;
    Ok(())
}

pub fn status(conn: &Connection) -> Result<Option<RunnerStatus>> {
    let row = conn
        .query_row(
            "SELECT pid, started_at, version, contract, cursor, tick_at, health FROM runner_status WHERE id = 1",
            [],
            |r| {
                Ok((
                    r.get::<_, u32>(0)?,
                    r.get::<_, String>(1)?,
                    r.get::<_, String>(2)?,
                    r.get::<_, u32>(3)?,
                    r.get::<_, Option<i64>>(4)?,
                    r.get::<_, String>(5)?,
                    r.get::<_, String>(6)?,
                ))
            },
        )
        .optional()?;
    row.map(|(pid, started_at, version, contract, cursor, tick_at, health)| {
        Ok(RunnerStatus {
            pid,
            started_at,
            version,
            contract,
            cursor,
            tick_at,
            health: serde_json::from_str(&health)?,
        })
    })
    .transpose()
}

/// No status, or a tick older than [`DOWN_AFTER_SECONDS`] at `now`.
pub fn is_down(s: Option<&RunnerStatus>, now: DateTime<Utc>) -> Result<bool> {
    Ok(match s {
        None => true,
        Some(s) => now - parse_time(&s.tick_at)? > Duration::seconds(DOWN_AFTER_SECONDS),
    })
}

/// `launch` waits for `reason`; `since` is kept while the reason stays the same.
pub fn hold(conn: &Connection, launch: &str, reason: &str, at: DateTime<Utc>) -> Result<()> {
    conn.execute(
        "INSERT INTO runner_reasons (launch, reason, since) VALUES (?1, ?2, ?3)
         ON CONFLICT (launch) DO UPDATE SET reason = ?2, since = ?3 WHERE reason <> ?2",
        params![launch, reason, format_time(at)],
    )?;
    Ok(())
}

/// `launch` no longer waits (it was reserved, withdrawn or ended).
pub fn unhold(conn: &Connection, launch: &str) -> Result<()> {
    conn.execute("DELETE FROM runner_reasons WHERE launch = ?1", params![launch])?;
    Ok(())
}

pub fn held(conn: &Connection) -> Result<Vec<Held>> {
    let mut stmt = conn.prepare("SELECT launch, reason, since FROM runner_reasons ORDER BY launch")?;
    let rows = stmt.query_map([], |r| {
        Ok(Held {
            launch: r.get(0)?,
            reason: r.get(1)?,
            since: r.get(2)?,
        })
    })?;
    Ok(rows.collect::<rusqlite::Result<_>>()?)
}
