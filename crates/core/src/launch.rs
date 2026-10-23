//! Launch reservations (10.7): a launch moves through 10.7's transitions, each a `launch`
//! event, and only those. Orphans are recorded alongside a launch, never as its state.
//!
//! | From | To | By |
//! |---|---|---|
//! | — | `requested` | ingest, or `reserve --request` ([`request`]) |
//! | `requested` | `reserved` or `queued` | [`reserve`] |
//! | `queued` | `reserved` | [`reserve`] or [`reserve_next`], when a slot frees |
//! | `reserved` | `started` | [`started`] |
//! | `reserved` | `unconfirmed` | [`unconfirm`], by the owning dispatcher |
//! | `reserved`, `unconfirmed` | `released` | [`release`], by the owning dispatcher, after 15 min |
//! | `unconfirmed` | `started` | [`started`] |
//! | `started` | `ended` | [`ended`] |
//! | `ended` | `released` | [`ended`], in the same transaction |
//! | `requested`, `queued` | `withdrawn` | [`withdraw`] |
//!
//! Alongside a launch: `orphan` ([`started`], for a released token or a second session) and
//! `orphan_settled` ([`settle_orphan`]). An unsettled orphan holds a slot of the pool.

use std::collections::BTreeMap;
use std::fmt;

use chrono::Duration;
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::json;

use crate::docs::LaunchRequest;
use crate::event::{Event, Kind};
use crate::ledger::{self, Dedupe, Tx};
use crate::root::parse_time;
use crate::{Error, Result};

/// A session list is evidence of absence only for a reservation older than this (10.7).
pub const SETTLE_AFTER: Duration = Duration::minutes(15);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Transition {
    Requested,
    Queued,
    Reserved,
    Unconfirmed,
    Started,
    Ended,
    Released,
    Withdrawn,
    Orphan,
    OrphanSettled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EndState {
    Landed,
    Stopped,
    Failed,
}

/// Who recorded a `requested` transition.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Via {
    Ingest,
    Reserve,
}

/// The `data` of a `launch` event. Its schema is `schemas/event-launch.schema.json`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LaunchData {
    pub transition: Transition,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub token: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dispatcher: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_state: Option<EndState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<LaunchRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub decision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub via: Option<Via>,
}

impl LaunchData {
    fn new(transition: Transition) -> LaunchData {
        LaunchData {
            transition,
            token: None,
            session: None,
            dispatcher: None,
            end_state: None,
            request: None,
            project: None,
            decision: None,
            via: None,
        }
    }

    fn token(mut self, token: &str) -> Self {
        self.token = Some(token.to_string());
        self
    }

    fn session(mut self, session: &str) -> Self {
        self.session = Some(session.to_string());
        self
    }

    fn dispatcher(mut self, dispatcher: &str) -> Self {
        self.dispatcher = Some(dispatcher.to_string());
        self
    }
}

/// A launch's state, folded from its events (orphans excluded).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Requested,
    Queued,
    Reserved,
    Unconfirmed,
    Started,
    Ended,
    Released,
    Withdrawn,
}

impl State {
    /// A launch in one of these holds its issues and a slot of its recipe's pool (10.7).
    pub fn holds(self) -> bool {
        matches!(self, State::Reserved | State::Unconfirmed | State::Started)
    }
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let v = serde_json::to_value(self).expect("a state serialises");
        f.write_str(v.as_str().expect("a state is a string"))
    }
}

/// An orphan: a session that reported a token it does not own (10.7).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Orphan {
    pub token: String,
    pub session: String,
    pub reported_at: String,
}

/// Everything known about one launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Launch {
    pub id: String,
    pub request: LaunchRequest,
    pub state: State,
    pub token: Option<String>,
    pub dispatcher: Option<String>,
    pub session: Option<String>,
    pub end_state: Option<EndState>,
    pub reserved_at: Option<String>,
    /// The sequence number of the `queued` event, for first-in first-out.
    pub queued_seq: Option<i64>,
    /// Orphans not yet settled; each holds a slot (10.7).
    pub orphans: Vec<Orphan>,
    /// Orphans already settled. A session that reports again after its settlement is a repeat,
    /// not a new orphan, so a retry cannot take the slot back.
    pub settled_orphans: Vec<Orphan>,
    /// The sequence number of the launch's latest event.
    pub last_seq: i64,
}

/// What [`reserve`] decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum Reservation {
    Reserved {
        launch: String,
        token: String,
        repeat: bool,
    },
    Queued {
        launch: String,
        repeat: bool,
    },
}

