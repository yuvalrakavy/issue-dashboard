//! Errors, and the exit code each maps to (spec chapter 8).

use std::path::PathBuf;
use std::time::Duration;

pub type Result<T, E = Error> = std::result::Result<T, E>;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// A check refused the command: an illegal transition, a non-owner, an unknown launch.
    #[error("refused: {0}")]
    Refused(String),

    /// The root lock was not free within the wait (10.4).
    #[error("the root lock {} is held by {}; gave up after {}s", .path.display(), .holder.as_deref().unwrap_or("an unknown holder"), .waited.as_secs())]
    LockTimeout { path: PathBuf, holder: Option<String>, waited: Duration },

    /// A line of the JSON Lines backup that is not a torn final line failed to parse or
    /// verify (10.2).
    #[error("corrupt backup line {}:{line}: {reason}", .file.display())]
    CorruptBackup { file: PathBuf, line: usize, reason: String },

    /// A configuration file failed to parse or is inconsistent.
    #[error("config {}: {reason}", .file.display())]
    Config { file: PathBuf, reason: String },

    /// A test failpoint fired (only in-process; the CLI aborts instead).
    #[error("failpoint {0} fired")]
    Failpoint(&'static str),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Sqlite(#[from] rusqlite::Error),

    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

impl Error {
    pub fn refused(message: impl Into<String>) -> Self {
        Error::Refused(message.into())
    }

    /// 1: refused by a check; 2: a file or environment error (spec chapter 8).
    pub fn exit_code(&self) -> i32 {
        match self {
            Error::Refused(_) | Error::CorruptBackup { .. } | Error::Config { .. } => 1,
            _ => 2,
        }
    }
}
