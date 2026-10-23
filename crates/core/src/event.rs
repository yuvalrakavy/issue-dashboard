//! Events and their identity (10.3).

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::canonical::hash_value;

/// The event kinds of phase 1 (10.3). The write events are phase 1's one GitHub write, a close
/// from the page (10.10); phase 2 adds the rest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    Config,
    Observed,
    ItemGone,
    Decided,
    ViewSaved,
    Request,
    Suggested,
    Launch,
    Outcome,
    Exported,
    /// A project's issue and pull-request set in one repo changed (2.2 read 1).
    Membership,
    /// A cycle took its outbox sequence number (10.4 step 3); the event's seq is the number.
    Outbox,
    /// A GitHub write is about to be made (10.9): recorded, under the lock, before the call.
    WriteIntended,
    /// A GitHub write was made, or found already made.
    WriteApplied,
    /// A GitHub write was refused before it was attempted, or failed.
    WriteFailed,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Config => "config",
            Kind::Observed => "observed",
            Kind::ItemGone => "item_gone",
            Kind::Decided => "decided",
            Kind::ViewSaved => "view_saved",
            Kind::Request => "request",
            Kind::Suggested => "suggested",
            Kind::Launch => "launch",
            Kind::Outcome => "outcome",
            Kind::Exported => "exported",
            Kind::Membership => "membership",
            Kind::Outbox => "outbox",
            Kind::WriteIntended => "write_intended",
            Kind::WriteApplied => "write_applied",
            Kind::WriteFailed => "write_failed",
        }
    }

    pub fn parse(s: &str) -> Option<Kind> {
        serde_json::from_value(Value::String(s.to_string())).ok()
    }
}

/// One ledger event, exactly as stored in SQLite and in a backup line.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub seq: i64,
    /// RFC 3339, UTC.
    pub time: String,
    pub kind: Kind,
    /// The node id, launch id or decision id the event concerns.
    pub subject: String,
    /// The sequence number of the subject's previous event of this kind; 0 for the first.
    pub pred: i64,
    /// The fields that make this occurrence distinct (10.3's table).
    pub distinct: Value,
    pub identity: String,
    pub data: Value,
}

/// identity = sha256 of the canonical JSON of {kind, subject, pred, distinct} (10.3).
pub fn identity(kind: Kind, subject: &str, pred: i64, distinct: &Value) -> String {
    hash_value(&json!({
        "kind": kind.as_str(),
        "subject": subject,
        "pred": pred,
        "distinct": distinct,
    }))
}

impl Event {
    /// Recomputes the identity from the stored fields.
    pub fn computed_identity(&self) -> String {
        identity(self.kind, &self.subject, self.pred, &self.distinct)
    }
}