/// What [`started`] decided.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum StartResult {
    Started {
        launch: String,
        repeat: bool,
    },
    /// The token was released, or another session already started under it; the reporting
    /// session is recorded alongside the launch as possible duplicate work.
    Orphan {
        launch: String,
        repeat: bool,
    },
}

/// The result of a command that moves a launch along, or repeats a move already made.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Moved {
    pub launch: String,
    pub state: State,
    pub repeat: bool,
}

fn moved(l: &Launch, state: State, repeat: bool) -> Moved {
    Moved {
        launch: l.id.clone(),
        state,
        repeat,
    }
}

fn record(tx: &mut Tx<'_>, launch_id: &str, data: LaunchData) -> Result<Event> {
    let mut distinct = json!({"transition": data.transition, "token": data.token});
    if let Some(s) = &data.session {
        distinct["session"] = json!(s);
    }
    let value = serde_json::to_value(&data)?;
    Ok(tx
        .record_valid(Kind::Launch, launch_id, distinct, value, Dedupe::IdentityOnly)?
        .event()
        .clone())
}

fn fold(id: &str, events: &[Event]) -> Result<Option<Launch>> {
    let Some(first) = events.first() else {
        return Ok(None);
    };
    let first_data: LaunchData = serde_json::from_value(first.data.clone())?;
    let request = match (first_data.transition, first_data.request) {
        (Transition::Requested, Some(r)) => r,
        _ => {
            return Err(Error::refused(format!(
                "launch {id}: its first event (seq {}) is not a request",
                first.seq
            )));
        }
    };
    let mut l = Launch {
        id: id.to_string(),
        request,
        state: State::Requested,
        token: None,
        dispatcher: None,
        session: None,
        end_state: None,
        reserved_at: None,
        queued_seq: None,
        orphans: Vec::new(),
        settled_orphans: Vec::new(),
        last_seq: first.seq,
    };
    for e in &events[1..] {
        let d: LaunchData = serde_json::from_value(e.data.clone())?;
        match d.transition {
            Transition::Requested => {}
            Transition::Queued => {
                l.state = State::Queued;
                l.queued_seq = Some(e.seq);
            }
            Transition::Reserved => {
                l.state = State::Reserved;
                l.token = d.token;
                l.dispatcher = d.dispatcher;
                l.reserved_at = Some(e.time.clone());
            }
            Transition::Unconfirmed => l.state = State::Unconfirmed,
            Transition::Started => {
                l.state = State::Started;
                l.session = d.session;
            }
            Transition::Ended => {
                l.state = State::Ended;
                l.end_state = d.end_state;
            }
            Transition::Released => l.state = State::Released,
            Transition::Withdrawn => l.state = State::Withdrawn,
            Transition::Orphan => l.orphans.push(Orphan {
                token: d.token.unwrap_or_default(),
                session: d.session.unwrap_or_default(),
                reported_at: e.time.clone(),
            }),
            Transition::OrphanSettled => {
                let (settled, open): (Vec<Orphan>, Vec<Orphan>) = l
                    .orphans
                    .drain(..)
                    .partition(|o| Some(&o.token) == d.token.as_ref() && Some(&o.session) == d.session.as_ref());
                l.orphans = open;
                l.settled_orphans.extend(settled);
            }
        }
        l.last_seq = e.seq;
    }
    Ok(Some(l))
}

/// One launch, folded from its events.
pub fn load(conn: &Connection, launch_id: &str) -> Result<Option<Launch>> {
    fold(launch_id, &ledger::events_of(conn, launch_id, Kind::Launch)?)
}

/// Every launch, in order of request.
pub fn all(conn: &Connection) -> Result<Vec<Launch>> {
    let mut by_id: BTreeMap<String, Vec<Event>> = BTreeMap::new();
    let mut order: Vec<String> = Vec::new();
    for e in ledger::events_by_kind(conn, Kind::Launch)? {
        if !by_id.contains_key(&e.subject) {
            order.push(e.subject.clone());
        }
        by_id.entry(e.subject.clone()).or_default().push(e);
    }
    let mut out = Vec::new();
    for id in order {
        if let Some(l) = fold(&id, &by_id[&id])? {
            out.push(l);
        }
    }
    Ok(out)
}

