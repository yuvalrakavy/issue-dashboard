//! The daily triage run (triage spec 5.2, owner option A): `triage-request` records a `triage`
//! launch request authored by `schedule`, naming the items 2.2 selects, at the project's
//! configured routing. A scheduled task cannot pin its model, so it never answers a question:
//! it records the request and wakes the dispatcher, which reserves it as it reserves the
//! owner's and starts the session at the routing; that session's `prepare --session-model`
//! refuses any other model.
//!
//! The request is recorded straight into the ledger, as `sync-now` records its request: it is
//! not a page document, so the owner check of ingest does not apply (4.6). `reserve` accepts a
//! `schedule` author for a `triage` launch only ([`launch::check_author`]).

use serde::Serialize;

use crate::canonical::hash_value;
use crate::config::{ConfigSet, MAX_TRIAGE_ITEMS, READ_ONLY_RECIPE};
use crate::docs::{LaunchKind, LaunchRequest, Routing, TriageScope};
use crate::launch::{self, SCHEDULE_AUTHOR, Via};
use crate::ledger::Tx;
use crate::root::parse_time;
use crate::triage;
use crate::{Error, Result};

/// What `triage-request` did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum TriageRequested {
    /// A launch request was recorded; the dispatcher reserves it when woken.
    Requested {
        project: String,
        launch: String,
        aliases: Vec<String>,
        routing: Routing,
        /// The same request was already recorded (the same items at the same instant).
        repeat: bool,
    },
    /// An earlier daily request is still `requested`: no dispatcher has taken it. Nothing new is
    /// recorded; the caller wakes the dispatcher for this one again (5.2 "pending").
    Pending {
        project: String,
        launch: String,
        aliases: Vec<String>,
        routing: Routing,
    },
    /// No item has a question due: nothing was recorded.
    NothingToTriage { project: String },
}

/// `triage-request --project P` (5.2): refused while a triage launch of `project` is live,
/// except an earlier daily request no dispatcher took, which is returned as pending; with
/// nothing eligible, records nothing.
pub fn request(tx: &mut Tx<'_>, config: &ConfigSet, project: &str) -> Result<TriageRequested> {
    let p = config.project(project)?;
    let now = tx.now().to_string();
    if let Some(l) = launch::live_triage(tx.conn(), project, &now)? {
        if l.state == launch::State::Requested && l.request.author == SCHEDULE_AUTHOR {
            return Ok(TriageRequested::Pending {
                project: project.to_string(),
                launch: l.id,
                aliases: l.request.aliases,
                routing: l.request.routing,
            });
        }
        return Err(Error::refused(format!(
            "triage launch {} of project {project} is live ({}); the daily run requests nothing until it ends",
            l.id, l.state
        )));
    }
    let routing = config.routing(project, "triage")?;
    let max = p.max_triage_items.unwrap_or(MAX_TRIAGE_ITEMS) as usize;
    let mut picked = triage::select(tx.conn(), config, project, TriageScope::Full, &now)?;
    picked.truncate(max);
    if picked.is_empty() {
        return Ok(TriageRequested::NothingToTriage {
            project: project.to_string(),
        });
    }
    let items: Vec<String> = picked.iter().map(|c| c.item.clone()).collect();
    let aliases: Vec<String> = picked.iter().map(|c| c.alias.clone()).collect();
    let at = parse_time(&now)?;
    let id = format!(
        "L_{}_{}",
        at.format("%Y%m%d_%H%M%S"),
        &hash_value(&serde_json::json!({"project": project, "items": items, "created_at": now}))[..8]
    );
    let doc = LaunchRequest {
        id: id.clone(),
        project: project.to_string(),
        author: SCHEDULE_AUTHOR.to_string(),
        created_at: now,
        kind: LaunchKind::Triage,
        items,
        aliases: aliases.clone(),
        recipe: READ_ONLY_RECIPE.to_string(),
        routing: routing.clone(),
        suggested_routing: None,
        scope: Some(TriageScope::Full),
    };
    let moved = launch::request(tx, &doc, Via::Schedule)?;
    Ok(TriageRequested::Requested {
        project: project.to_string(),
        launch: id,
        aliases,
        routing,
        repeat: moved.repeat,
    })
}
