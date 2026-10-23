//! Page documents the ledger ingests (5.3), as Rust types. Their JSON Schemas are
//! `schemas/page-*.schema.json`; the test suite checks every example deserialises here.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Priority {
    P0,
    P1,
    P2,
    P3,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Routing {
    pub model: String,
    pub effort: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LaunchKind {
    Fix,
    Batch,
    Review,
    Triage,
    SendUpstream,
    GoBack,
    StrongerLook,
    Design,
}

/// A `launches` document asking for a launch (5.4). Written once, never changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchRequest {
    pub id: String,
    pub project: String,
    pub author: String,
    pub created_at: String,
    pub kind: LaunchKind,
    /// Node ids: one issue, or a batch's members.
    pub items: Vec<String>,
    pub aliases: Vec<String>,
    /// The recipe (7.6) whose pool the launch takes a slot from.
    pub recipe: String,
    pub routing: Routing,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_routing: Option<Routing>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    AcceptSuggestion,
    DismissSuggestion,
    EditSuggestion,
    SetPriority,
    ClearPriority,
    SetOverride,
    ClearOverride,
    KeepOverride,
    AcceptGithubChange,
    IgnoreGithubChange,
    ApprovePlan,
    Undo,
    CancelLaunch,
    ReleaseReservation,
    SettleOrphan,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Target {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub item: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggestion: Option<String>,
    /// The decision an undo names.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub launch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DecisionValue {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub priority: Option<Priority>,
    #[serde(default, rename = "type", skip_serializing_if = "Option::is_none")]
    pub type_: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub areas: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub routing: Option<Routing>,
}

/// A `decisions` document: one thing the owner did on the dashboard (5.3, 10.5).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Decision {
    pub id: String,
    pub project: String,
    /// The page's user id of whoever wrote the document; only the project's `owner_id`
    /// counts (10.5).
    pub author: String,
    pub created_at: String,
    pub action: Action,
    pub target: Target,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub value: Option<DecisionValue>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}