fn by_token(conn: &Connection, token: &str) -> Result<Launch> {
    let id: Option<String> = conn
        .query_row(
            "SELECT subject FROM events WHERE kind = 'launch'
               AND json_extract(data, '$.transition') = 'reserved'
               AND json_extract(data, '$.token') = ?1",
            [token],
            |r| r.get(0),
        )
        .map(Some)
        .or_else(|e| {
            if e == rusqlite::Error::QueryReturnedNoRows {
                Ok(None)
            } else {
                Err(e)
            }
        })?;
    let id = id.ok_or_else(|| Error::refused(format!("no reservation has token {token}")))?;
    load(conn, &id)?.ok_or_else(|| Error::refused(format!("launch {id} has no request")))
}

fn new_token() -> String {
    format!("res-{}", uuid::Uuid::new_v4().simple())
}

fn check_owner(l: &Launch, dispatcher: &str) -> Result<()> {
    match &l.dispatcher {
        Some(owner) if owner == dispatcher => Ok(()),
        Some(owner) => Err(Error::refused(format!(
            "launch {} is held by dispatcher {owner}; only that dispatcher settles it (10.7)",
            l.id
        ))),
        None => Err(Error::refused(format!("launch {} has no reservation", l.id))),
    }
}

/// Records a `requested` transition for a page launch document. A document whose id is
/// already requested is a repeat if identical, and refused if different (launch documents
/// are never changed, 5.3).
pub fn request(tx: &mut Tx<'_>, doc: &LaunchRequest, via: Via) -> Result<Moved> {
    if let Some(l) = load(tx.conn(), &doc.id)? {
        if &l.request == doc {
            return Ok(moved(&l, l.state, true));
        }
        return Err(Error::refused(format!(
            "launch {} is already requested with a different document; launch documents are never changed",
            doc.id
        )));
    }
    if doc.items.is_empty() {
        return Err(Error::refused(format!("launch {} names no items", doc.id)));
    }
    let mut data = LaunchData::new(Transition::Requested);
    data.request = Some(doc.clone());
    data.project = Some(doc.project.clone());
    data.via = Some(via);
    record(tx, &doc.id, data)?;
    Ok(Moved {
        launch: doc.id.clone(),
        state: State::Requested,
        repeat: false,
    })
}

/// An item of `l` held by another launch, with that launch. A launch whose kind holds no
/// items (a read-only recommender) neither blocks nor is blocked.
fn held_item<'a>(l: &'a Launch, all: &'a [Launch]) -> Option<(&'a str, &'a Launch)> {
    if !l.request.kind.holds_items() {
        return None;
    }
    let mut holders = all
        .iter()
        .filter(|o| o.id != l.id && o.state.holds() && o.request.kind.holds_items());
    holders.find_map(|o| {
        l.request
            .items
            .iter()
            .find(|i| o.request.items.contains(i))
            .map(|i| (i.as_str(), o))
    })
}

/// `reserve` (10.7): in one transaction across every project, checks that no launch holding
/// a slot covers any of this launch's items, then that the recipe's pool has a free slot and
/// no earlier queued launch is waiting for it. Returns a token, or `queued`; a held issue is
/// refused. `capacity` gives a recipe's pool size.
pub fn reserve(
    tx: &mut Tx<'_>,
    launch_id: &str,
    dispatcher: &str,
    capacity: &dyn Fn(&str) -> Result<u32>,
) -> Result<Reservation> {
    let l = load(tx.conn(), launch_id)?.ok_or_else(|| {
        Error::refused(format!(
            "no launch {launch_id}; reserve --request <file> records its document first"
        ))
    })?;
    match l.state {
        State::Requested | State::Queued => {}
        State::Reserved | State::Unconfirmed | State::Started => {
            check_owner(&l, dispatcher)?;
            return Ok(Reservation::Reserved {
                launch: l.id.clone(),
                token: l.token.clone().expect("a held launch has a token"),
                repeat: true,
            });
        }
        State::Ended | State::Released | State::Withdrawn => {
            return Err(Error::refused(format!(
                "launch {launch_id} is {}; it cannot be reserved",
                l.state
            )));
        }
    }
    let all = all(tx.conn())?;
    try_reserve(tx, &l, dispatcher, capacity, &all)
}

