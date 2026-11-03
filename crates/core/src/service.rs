//! The bridge runner's service (runner spec 1.1-1.3, 3.4, 4.3): its setup file, its lock, the
//! history refreshes it runs off the loop, and the loop's one tick. The process loop around
//! the connector child is the CLI's `bridge-serve`; everything it decides is here, over the
//! [`bridge::Bridge`] seam, so the tests drive it against the fake.

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::bridge::{self, Pin};
use crate::docs::LaunchKind;
use crate::launch::{self, State};
use crate::lock::RootLock;
use crate::runner::{self, Ctx, Stepped};
use crate::{Error, Result, Root, runner_status};

/// The setup `runner init` writes and `runner pin` completes, beside the ledger.
pub const SETUP_FILE: &str = "runner.json";
/// Held for the service's whole life (1.1).
pub const LOCK_FILE: &str = "runner.lock";
/// The longest the loop sleeps in `wait` before it looks for newly queued launches (4.3).
pub const POLL_MS: u64 = 5_000;
/// How long after a failed history refresh the next is tried.
pub const REFRESH_RETRY_SECONDS: i64 = 300;

/// What the runner runs against (1.2).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Setup {
    /// The `agentbridge-dashboard` executable.
    pub connector: PathBuf,
    /// Its `--config`, absolute.
    pub config: PathBuf,
    /// The connector's private `state_dir`: AgentBridge stages only files under its `files/`.
    pub state_dir: PathBuf,
    /// What `runner pin` recorded; the service refuses to start without one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pin: Option<Pin>,
    /// Why the service stopped on a rollback (3.4); it refuses to start until
    /// `runner pin --accept-rollback` clears it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rollback: Option<String>,
}

impl Setup {
    pub fn files(&self) -> PathBuf {
        self.state_dir.join("files")
    }

    /// The pin to start against: refused while a rollback stands or before `runner pin`.
    pub fn startable(&self) -> Result<&Pin> {
        if let Some(why) = &self.rollback {
            return Err(Error::refused(format!(
                "the runner stopped on a rollback ({why}); after checking AgentBridge's state, run \
                 `runner pin --accept-rollback`"
            )));
        }
        self.pin
            .as_ref()
            .ok_or_else(|| Error::refused("the runner is not pinned: qualify the connector, then run `runner pin`"))
    }
}

pub fn load(dir: &Path) -> Result<Setup> {
    let path = dir.join(SETUP_FILE);
    match fs::read_to_string(&path) {
        Ok(text) => Ok(serde_json::from_str(&text)?),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Err(Error::refused(format!(
            "no {}: run `runner init` first",
            path.display()
        ))),
        Err(e) => Err(e.into()),
    }
}

/// Writes the setup whole, by rename.
pub fn save(dir: &Path, setup: &Setup) -> Result<()> {
    let path = dir.join(SETUP_FILE);
    let tmp = dir.join(format!("{SETUP_FILE}.tmp"));
    fs::write(&tmp, serde_json::to_string_pretty(setup)? + "\n")?;
    fs::rename(&tmp, &path)?;
    Ok(())
}

/// The connector state `--init` would create. AgentBridge reopens existing state even under
/// `--init` (`DASHBOARD-RUNNER.md`), so refusing to initialize over it is the runner's job.
const CONNECTOR_STATE: &[&str] = &["dashboard.redb", "transport/journal.redb"];

/// `runner init`'s refusals: an existing setup, or any connector state already in `state_dir`.
pub fn check_init(dir: &Path, state_dir: &Path) -> Result<()> {
    if dir.join(SETUP_FILE).exists() {
        return Err(Error::refused(format!(
            "{} exists: the runner is already set up",
            dir.join(SETUP_FILE).display()
        )));
    }
    for f in CONNECTOR_STATE {
        let p = state_dir.join(f);
        if p.exists() {
            return Err(Error::refused(format!(
                "{} exists: initializing would reuse an identity; restore or choose a new state_dir",
                p.display()
            )));
        }
    }
    Ok(())
}

/// `runner pin`: records `seen` as the pin. Refused while a rollback stands, unless
/// `accept_rollback`, which also clears it.
pub fn pin(dir: &Path, seen: Pin, accept_rollback: bool) -> Result<Setup> {
    let mut setup = load(dir)?;
    match (&setup.rollback, accept_rollback) {
        (Some(why), false) => {
            return Err(Error::refused(format!(
                "the runner stopped on a rollback ({why}); pin with --accept-rollback once AgentBridge's \
                 state is checked"
            )));
        }
        (None, true) => return Err(Error::refused("there is no rollback to accept")),
        _ => {}
    }
    setup.pin = Some(seen);
    setup.rollback = None;
    save(dir, &setup)?;
    Ok(setup)
}

/// The service's lock (1.1): a second instance is refused at once, naming the holder's pid.
pub fn lock(dir: &Path) -> Result<RootLock> {
    RootLock::acquire(&dir.join(LOCK_FILE), "bridge-serve", Duration::ZERO)
}

/// One history refresh: a project in, done or why not.
pub type Refresh = Arc<dyn Fn(&str) -> Result<()> + Send + Sync>;

/// The history refreshes a reserved eval waits for before it starts (D1, 4.3): each runs on its
/// own thread and posts its completion back, so a long `gh` read never stalls the loop. Held
/// in memory only: after a restart a reserved eval is refreshed again, which resumes.
pub struct Refresher {
    run: Refresh,
    in_flight: HashSet<String>,
    done: HashMap<String, std::result::Result<(), (String, DateTime<Utc>)>>,
    tx: mpsc::Sender<(String, std::result::Result<(), String>)>,
    rx: mpsc::Receiver<(String, std::result::Result<(), String>)>,
}

