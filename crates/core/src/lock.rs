//! The root lock: an exclusive lock on `<root>/lock` (10.4).
//!
//! Held only while a command writes the ledger and exports it; never across a network call.
//! A waiter gives up after a bounded wait, naming the holder's command and process id, which
//! the holder writes into the lock file once it has the lock.

use std::fs::{File, OpenOptions, TryLockError};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use crate::{Error, Result};

/// The default wait before giving up (10.4).
pub const DEFAULT_WAIT: Duration = Duration::from_secs(30);

const POLL: Duration = Duration::from_millis(20);

/// The held lock. Dropping it releases the lock.
pub struct RootLock {
    file: File,
    path: PathBuf,
}

impl RootLock {
    /// Waits up to `wait` for the exclusive lock on `path`, then records `command` and this
    /// process's id in the file so a waiter can name the holder.
    pub fn acquire(path: &Path, command: &str, wait: Duration) -> Result<RootLock> {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)?;
        let start = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => break,
                Err(TryLockError::WouldBlock) => {
                    if start.elapsed() >= wait {
                        return Err(Error::LockTimeout {
                            path: path.to_path_buf(),
                            holder: read_holder(path),
                            waited: start.elapsed(),
                        });
                    }
                    thread::sleep(POLL);
                }
                Err(TryLockError::Error(e)) => return Err(e.into()),
            }
        }
        file.set_len(0)?;
        file.seek(SeekFrom::Start(0))?;
        writeln!(file, "pid={} command={}", std::process::id(), command)?;
        file.flush()?;
        Ok(RootLock {
            file,
            path: path.to_path_buf(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for RootLock {
    fn drop(&mut self) {
        // Clear the holder line first, so a waiter that reads it later does not name a
        // holder that has gone. The lock itself is released when the file is closed.
        let _ = self.file.set_len(0);
        let _ = self.file.unlock();
    }
}

/// The holder line a lock holder wrote, if any. Read through a separate handle, which does
/// not need the lock.
fn read_holder(path: &Path) -> Option<String> {
    let mut text = String::new();
    File::open(path).ok()?.read_to_string(&mut text).ok()?;
    let line = text.trim();
    (!line.is_empty()).then(|| line.to_string())
}