fn try_reserve(
    tx: &mut Tx<'_>,
    l: &Launch,
    dispatcher: &str,
    capacity: &dyn Fn(&str) -> Result<u32>,
    all: &[Launch],
) -> Result<Reservation> {
    if let Some((item, other)) = held_item(l, all) {
        let alias = l
            .request
            .items
            .iter()
            .position(|i| i == item)
            .and_then(|p| l.request.aliases.get(p))
            .map_or(item.to_string(), |a| format!("{a} ({item})"));
        return Err(Error::refused(format!(
            "{alias} is held by launch {} ({}) in project {}",
            other.id, other.state, other.request.project
        )));
    }
    let recipe = &l.request.recipe;
    let cap = capacity(recipe)? as usize;
    let same_pool = || all.iter().filter(|o| &o.request.recipe == recipe && o.id != l.id);
    let held = same_pool().filter(|o| o.state.holds()).count()
        + all
            .iter()
            .filter(|o| &o.request.recipe == recipe)
            .map(|o| o.orphans.len())
            .sum::<usize>();
    // Queued launches that come before this one and could take a slot now.
    let ahead = same_pool()
        .filter(|o| o.state == State::Queued && held_item(o, all).is_none())
        .filter(|o| match (l.state, l.queued_seq, o.queued_seq) {
            (State::Queued, Some(mine), Some(theirs)) => theirs < mine,
            _ => true,
        })
        .count();
    if held + ahead >= cap {
        if l.state == State::Queued {
            return Ok(Reservation::Queued {
                launch: l.id.clone(),
                repeat: true,
            });
        }
        record(tx, &l.id, LaunchData::new(Transition::Queued).dispatcher(dispatcher))?;
        return Ok(Reservation::Queued {
            launch: l.id.clone(),
            repeat: false,
        });
    }
    let token = new_token();
    record(
        tx,
        &l.id,
        LaunchData::new(Transition::Reserved)
            .token(&token)
            .dispatcher(dispatcher),
    )?;
    Ok(Reservation::Reserved {
        launch: l.id.clone(),
        token,
        repeat: false,
    })
}

