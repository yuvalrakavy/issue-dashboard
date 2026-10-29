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
    /// A read-only session that recommends which issues to close, and which together (5.8).
    RecommendClosures,
}

impl LaunchKind {
    /// Whether a launch of this kind holds its items against other launches (10.7). A
    /// read-only recommender changes nothing, so it neither holds an item nor waits for one.
    pub fn holds_items(self) -> bool {
        self != LaunchKind::RecommendClosures
    }
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
    /// Close an issue on GitHub (10.0's exception, 10.10).
    Close,
    /// Stop showing a closed item: it leaves every list of the page, while the ledger and the
    /// metrics keep it (5.8). It holds for the close it names (`value.closed_at`) only.
    DismissClosed,
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
    /// The card an accept or edit was made on (triage spec 4.6): a combined card's id, or the
    /// suggestion's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub card: Option<String>,
    /// Which part of the card: a combined card's `type` or `priority` (4.6).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub component: Option<String>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close: Option<CloseValue>,
    /// The close a `dismiss_closed` dismisses: the item's `closed_at` as the page showed it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at: Option<String>,
    /// An accept made by apply all (triage spec 4.3): `owner_bulk`, never ground truth (3.1).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bulk: Option<bool>,
    /// A Details edit (triage spec 4.2): stored for ordering, never a label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub severity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reach: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detectability: Option<String>,
}

/// Why an issue is closed: GitHub's `state_reason`, as `gh issue close --reason` takes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseReason {
    Completed,
    NotPlanned,
    Duplicate,
}

impl CloseReason {
    /// The value `gh issue close --reason` takes.
    pub fn gh(self) -> &'static str {
        match self {
            CloseReason::Completed => "completed",
            CloseReason::NotPlanned => "not planned",
            CloseReason::Duplicate => "duplicate",
        }
    }
}

/// What a `close` decision asks for: the reason, the issue a duplicate duplicates, and the
/// comment to post with the close (empty: none).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct CloseValue {
    pub reason: CloseReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duplicate_of: Option<u64>,
    pub comment: String,
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
