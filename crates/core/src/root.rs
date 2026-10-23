//! A tracker root, and the shape of every mutating command (10.1, 10.4).

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use chrono::{DateTime, SecondsFormat, Utc};

use crate::backup::{Backup, Failpoint};
use crate::ledger::{self, Ledger, Tx};
use crate::lock::{DEFAULT_WAIT, RootLock};
use crate::{Error, Result};

/// How a [`Root`] runs its commands.
#[derive(Debug, Clone)]
pub struct Options {
    /// The command name written into the lock file for a waiter to see.
    pub command: String,
    /// How long to wait for the root lock (10.4: 30 s).
    pub lock_wait: Duration,
    /// A simulated crash point, for tests.
    pub failpoint: Option<Failpoint>,
    /// At the failpoint, abort the process (the CLI) instead of returning an error.
    pub abort_on_failpoint: bool,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            command: "issue-dashboard".into(),
            lock_wait: DEFAULT_WAIT,
            failpoint: None,
            abort_on_failpoint: false,
        }
    }
}

/// The tracker root: `<root>/ledger.sqlite`, `<root>/ledger/*.jsonl`, `<root>/lock`.
#[derive(Debug, Clone)]
pub struct Root {
    dir: PathBuf,
    options: Options,
}

/// A time as the ledger stores it: RFC 3339, UTC, millisecond precision.
pub fn format_time(t: DateTime<Utc>) -> String {
    t.to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Parses a time the ledger stored.
pub fn parse_time(s: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .map(|t| t.with_timezone(&Utc))
        .map_err(|e| Error::refused(format!("not an RFC 3339 time: {s}: {e}")))
}

/// The `-wal` and `-shm` companions of an SQLite file.
fn companions(db: &Path) -> [PathBuf; 2] {
    let s = db.as_os_str().to_string_lossy();
    [PathBuf::from(format!("{s}-wal")), PathBuf::from(format!("{s}-shm"))]
}

fn sync_dir(dir: &Path) -> Result<()> {
    fs::File::open(dir)?.sync_all()?;
    Ok(())
}

/// A consistent copy of an SQLite database, WAL included. Falls back to copying the files
/// when SQLite cannot read it (a damaged ledger is still worth keeping).
///
/// It first checkpoints the old database's WAL into its main file, so that removing the `-wal`
/// afterwards (before the new file is renamed into place) loses nothing even if the process
/// dies in between. SQLite reports a checkpoint a reader blocked as a successful row saying
/// "busy"; that is refused, since the WAL then still holds committed events. When SQLite cannot
/// open or checkpoint the file at all, the main file and its WAL are copied together, so the
/// copy holds everything the old pair did.
fn copy_database(from: &Path, to: &Path) -> Result<()> {
    let checkpointed = rusqlite::Connection::open(from).and_then(|c| {
        let (busy, log, done): (i64, i64, i64) = c.query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?))
        })?;
        Ok((c, busy != 0 || done < log))
    });
    match checkpointed {
        Ok((_, true)) => {
            return Err(Error::refused(format!(
                "{} is in use: a reader keeps its WAL from being checkpointed; retry rebuild when it has finished",
                from.display()
            )));
        }
        Ok((c, false)) => c.execute("VACUUM INTO ?1", [to.to_string_lossy()]).map(|_| ())?,
        Err(_) => {
            fs::copy(from, to)?;
            for (f, t) in companions(from).into_iter().zip(companions(to)) {
                if f.exists() {
                    fs::copy(f, &t)?;
                    fs::File::open(&t)?.sync_all()?;
                }
            }
        }
    }
    fs::File::open(to)?.sync_all()?;
    Ok(())
}