/// Reserves the oldest queued launch that can take a slot now, if any.
pub fn reserve_next(
    tx: &mut Tx<'_>,
    dispatcher: &str,
    capacity: &dyn Fn(&str) -> Result<u32>,
) -> Result<Option<Reservation>> {
    let all = all(tx.conn())?;
    let mut queued: Vec<&Launch> = all.iter().filter(|l| l.state == State::Queued).collect();
    queued.sort_by_key(|l| l.queued_seq);
    let mut first_error = None;
    for l in queued {
        if held_item(l, &all).is_some() {
            continue;
        }
        // A launch whose recipe can no longer be resolved must not stop other recipes' queues;
        // that refusal is returned only when nothing could be reserved. It is resolved before
        // anything is recorded, and any other error propagates.
        let cap = match capacity(&l.request.recipe) {
            Ok(cap) => cap,
            Err(e) => {
                first_error.get_or_insert(e);
                continue;
            }
        };
        if let r @ Reservation::Reserved { .. } = try_reserve(tx, l, dispatcher, &|_| Ok(cap), &all)? {
            return Ok(Some(r));
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(None),
    }
}

/// `launch-started` (10.7). Idempotent for a token and session already recorded; a released
/// token, or a different session for a started token, is recorded as an orphan.
pub fn started(tx: &mut Tx<'_>, token: &str, session: &str) -> Result<StartResult> {
    let l = by_token(tx.conn(), token)?;
    if l.session.as_deref() == Some(session) {
        return Ok(StartResult::Started {
            launch: l.id,
            repeat: true,
        });
    }
    match l.state {
        State::Reserved | State::Unconfirmed => {
            record(
                tx,
                &l.id,
                LaunchData::new(Transition::Started).token(token).session(session),
            )?;
            Ok(StartResult::Started {
                launch: l.id,
                repeat: false,
            })
        }
        State::Started | State::Ended | State::Released => {
            let reported = |o: &Orphan| o.token == token && o.session == session;
            if l.orphans.iter().any(reported) || l.settled_orphans.iter().any(reported) {
                return Ok(StartResult::Orphan {
                    launch: l.id,
                    repeat: true,
                });
            }
            record(
                tx,
                &l.id,
                LaunchData::new(Transition::Orphan).token(token).session(session),
            )?;
            Ok(StartResult::Orphan {
                launch: l.id,
                repeat: false,
            })
        }
        State::Requested | State::Queued | State::Withdrawn => Err(Error::refused(format!(
            "launch {} is {}; it has no live token",
            l.id, l.state
        ))),
    }
}

/// The owning dispatcher, on restart, when its start call never returned.
pub fn unconfirm(tx: &mut Tx<'_>, token: &str, dispatcher: &str) -> Result<Moved> {
    let l = by_token(tx.conn(), token)?;
    match l.state {
        State::Reserved => {
            check_owner(&l, dispatcher)?;
            record(
                tx,
                &l.id,
                LaunchData::new(Transition::Unconfirmed)
                    .token(token)
                    .dispatcher(dispatcher),
            )?;
            Ok(moved(&l, State::Unconfirmed, false))
        }
        State::Unconfirmed => {
            check_owner(&l, dispatcher)?;
            Ok(moved(&l, State::Unconfirmed, true))
        }
        other => Err(Error::refused(format!(
            "launch {} is {other}; only a reserved launch becomes unconfirmed",
            l.id
        ))),
    }
}

/// `launch-ended`: `started` → `ended` → `released`, in one transaction.
pub fn ended(tx: &mut Tx<'_>, token: &str, end_state: EndState) -> Result<Moved> {
    let l = by_token(tx.conn(), token)?;
    match (l.state, l.end_state) {
        (State::Started, _) => {
            let mut data = LaunchData::new(Transition::Ended).token(token);
            data.end_state = Some(end_state);
            record(tx, &l.id, data)?;
            record(tx, &l.id, LaunchData::new(Transition::Released).token(token))?;
            Ok(moved(&l, State::Released, false))
        }
        (State::Released, Some(recorded)) if recorded == end_state => Ok(moved(&l, State::Released, true)),
        (State::Released, Some(recorded)) => Err(Error::refused(format!(
            "launch {} already ended as {}",
            l.id,
            serde_json::to_value(recorded)?.as_str().unwrap_or_default()
        ))),
        (other, _) => Err(Error::refused(format!(
            "launch {} is {other}; only a started launch can end",
            l.id
        ))),
    }
}

/// `release`: the owning dispatcher frees a `reserved` or `unconfirmed` reservation whose
/// session it cannot see, once the reservation is older than [`SETTLE_AFTER`].
pub fn release(tx: &mut Tx<'_>, token: &str, dispatcher: &str) -> Result<Moved> {
    let l = by_token(tx.conn(), token)?;
    match l.state {
        State::Reserved | State::Unconfirmed => {
            check_owner(&l, dispatcher)?;
            let reserved_at = parse_time(l.reserved_at.as_deref().expect("a reserved launch has a time"))?;
            let age = parse_time(tx.now())? - reserved_at;
            if age <= SETTLE_AFTER {
                return Err(Error::refused(format!(
                    "launch {} was reserved {} s ago; a session list is evidence of absence only after 15 minutes",
                    l.id,
                    age.num_seconds()
                )));
            }
            record(
                tx,
                &l.id,
                LaunchData::new(Transition::Released)
                    .token(token)
                    .dispatcher(dispatcher),
            )?;
            Ok(moved(&l, State::Released, false))
        }
        State::Released => Ok(moved(&l, State::Released, true)),
        other => Err(Error::refused(format!(
            "launch {} is {other}; release frees only a reserved or unconfirmed launch (a started one ends with launch-ended)",
            l.id
        ))),
    }
}

/// A page decision cancelling a launch that has not been reserved.
pub fn withdraw(tx: &mut Tx<'_>, launch_id: &str, decision: &str) -> Result<Moved> {
    let l = load(tx.conn(), launch_id)?.ok_or_else(|| Error::refused(format!("no launch {launch_id}")))?;
    match l.state {
        State::Requested | State::Queued => {
            let mut data = LaunchData::new(Transition::Withdrawn);
            data.decision = Some(decision.to_string());
            record(tx, launch_id, data)?;
            Ok(moved(&l, State::Withdrawn, false))
        }
        State::Withdrawn => Ok(moved(&l, State::Withdrawn, true)),
        other => Err(Error::refused(format!(
            "launch {launch_id} is {other}; only a requested or queued launch can be withdrawn"
        ))),
    }
}

/// Settles an orphan, freeing the slot it held.
pub fn settle_orphan(tx: &mut Tx<'_>, token: &str, session: &str) -> Result<Moved> {
    let l = by_token(tx.conn(), token)?;
    if l.settled_orphans
        .iter()
        .any(|o| o.token == token && o.session == session)
    {
        return Ok(moved(&l, l.state, true));
    }
    if !l.orphans.iter().any(|o| o.token == token && o.session == session) {
        return Err(Error::refused(format!(
            "launch {} has no unsettled orphan for session {session}",
            l.id
        )));
    }
    record(
        tx,
        &l.id,
        LaunchData::new(Transition::OrphanSettled).token(token).session(session),
    )?;
    Ok(moved(&l, l.state, false))
}
