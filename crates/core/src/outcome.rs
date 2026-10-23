//! A launched session's outcome record (7.7), recorded once per launch.

use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::event::Kind;
use crate::launch;
use crate::ledger::{self, Dedupe, Recorded, Tx};
use crate::{Error, Result};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GateRuns {
    pub level: String,
    pub runs: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecordedBy {
    Session,
    Dispatcher,
}

/// The `data` of an `outcome` event. Its schema is `schemas/event-outcome.schema.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    pub launch: String,
    pub landed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pull_request: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wall_clock_minutes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate_runs: Option<Vec<GateRuns>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub findings_fixed: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_tokens: Option<u64>,
    pub recorded_by: RecordedBy,
    /// The fields the recorder could not supply.
    pub missing: Vec<String>,
}

/// Records the outcome of a known launch. The same record again is a no-op; a different
/// record for a launch that already has one is refused.
pub fn record(tx: &mut Tx<'_>, outcome: &Outcome) -> Result<Recorded> {
    if launch::load(tx.conn(), &outcome.launch)?.is_none() {
        return Err(Error::refused(format!("no launch {}", outcome.launch)));
    }
    if outcome.gate_runs.iter().flatten().any(|g| g.runs == 0) {
        return Err(Error::refused("a gate level with zero runs is not a gate run"));
    }
    let data = serde_json::to_value(outcome)?;
    if let Some(existing) = ledger::latest(tx.conn(), &outcome.launch, Kind::Outcome)? {
        if existing.data == data {
            return Ok(Recorded::Repeat(existing));
        }
        return Err(Error::refused(format!(
            "launch {} already has an outcome (seq {}); a different second one is refused",
            outcome.launch, existing.seq
        )));
    }
    tx.record_valid(Kind::Outcome, &outcome.launch, json!({}), data, Dedupe::Any)
}