impl Root {
    pub fn new(dir: impl Into<PathBuf>, options: Options) -> Root {
        Root {
            dir: dir.into(),
            options,
        }
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn ledger_path(&self) -> PathBuf {
        self.dir.join("ledger.sqlite")
    }

    pub fn backup(&self) -> Backup {
        Backup::new(self.dir.join("ledger"))
    }

    pub fn lock_path(&self) -> PathBuf {
        self.dir.join("lock")
    }

    fn lock(&self) -> Result<RootLock> {
        fs::create_dir_all(&self.dir)?;
        RootLock::acquire(&self.lock_path(), &self.options.command, self.options.lock_wait)
    }

    fn failpoint(&self, at: Failpoint) -> Result<()> {
        if self.options.failpoint != Some(at) {
            return Ok(());
        }
        if self.options.abort_on_failpoint {
            eprintln!("failpoint {} fired: aborting", at.name());
            std::process::abort();
        }
        Err(Error::Failpoint(at.name()))
    }

    /// Runs one mutating command: takes the root lock, brings the backup up to date (a
    /// previous command may have crashed before exporting), runs `f` in one transaction,
    /// commits, exports what it recorded, and releases the lock. `f` must not make network
    /// calls (10.4).
    pub fn mutate<T>(&self, now: DateTime<Utc>, f: impl FnOnce(&mut Tx<'_>) -> Result<T>) -> Result<T> {
        self.mutate_with_clock(|| now, f)
    }

    /// [`Root::mutate`] with the command's time read from `clock` once the lock is held, so an
    /// event's time is never earlier than events recorded by whoever held the lock before.
    pub fn mutate_with_clock<T>(
        &self,
        clock: impl FnOnce() -> DateTime<Utc>,
        f: impl FnOnce(&mut Tx<'_>) -> Result<T>,
    ) -> Result<T> {
        let _lock = self.lock()?;
        let now = format_time(clock());
        let backup = self.backup();
        // Never create an empty ledger beside a backup: that is a lost ledger, which rebuild
        // restores; opening one here would make every later command refuse until --force.
        // Read-only: a torn tail here is left for rebuild, which tolerates it.
        if !self.ledger_path().exists() && backup.holds_events()? {
            let held = match backup.read_all() {
                Ok((events, _)) => events.len() as i64,
                // Corruption is for rebuild to report; a failed read is reported as itself.
                Err(Error::CorruptBackup { .. }) => 0,
                Err(e) => return Err(e),
            };
            return Err(crate::backup::behind(held, 0));
        }
        let mut ledger = Ledger::open(&self.ledger_path())?;
        backup.catch_up(&mut ledger, &now, &mut |_| Ok(()))?;

        let out = {
            let mut tx = ledger.begin(&now)?;
            let out = f(&mut tx)?;
            tx.commit()?;
            out
        };
        self.failpoint(Failpoint::AfterCommit)?;
        backup.catch_up(&mut ledger, &now, &mut |at| self.failpoint(at))?;
        Ok(out)
    }

    /// Opens the ledger for reading, without the lock. Refuses when there is no ledger yet,
    /// rather than creating one.
    pub fn read<T>(&self, f: impl FnOnce(&Ledger) -> Result<T>) -> Result<T> {
        let ledger = Ledger::open_existing(&self.ledger_path())?;
        f(&ledger)
    }

    /// Recreates `ledger.sqlite` from the backup (10.2), verifying every line. Refuses when
    /// `ledger.sqlite` exists unless `force`, which keeps a copy of the old file beside it.
    pub fn rebuild(&self, now: DateTime<Utc>, force: bool) -> Result<RebuildReport> {
        let _lock = self.lock()?;
        let stamp = format_time(now);
        let target = self.ledger_path();
        if target.exists() && !force {
            return Err(Error::refused(format!(
                "{} exists; SQLite is the authority, so rebuild replaces it only with --force (which keeps it beside)",
                target.display()
            )));
        }
        let backup = self.backup();
        let (events, tail) = backup.read_all()?;

        let tmp = self.dir.join("ledger.sqlite.rebuild");
        for p in std::iter::once(tmp.clone()).chain(companions(&tmp)) {
            if p.exists() {
                fs::remove_file(&p)?;
            }
        }
        {
            let mut fresh = Ledger::open(&tmp)?;
            let tx = fresh.begin(&stamp)?;
            for e in &events {
                ledger::insert_verbatim(tx.conn(), e)?;
            }
            tx.commit()?;
            fresh.conn().execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
        }

        // The old ledger is COPIED aside, never renamed away, so at every moment there is a
        // ledger.sqlite: a crash here leaves the old one in place, and the next command still
        // works on it.
        let mut moved_aside = None;
        if target.exists() {
            let safe: String = stamp
                .chars()
                .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                .collect();
            let mut aside = self.dir.join(format!("ledger.sqlite.before-rebuild-{safe}"));
            let mut n = 1;
            while aside.exists() {
                n += 1;
                aside = self.dir.join(format!("ledger.sqlite.before-rebuild-{safe}-{n}"));
            }
            copy_database(&target, &aside)?;
            moved_aside = Some(aside);
        }
        // A WAL left beside the target belongs to the old database; the new file must never
        // be opened next to it.
        for p in companions(&target) {
            if p.exists() {
                fs::remove_file(p)?;
            }
        }
        fs::rename(&tmp, &target)?;
        for p in companions(&tmp) {
            if p.exists() {
                fs::remove_file(p)?;
            }
        }
        sync_dir(&self.dir)?;
        Ok(RebuildReport {
            events: events.len(),
            files: backup.files()?.len(),
            dropped_tail_lines: tail.cut_lines,
            moved_aside,
        })
    }
}

/// What `rebuild` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RebuildReport {
    pub events: usize,
    pub files: usize,
    /// Lines of a torn tail that were dropped.
    pub dropped_tail_lines: usize,
    /// Where the previous `ledger.sqlite` was moved, with `force`.
    pub moved_aside: Option<PathBuf>,
}
