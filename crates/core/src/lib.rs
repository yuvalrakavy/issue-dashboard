//! The deterministic core of issue-dashboard: the event ledger and what is built on it
//! (spec chapter 10).
//!
//! Every state change is an event in one SQLite ledger per tracker root ([`ledger`]),
//! recorded under the root lock ([`lock`]) by a mutating command ([`root::Root::mutate`]),
//! and appended to the JSON Lines backup before the lock is released ([`backup`]).

pub mod backup;
pub mod brief;
pub mod canonical;
pub mod config;
pub mod cycle;
pub mod decision;
pub mod derive;
pub mod docs;
pub mod error;
pub mod event;
pub mod github;
pub mod ingest;
pub mod launch;
pub mod ledger;
pub mod lock;
pub mod observe;
pub mod outcome;
pub mod publish;
pub mod read;
pub mod root;
pub mod schema;
pub mod sessions;
pub mod snapshot;
pub mod suggest;
pub mod sync;
pub mod translate;
pub mod write;

pub use error::{Error, Result};
pub use event::{Event, Kind};
pub use root::Root;
