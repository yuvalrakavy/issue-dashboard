//! A tracker root, and the shape of every mutating command (10.1, 10.4).

use std::fs;
use std::io::Write;
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

    /// The reader lease: shared by every reader of the ledger, exclusive for `rebuild`, so a
    /// rebuild never moves the ledger out from under a read (10.6).
    pub fn readers_path(&self) -> PathBuf {
        self.dir.join("readers")
    }

    fn lease_file(&self) -> Result<fs::File> {
        fs::create_dir_all(&self.dir)?;
        Ok(fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(self.readers_path())?)
    }

    /// Takes a shared reader lease, waiting while a rebuild holds it.
    fn reader_lease(&self) -> Result<fs::File> {
        let f = self.lease_file()?;
        f.lock_shared()?;
        Ok(f)
    }

    fn exclusive_lease(&self) -> Result<fs::File> {
        let f = self.lease_file()?;
        let start = std::time::Instant::now();
        loop {
            match f.try_lock() {
                Ok(()) => return Ok(f),
                Err(fs::TryLockError::WouldBlock) if start.elapsed() < self.options.lock_wait => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(fs::TryLockError::WouldBlock) => {
                    return Err(Error::ReadersActive {
                        path: self.readers_path(),
                        waited: self.options.lock_wait,
                    });
                }
                Err(fs::TryLockError::Error(e)) => return Err(e.into()),
            }
        }
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
        self.locked(clock, f, |_| Ok(())).map(|(_, out)| out)
    }

    /// The body of every mutating command; `then` runs last, still under the lock.
    fn locked<T, R>(
        &self,
        clock: impl FnOnce() -> DateTime<Utc>,
        f: impl FnOnce(&mut Tx<'_>) -> Result<T>,
        then: impl FnOnce(&Root) -> Result<R>,
    ) -> Result<(R, T)> {
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
        Ok((then(self)?, out))
    }

    /// Opens the ledger for reading, without the lock. Refuses when there is no ledger yet,
    /// rather than creating one.
    pub fn read<T>(&self, f: impl FnOnce(&Ledger) -> Result<T>) -> Result<T> {
        let _lease = self.reader_lease()?;
        let ledger = Ledger::open_existing(&self.ledger_path())?;
        f(&ledger)
    }

    /// [`Root::mutate_with_clock`], then `g` over a read of the ledger as it stood at that
    /// commit: the read transaction is begun while the root lock is still held, and `g` runs
    /// after the lock is released, so a slow derive never blocks other commands and never
    /// sees a later command's events (10.4 step 3).
    pub fn mutate_then_read<T, U>(
        &self,
        clock: impl FnOnce() -> DateTime<Utc>,
        f: impl FnOnce(&mut Tx<'_>) -> Result<T>,
        g: impl FnOnce(&Ledger, T) -> Result<U>,
    ) -> Result<U> {
        let ((_lease, ledger), out) = self.locked(clock, f, |root| {
            let lease = root.reader_lease()?;
            let ledger = Ledger::open_existing(&root.ledger_path())?;
            ledger.pin()?;
            Ok((lease, ledger))
        })?;
        let result = g(&ledger, out);
        ledger.unpin()?;
        result
    }

    /// Recreates `ledger.sqlite` from the backup (10.2), verifying every line. Refuses when
    /// `ledger.sqlite` exists unless `force`, which keeps the old file (and its WAL) beside it, renamed.
    pub fn rebuild(&self, now: DateTime<Utc>, force: bool) -> Result<RebuildReport> {
        let _lock = self.lock()?;
        // Readers do not take the lock, so the lock alone cannot keep one from opening the
        // ledger this moves aside; the exclusive lease waits for the running ones and keeps new
        // ones out until the new ledger is in place.
        let _lease = self.exclusive_lease()?;
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

        // Nothing of the old database is deleted or rewritten: its main file and whatever
        // `-wal` / `-shm` lie beside the target are RENAMED aside together, as one openable
        // set, and only then is the new file renamed in. A crash in between leaves no
        // ledger.sqlite, which the next command refuses (naming rebuild) rather than opening
        // empty; a stray WAL is never left beside the new file.
        let mut moved_aside = None;
        let old: Vec<PathBuf> = std::iter::once(target.clone())
            .chain(companions(&target))
            .filter(|p| p.exists())
            .collect();
        // `rebuild.pending` names the aside set while the old files are being moved, so a move
        // interrupted by a crash resumes into the SAME set and a main file is never separated
        // from its WAL by name. It is removed once the old set is complete, before the new file
        // goes in, so a pending marker always means "the target's main file is already aside".
        let pending = self.dir.join("rebuild.pending");
        let resumed = if pending.exists() {
            let name = fs::read_to_string(&pending)?.trim().to_string();
            if target.exists() || name.is_empty() || name.contains('/') {
                return Err(Error::refused(format!(
                    "{} names {name:?} as an interrupted rebuild's aside set, but it does not match what is on disk; \
                     inspect it and remove it before rebuilding",
                    pending.display()
                )));
            }
            Some(self.dir.join(name))
        } else {
            None
        };
        if !old.is_empty() || resumed.is_some() {
            let aside = match resumed.clone() {
                Some(a) => a,
                None => {
                    let safe: String = stamp
                        .chars()
                        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
                        .collect();
                    let free = |a: &Path| {
                        std::iter::once(a.to_path_buf())
                            .chain(companions(a))
                            .all(|p| !p.exists())
                    };
                    let mut aside = self.dir.join(format!("ledger.sqlite.before-rebuild-{safe}"));
                    let mut n = 1;
                    while !free(&aside) {
                        n += 1;
                        aside = self.dir.join(format!("ledger.sqlite.before-rebuild-{safe}-{n}"));
                    }
                    let mut f = fs::File::create(&pending)?;
                    writeln!(f, "{}", aside.file_name().unwrap().to_string_lossy())?;
                    f.sync_all()?;
                    sync_dir(&self.dir)?;
                    aside
                }
            };
            // The main file first: once it is gone, a crash leaves "no ledger" and the marker.
            let pairs = std::iter::once((target.clone(), aside.clone()))
                .chain(companions(&target).into_iter().zip(companions(&aside)));
            for (from, to) in pairs {
                if from.exists() {
                    if to.exists() {
                        return Err(Error::refused(format!(
                            "{} would overwrite {}; inspect both before rebuilding",
                            from.display(),
                            to.display()
                        )));
                    }
                    fs::rename(&from, &to)?;
                }
            }
            sync_dir(&self.dir)?;
            fs::remove_file(&pending)?;
            sync_dir(&self.dir)?;
            moved_aside = Some(aside);
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