impl Refresher {
    pub fn new(run: Refresh) -> Refresher {
        let (tx, rx) = mpsc::channel();
        Refresher {
            run,
            in_flight: HashSet::new(),
            done: HashMap::new(),
            tx,
            rx,
        }
    }

    fn collect(&mut self, now: DateTime<Utc>) {
        while let Ok((token, r)) = self.rx.try_recv() {
            self.in_flight.remove(&token);
            self.done.insert(token, r.map_err(|e| (e, now)));
        }
    }

    /// Whether `token`'s refresh of `project` is done: `Ok(true)` once it succeeded, `Ok(false)`
    /// while one runs (one is started if none has), `Err` with why the last failed.
    fn ready(&mut self, token: &str, project: &str, now: DateTime<Utc>) -> std::result::Result<bool, String> {
        match self.done.get(token) {
            Some(Ok(())) => return Ok(true),
            Some(Err((e, at))) if (now - *at).num_seconds() < REFRESH_RETRY_SECONDS => return Err(e.clone()),
            _ => {}
        }
        if self.in_flight.insert(token.to_string()) {
            self.done.remove(token);
            let (run, tx, token, project) = (self.run.clone(), self.tx.clone(), token.to_string(), project.to_string());
            thread::spawn(move || {
                let r = run(&project).map_err(|e| e.to_string());
                let _ = tx.send((token, r));
            });
        }
        Ok(false)
    }

    /// Waits up to `bound` for every refresh in flight to post its completion.
    pub fn settle(&mut self, bound: Duration, now: DateTime<Utc>) {
        let deadline = std::time::Instant::now() + bound;
        while !self.in_flight.is_empty() {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            match self.rx.recv_timeout(left) {
                Ok((token, r)) => {
                    self.in_flight.remove(&token);
                    self.done.insert(token, r.map_err(|e| (e, now)));
                }
                Err(_) => return,
            }
        }
    }
}

/// What one tick did.
#[derive(Debug, Default)]
pub struct Tick {
    /// The health read, per harness, for `runner_status`.
    pub health: Value,
    /// Tokens reserved by admission.
    pub admitted: Vec<String>,
    /// What each held runner launch's step did, by token; a step's error is that launch's
    /// alone.
    pub stepped: Vec<(String, std::result::Result<Stepped, String>)>,
    /// Set when a step found a rollback (3.4): the service stops without stepping further.
    pub rollback: Option<String>,
}

/// One tick of the loop (4.3): reads the health of every harness a runner launch uses, runs
/// admission, then steps each launch the runner holds. A reserved eval is not started until
/// its history refresh is done (D1); one whose refresh failed waits with that reason.
pub fn tick(cx: &Ctx<'_>, refresher: &mut Refresher) -> Result<Tick> {
    let now = (cx.now)();
    refresher.collect(now);
    let harnesses = runner::harnesses(cx.root)?;
    let health = if harnesses.is_empty() {
        HashMap::new()
    } else {
        bridge::health(cx.bridge, &harnesses)?
    };
    let mut t = Tick {
        health: json!(health),
        admitted: runner::admit(cx, &health)?,
        ..Tick::default()
    };
    let all = cx.root.read(|l| launch::all(l.conn()))?;
    forget_stale_reasons(cx.root, &all, now)?;
    for l in all.iter().filter(|l| l.request.routing.harness.is_some() && l.state.holds()) {
        let Some(token) = l.token.clone() else { continue };
        if l.state == State::Reserved && l.request.kind == LaunchKind::Eval {
            match refresher.ready(&token, &l.request.project, now) {
                Ok(true) => cx.root.mutate(now, |tx| runner_status::unhold(tx.conn(), &l.id))?,
                Ok(false) => continue,
                Err(e) => {
                    let why = format!("the history refresh of {} failed: {e}", l.request.project);
                    cx.root.mutate(now, |tx| runner_status::hold(tx.conn(), &l.id, &why, now))?;
                    continue;
                }
            }
        }
        match runner::step(cx, &token) {
            Ok(Stepped::Rollback(why)) => {
                t.rollback = Some(why);
                return Ok(t);
            }
            Ok(s) => t.stepped.push((token, Ok(s))),
            Err(e) => t.stepped.push((token, Err(e.to_string()))),
        }
    }
    Ok(t)
}

/// Drops the waiting reason of every launch that neither waits nor holds any more (it was
/// withdrawn or ended), so the page never shows a reason for a launch gone.
fn forget_stale_reasons(root: &Root, all: &[launch::Launch], now: DateTime<Utc>) -> Result<()> {
    let live = |id: &str| {
        all.iter()
            .any(|l| l.id == id && (l.state.holds() || matches!(l.state, State::Requested | State::Queued)))
    };
    let stale: Vec<String> = root
        .read(|l| runner_status::held(l.conn()))?
        .into_iter()
        .map(|h| h.launch)
        .filter(|id| !live(id))
        .collect();
    if stale.is_empty() {
        return Ok(());
    }
    root.mutate(now, |tx| {
        for id in &stale {
            runner_status::unhold(tx.conn(), id)?;
        }
        Ok(())
    })
}

/// The largest event `sequence` in `events`, if any: the next `wait`'s `after`.
pub fn cursor(events: &[Value]) -> Option<i64> {
    events.iter().filter_map(|e| e["sequence"].as_i64()).max()
}
