//! `issue-dashboard`: the CLI (spec chapter 8). Phase 1 builds the commands that touch only
//! the ledger; each prints its result as JSON on stdout, and an error on stderr with the exit
//! code of chapter 8 (1 refused by a check, 2 a file or environment error).

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::Duration;

use anyhow::Context;
use chrono::{DateTime, Utc};
use clap::{Args, Parser, Subcommand, ValueEnum};
use issue_dashboard_core::backup::Failpoint;
use issue_dashboard_core::brief;
use issue_dashboard_core::config::ConfigSet;
use issue_dashboard_core::cycle;
use issue_dashboard_core::docs::LaunchRequest;
use issue_dashboard_core::eval;
use issue_dashboard_core::github::{GhCli, GitHub, Recorder, Replay};
use issue_dashboard_core::history;
use issue_dashboard_core::ingest::{self, Pages};
use issue_dashboard_core::launch::{self, EndState, Via};
use issue_dashboard_core::outcome::{self, GateRuns, Outcome, RecordedBy};
use issue_dashboard_core::publish::{self, Active};
use issue_dashboard_core::read;
use issue_dashboard_core::root::{Options, parse_time};
use issue_dashboard_core::sessions::SessionList;
use issue_dashboard_core::suggest;
use issue_dashboard_core::sync;
use issue_dashboard_core::triage;
use issue_dashboard_core::truth;
use issue_dashboard_core::write::{GhIssues, IssueWriter};
use issue_dashboard_core::{Error, Kind, Root, ledger};
use serde_json::{Value, json};

/// Test seams, read from the environment and not shown in --help: a fixed clock, a simulated
/// crash point, and a shorter lock wait.
const ENV_NOW: &str = "ISSUE_DASHBOARD_NOW";
const ENV_FAILPOINT: &str = "ISSUE_DASHBOARD_FAILPOINT";
const ENV_LOCK_WAIT_MS: &str = "ISSUE_DASHBOARD_LOCK_WAIT_MS";
/// GitHub seams (2.7): serve recorded responses from a directory instead of calling `gh`, or
/// call `gh` and record every exchange into one; and the page size of every paginated read.
const ENV_REPLAY: &str = "ISSUE_DASHBOARD_REPLAY";
const ENV_RECORD: &str = "ISSUE_DASHBOARD_RECORD";
const ENV_PAGE_SIZE: &str = "ISSUE_DASHBOARD_PAGE_SIZE";

#[derive(Parser)]
#[command(
    name = "issue-dashboard",
    version,
    about = "A config-driven issue tracker for a project made of many GitHub repos"
)]
struct Cli {
    /// The tracker root.
    #[arg(long, env = "ISSUE_DASHBOARD_ROOT", global = true)]
    root: Option<PathBuf>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Recreate the SQLite ledger from the JSON Lines backup, verifying every line (10.2).
    Rebuild {
        /// Replace an existing ledger.sqlite (it is kept beside, renamed, with its WAL).
        #[arg(long)]
        force: bool,
    },
    /// Reserve a slot for a launch, or queue it (10.7).
    Reserve(ReserveArgs),
    /// Record that the session for a reservation started (10.7).
    LaunchStarted {
        #[arg(long)]
        token: String,
        #[arg(long)]
        session: String,
    },
    /// Record that a started session ended, and free its slot (10.7).
    LaunchEnded {
        #[arg(long)]
        token: String,
        #[arg(long, value_enum)]
        state: EndStateArg,
    },
    /// Free a reservation whose session the owning dispatcher cannot see (10.7), or settle
    /// an orphan with --orphan-session.
    Release {
        #[arg(long)]
        token: String,
        /// The dispatcher session that owns the reservation.
        #[arg(long, required_unless_present = "orphan_session")]
        dispatcher: Option<String>,
        /// Settle the orphan this session reported, freeing the slot it holds.
        #[arg(long, conflicts_with = "dispatcher")]
        orphan_session: Option<String>,
    },
    /// Mark a reservation unconfirmed: the owning dispatcher restarted and its start call
    /// never returned (10.7).
    Unconfirm {
        #[arg(long)]
        token: String,
        #[arg(long)]
        dispatcher: String,
    },
    /// Record a launched session's outcome (7.7).
    RecordOutcome(OutcomeArgs),
    /// The brief of a launch (7.5): its session title (with the token once reserved), its
    /// text, and every piece of the text with its source.
    Brief {
        #[arg(long)]
        launch: String,
    },
    /// Prepare a triage launch's blinded questions (triage spec 2.1) into a JSON file, and
    /// record the `prepared` event that freezes them.
    Prepare {
        #[arg(long)]
        launch: String,
        /// The calling session's model and effort, as model@effort; refused unless it is the
        /// launch's routing.
        #[arg(long)]
        session_model: String,
        #[arg(long)]
        out: PathBuf,
        /// `priority` prepares stage 2 from the recorded stage-1 answers.
        #[arg(long, value_enum, default_value = "first")]
        stage: StageArg,
    },
    /// Record a launch's answers, estimates and suggestions (5.8, triage spec 2.1) from a JSON
    /// file; the whole file is refused if any entry is.
    RecordSuggestions {
        #[arg(long)]
        launch: String,
        file: PathBuf,
    },
    /// The launches the ledger knows, for the dispatcher to settle against its session list.
    Launches {
        /// Only those holding a slot (reserved, unconfirmed or started) or queued.
        #[arg(long)]
        live: bool,
        /// Only those recorded and not yet reserved or queued: the daily triage run's request
        /// waits here until the dispatcher reserves it (triage spec 5.2).
        #[arg(long, conflicts_with = "live")]
        requested: bool,
    },
    /// The daily triage run (triage spec 5.2): record a `triage` launch request authored by
    /// `schedule`, naming the items due, at the project's configured routing. Refused while a
    /// triage launch of the project is live; with nothing due it records nothing.
    TriageRequest {
        #[arg(long)]
        project: String,
    },
    /// One cycle of a project (10.4): ingest the page documents, read GitHub, and write the
    /// project's outbox.
    Cycle {
        #[arg(long)]
        project: String,
        /// The page documents the sync job fetched, as JSON
        /// {"decisions": [], "launches": [], "views": [], "requests": []}.
        #[arg(long)]
        pages: Option<PathBuf>,
        /// The app's session list the sync job wrote, as JSON
        /// {"listed_at": "...", "limit": N, "sessions": [rows as list_sessions returns them]}.
        #[arg(long)]
        sessions: Option<PathBuf>,
    },
    /// Ask for a sync run now (7.2): record a sync request, global or for one item, and print
    /// the project's scheduled sync task, for the calling session to run now.
    SyncNow {
        #[arg(long)]
        project: String,
        /// The item the next run reads in full, as owner/repo#N; it must be in the project's set.
        #[arg(long)]
        issue: Option<String>,
    },
    /// Only the read of GitHub (chapter 2): record what changed; no ingest, no outbox.
    Snapshot {
        #[arg(long)]
        project: String,
    },
    /// The sync job (7.2): the page documents listed into DIR (`<dir>/<collection>/<id>.json`)
    /// as one file for `cycle --pages`. Needs no root.
    Pages {
        #[arg(long)]
        dir: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// The sync job (7.2): plan the publish of an outbox as a generation of page documents:
    /// batch files of `ArtifactData` writes, a switch of `meta/active` pinned to the version
    /// read, and the generations to clean afterwards; or, when the outbox has the active
    /// generation's fingerprint, `unchanged` and one `meta/heartbeat` write. Needs no root.
    PublishPlan {
        #[arg(long)]
        outbox: PathBuf,
        /// `meta/active` as read (`list` or `get` with `out_dir`); absent before the first publish.
        #[arg(long, requires = "active_version")]
        active: Option<PathBuf>,
        /// The version that read returned.
        #[arg(long, requires = "active")]
        active_version: Option<u64>,
        /// The `out_dir` of a listing of the `gens` collection.
        #[arg(long)]
        markers: Option<PathBuf>,
        /// `meta/heartbeat` as read (the same listing as `--active`); absent when there is none yet.
        #[arg(long, requires = "heartbeat_version")]
        heartbeat: Option<PathBuf>,
        /// The version that read returned.
        #[arg(long, requires = "heartbeat")]
        heartbeat_version: Option<u64>,
        #[arg(long)]
        out: PathBuf,
    },
    /// The sync job (7.2): after a successful switch, plan the deletes of the generations a
    /// publish plan marks for cleaning, from the listings of them. Needs no root.
    CleanupPlan {
        /// The publish plan's plan.json.
        #[arg(long)]
        plan: PathBuf,
        /// The `out_dir` the listings of the generations went into.
        #[arg(long)]
        listing: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Measure a triage backend against history and the owner's records (triage spec 3).
    #[command(subcommand)]
    Eval(EvalCommand),
    /// Mark truth (triage spec 3.3).
    #[command(subcommand)]
    Truth(TruthCommand),
}

#[derive(Subcommand)]
enum EvalCommand {
    /// Read the project's issue history of the last eval_history_days (default 730) into
    /// state/evals/history; an interrupted read resumes from its last page (3.2).
    RefreshHistory {
        #[arg(long)]
        project: String,
    },
    /// Write a replay's blinded tuning and held-out question files into --out, and its manifest
    /// under state/evals (3.2).
    Prepare {
        #[arg(long)]
        project: String,
        #[arg(long)]
        decision: String,
        /// The backend measured, as model@effort.
        #[arg(long)]
        backend: String,
        #[arg(long)]
        out: PathBuf,
        /// Drop truths of this provenance; repeatable.
        #[arg(long)]
        exclude: Vec<String>,
        /// Only this repo's items (area is asked differently per repo).
        #[arg(long)]
        repo: Option<String>,
        /// Priority's stages: first (severity, reach, detectability), then priority.
        #[arg(long, value_enum)]
        stage: Option<StageArg>,
        /// Priority's stage: the first stage's answers files.
        #[arg(long)]
        answers: Vec<PathBuf>,
    },
    /// Score a replay's answers (or with --live a closure decision's recorded suggestions),
    /// fit and freeze its calibration map, and record the evaluated event (3.2, 3.5).
    Score {
        #[arg(long)]
        project: String,
        #[arg(long)]
        decision: String,
        #[arg(long)]
        backend: String,
        /// The answers files of the tuning and held-out questions.
        #[arg(long, required_unless_present = "live")]
        answers: Vec<PathBuf>,
        #[arg(long)]
        exclude: Vec<String>,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long, conflicts_with_all = ["answers", "repo"])]
        live: bool,
    },
    /// Every key's latest evaluation and drift (3.5).
    Status {
        #[arg(long)]
        project: String,
    },
}

#[derive(Subcommand)]
enum TruthCommand {
    /// List the later label events of a decision in a time range, and with --yes tag them as
    /// a named bulk review (3.3).
    Tag {
        #[arg(long)]
        project: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        decision: String,
        #[arg(long)]
        from: String,
        #[arg(long)]
        to: String,
        /// Only these issues (owner/repo#N); repeatable.
        #[arg(long)]
        issues: Vec<String>,
        /// Record the tags; without it the events are only listed.
        #[arg(long)]
        yes: bool,
    },
}

#[derive(Args)]
struct ReserveArgs {
    /// The launch to reserve.
    #[arg(long, required_unless_present_any = ["request", "next"])]
    launch: Option<String>,
    /// The dispatcher session making the reservation; it owns it.
    #[arg(long)]
    dispatcher: String,
    /// A launch document (JSON) the dispatcher just read; recorded as requested first.
    #[arg(long, conflicts_with = "next")]
    request: Option<PathBuf>,
    /// Reserve the oldest queued launch that can take a slot now.
    #[arg(long, conflicts_with = "launch")]
    next: bool,
}

#[derive(Clone, Copy, ValueEnum)]
enum EndStateArg {
    Landed,
    Stopped,
    Failed,
}

#[derive(Clone, Copy, ValueEnum)]
enum StageArg {
    First,
    Priority,
}

#[derive(Clone, Copy, ValueEnum)]
enum ByArg {
    Session,
    Dispatcher,
}

#[derive(Args)]
struct OutcomeArgs {
    #[arg(long)]
    launch: String,
    #[arg(long, action = clap::ArgAction::Set)]
    landed: bool,
    #[arg(long)]
    commit: Option<String>,
    #[arg(long)]
    pull_request: Option<String>,
    #[arg(long)]
    wall_clock_minutes: Option<u64>,
    /// A gate level and how many times it ran, as level=runs; repeatable.
    #[arg(long = "gate", value_name = "LEVEL=RUNS")]
    gates: Vec<String>,
    #[arg(long)]
    findings_fixed: Option<u64>,
    #[arg(long)]
    context_tokens: Option<u64>,
    /// Who records it: the session itself, or the dispatcher for a session that ended
    /// without one.
    #[arg(long, value_enum, default_value = "session")]
    by: ByArg,
}

/// The command's clock: the fixed time of `ISSUE_DASHBOARD_NOW`, else the system clock. It is
/// read once the root lock is held. A malformed override is an environment error (exit 2).
fn clock() -> anyhow::Result<impl Fn() -> DateTime<Utc> + Copy> {
    let fixed = match std::env::var(ENV_NOW) {
        Ok(s) => Some(parse_time(&s).map_err(|e| anyhow::anyhow!("{ENV_NOW}: {e}"))?),
        Err(_) => None,
    };
    Ok(move || fixed.unwrap_or_else(Utc::now))
}

fn options(command: &str) -> anyhow::Result<Options> {
    let mut o = Options {
        command: command.to_string(),
        abort_on_failpoint: true,
        ..Options::default()
    };
    if let Ok(fp) = std::env::var(ENV_FAILPOINT) {
        o.failpoint = Some(Failpoint::parse(&fp).with_context(|| format!("{ENV_FAILPOINT}={fp} is not a failpoint"))?);
    }
    if let Ok(ms) = std::env::var(ENV_LOCK_WAIT_MS) {
        o.lock_wait = Duration::from_millis(ms.parse().with_context(|| format!("{ENV_LOCK_WAIT_MS}={ms}"))?);
    }
    Ok(o)
}

/// GitHub as the environment chooses it: a replay, a recording of the real one, or the real one.
fn github() -> anyhow::Result<Box<dyn GitHub>> {
    if let Ok(dir) = std::env::var(ENV_REPLAY) {
        let replay = Replay::load(std::path::Path::new(&dir)).with_context(|| format!("{ENV_REPLAY}={dir}"))?;
        return Ok(Box::new(replay));
    }
    if let Ok(dir) = std::env::var(ENV_RECORD) {
        return Ok(Box::new(Recorder::new(GhCli, dir)));
    }
    Ok(Box::new(GhCli))
}

fn page_size() -> anyhow::Result<u32> {
    match std::env::var(ENV_PAGE_SIZE) {
        Ok(n) => Ok(n.parse().with_context(|| format!("{ENV_PAGE_SIZE}={n}"))?),
        Err(_) => Ok(read::DEFAULT_PAGE),
    }
}

fn parse_gate(s: &str) -> Result<GateRuns, Error> {
    let (level, runs) = s
        .split_once('=')
        .ok_or_else(|| Error::refused(format!("--gate {s}: expected LEVEL=RUNS")))?;
    let runs = runs
        .parse()
        .map_err(|_| Error::refused(format!("--gate {s}: RUNS is not a number")))?;
    Ok(GateRuns {
        level: level.to_string(),
        runs,
    })
}

fn outcome_of(a: &OutcomeArgs) -> Result<Outcome, Error> {
    let gate_runs = if a.gates.is_empty() {
        None
    } else {
        Some(a.gates.iter().map(|g| parse_gate(g)).collect::<Result<Vec<_>, _>>()?)
    };
    let mut missing = Vec::new();
    if a.commit.is_none() && a.pull_request.is_none() {
        missing.push("commit".to_string());
    }
    for (name, absent) in [
        ("wall_clock_minutes", a.wall_clock_minutes.is_none()),
        ("gate_runs", gate_runs.is_none()),
        ("findings_fixed", a.findings_fixed.is_none()),
        ("context_tokens", a.context_tokens.is_none()),
    ] {
        if absent {
            missing.push(name.to_string());
        }
    }
    Ok(Outcome {
        launch: a.launch.clone(),
        landed: a.landed,
        commit: a.commit.clone(),
        pull_request: a.pull_request.clone(),
        wall_clock_minutes: a.wall_clock_minutes,
        gate_runs,
        findings_fixed: a.findings_fixed,
        context_tokens: a.context_tokens,
        recorded_by: match a.by {
            ByArg::Session => RecordedBy::Session,
            ByArg::Dispatcher => RecordedBy::Dispatcher,
        },
        missing,
    })
}

/// Eval answers files (3.2), each `{backend, answers}`.
fn read_answers(paths: &[PathBuf]) -> anyhow::Result<Vec<eval::EvalAnswers>> {
    paths
        .iter()
        .map(|path| {
            let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
            Ok(serde_json::from_str(&text)
                .map_err(|e| Error::refused(format!("{}: not an answers file: {e}", path.display())))?)
        })
        .collect()
}

/// A path as one shell word, for the commands a brief hands a session.
fn shell_word(p: &std::path::Path) -> String {
    let s = p.display().to_string();
    if s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/._-~".contains(&b)) {
        s
    } else {
        format!("'{}'", s.replace('\'', r"'\''"))
    }
}

/// The sync helpers: files in, files out, no tracker root.
fn run_sync(command: &Command, now: DateTime<Utc>) -> anyhow::Result<Option<Value>> {
    let read_json = |path: &PathBuf| -> anyhow::Result<Value> {
        let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
        Ok(serde_json::from_str(&text).map_err(|e| Error::refused(format!("{}: not JSON: {e}", path.display())))?)
    };
    Ok(Some(match command {
        Command::Pages { dir, out } => {
            let p = publish::pages(dir)?;
            std::fs::write(out, serde_json::to_string(&p)?).with_context(|| out.display().to_string())?;
            json!({
                "out": out,
                "decisions": p.decisions.len(),
                "launches": p.launches.len(),
                "views": p.views.len(),
                "requests": p.requests.len(),
            })
        }
        Command::PublishPlan {
            outbox,
            active,
            active_version,
            markers,
            heartbeat,
            heartbeat_version,
            out,
        } => {
            let outbox = read_json(outbox)?;
            let heartbeat = match (heartbeat, heartbeat_version) {
                (Some(path), Some(version)) => Some(publish::Heartbeat {
                    doc: read_json(path)?,
                    version: *version,
                }),
                _ => None,
            };
            let active = match (active, active_version) {
                (Some(path), Some(version)) => Some(Active {
                    meta: read_json(path)?,
                    version: *version,
                }),
                _ => None,
            };
            let mut found = std::collections::BTreeMap::new();
            if let Some(dir) = markers {
                let gens = dir.join("gens");
                if gens.is_dir() {
                    for e in std::fs::read_dir(&gens)? {
                        let path = e?.path();
                        if path.extension().is_some_and(|x| x == "json")
                            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
                        {
                            found.insert(stem.to_string(), read_json(&path)?);
                        }
                    }
                }
            }
            publish::plan_with_heartbeat(&outbox, active.as_ref(), heartbeat.as_ref(), &found, now, out)?
        }
        Command::CleanupPlan { plan, listing, out } => publish::cleanup(plan, listing, out)?,
        _ => return Ok(None),
    }))
}

fn run(cli: Cli) -> anyhow::Result<Value> {
    if let Some(v) = run_sync(&cli.command, clock()?())? {
        return Ok(v);
    }
    let dir = cli
        .root
        .context("no tracker root: pass --root or set ISSUE_DASHBOARD_ROOT")?;
    let now = clock()?;
    let name = match &cli.command {
        Command::Rebuild { .. } => "rebuild",
        Command::Reserve(_) => "reserve",
        Command::LaunchStarted { .. } => "launch-started",
        Command::LaunchEnded { .. } => "launch-ended",
        Command::Release { .. } => "release",
        Command::Unconfirm { .. } => "unconfirm",
        Command::RecordOutcome(_) => "record-outcome",
        Command::Brief { .. } => "brief",
        Command::Prepare { .. } => "prepare",
        Command::RecordSuggestions { .. } => "record-suggestions",
        Command::Launches { .. } => "launches",
        Command::Cycle { .. } => "cycle",
        Command::Snapshot { .. } => "snapshot",
        Command::SyncNow { .. } => "sync-now",
        Command::TriageRequest { .. } => "triage-request",
        Command::Eval(EvalCommand::RefreshHistory { .. }) => "eval-refresh-history",
        Command::Eval(EvalCommand::Prepare { .. }) => "eval-prepare",
        Command::Eval(EvalCommand::Score { .. }) => "eval-score",
        Command::Eval(EvalCommand::Status { .. }) => "eval-status",
        Command::Truth(TruthCommand::Tag { .. }) => "truth-tag",
        Command::Pages { .. } | Command::PublishPlan { .. } | Command::CleanupPlan { .. } => {
            unreachable!("run_sync answered")
        }
    };
    let root = Root::new(&dir, options(name)?);

    Ok(match cli.command {
        Command::Rebuild { force } => {
            let r = root.rebuild(now(), force)?;
            json!({
                "events": r.events,
                "files": r.files,
                "dropped_tail_lines": r.dropped_tail_lines,
                "moved_aside": r.moved_aside,
            })
        }
        Command::Reserve(a) => {
            let request: Option<LaunchRequest> = match &a.request {
                Some(path) => {
                    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
                    Some(
                        serde_json::from_str(&text)
                            .map_err(|e| Error::refused(format!("{}: not a launch document: {e}", path.display())))?,
                    )
                }
                None => None,
            };
            if let (Some(id), Some(doc)) = (&a.launch, &request)
                && id != &doc.id
            {
                return Err(
                    Error::refused(format!("--launch {id} does not match the document's id {}", doc.id)).into(),
                );
            }
            let result = root.mutate_with_clock(now, |tx| {
                // Read under the lock (local files only), so a config recorded here is never
                // older than one another command recorded while this one waited.
                let config = ConfigSet::load(&dir)?;
                let capacity = |recipe: &str| config.capacity(recipe);
                config.record(tx)?;
                if a.next {
                    return Ok(serde_json::to_value(launch::reserve_next(
                        tx,
                        &a.dispatcher,
                        &capacity,
                    )?)?);
                }
                let id = match &request {
                    Some(doc) => {
                        // 7.3 step 1: only the owner's launch documents are acted on, and the
                        // daily run's triage requests (triage spec 5.2).
                        launch::check_author(&config, doc)?;
                        launch::request(tx, doc, Via::Reserve)?;
                        // A cancel ingested before this launch was requested takes effect now,
                        // or did on an earlier try of this command: either way it is withdrawn.
                        ingest::apply_cancels(tx, &doc.project)?;
                        if launch::load(tx.conn(), &doc.id)?.is_some_and(|l| l.state == launch::State::Withdrawn) {
                            return Ok(json!({"result": "withdrawn", "launch": doc.id}));
                        }
                        doc.id.clone()
                    }
                    None => {
                        let id = a.launch.clone().expect("clap requires --launch without --request");
                        // The same check for a launch already recorded: the daily run records
                        // its request itself, and reserve is where the author is checked.
                        if let Some(l) = launch::load(tx.conn(), &id)? {
                            launch::check_author(&config, &l.request)?;
                        }
                        id
                    }
                };
                Ok(serde_json::to_value(launch::reserve(
                    tx,
                    &id,
                    &a.dispatcher,
                    &capacity,
                )?)?)
            })?;
            if result.is_null() {
                json!({"result": "nothing_queued"})
            } else {
                result
            }
        }
        Command::LaunchStarted { token, session } => {
            serde_json::to_value(root.mutate_with_clock(now, |tx| launch::started(tx, &token, &session))?)?
        }
        Command::LaunchEnded { token, state } => {
            let state = match state {
                EndStateArg::Landed => EndState::Landed,
                EndStateArg::Stopped => EndState::Stopped,
                EndStateArg::Failed => EndState::Failed,
            };
            serde_json::to_value(root.mutate_with_clock(now, |tx| launch::ended(tx, &token, state))?)?
        }
        Command::Release {
            token,
            dispatcher,
            orphan_session,
        } => {
            let moved = match (orphan_session, dispatcher) {
                (Some(session), _) => root.mutate_with_clock(now, |tx| launch::settle_orphan(tx, &token, &session))?,
                (None, Some(d)) => root.mutate_with_clock(now, |tx| launch::release(tx, &token, &d))?,
                (None, None) => unreachable!("clap requires one of them"),
            };
            serde_json::to_value(moved)?
        }
        Command::Unconfirm { token, dispatcher } => {
            serde_json::to_value(root.mutate_with_clock(now, |tx| launch::unconfirm(tx, &token, &dispatcher))?)?
        }
        Command::RecordOutcome(a) => {
            let o = outcome_of(&a)?;
            let r = root.mutate_with_clock(now, |tx| outcome::record(tx, &o))?;
            json!({"launch": o.launch, "seq": r.event().seq, "repeat": !r.is_new(), "missing": o.missing})
        }
        Command::Brief { launch } => {
            let config = ConfigSet::load(&dir)?;
            let cli = format!("{} --root {}", shell_word(&std::env::current_exe()?), shell_word(&dir));
            serde_json::to_value(root.read(|l| brief::brief_with(l.conn(), &config, &launch, &cli))?)?
        }
        Command::Prepare {
            launch,
            session_model,
            out,
            stage,
        } => {
            let stage = match stage {
                StageArg::First => triage::Stage::First,
                StageArg::Priority => triage::Stage::Priority,
            };
            let prepared = root.mutate_with_clock(now, |tx| {
                let config = ConfigSet::load(&dir)?;
                triage::prepare(tx, &config, &launch, &session_model, stage)
            })?;
            // Written after the commit: a file never names questions the ledger does not hold.
            std::fs::write(&out, serde_json::to_string_pretty(&prepared.file)? + "\n")
                .with_context(|| out.display().to_string())?;
            serde_json::to_value(prepared.report)?
        }
        Command::RecordSuggestions { launch, file } => {
            let text = std::fs::read_to_string(&file).with_context(|| file.display().to_string())?;
            let input: suggest::Input = serde_json::from_str(&text)
                .map_err(|e| Error::refused(format!("{}: not a suggestions file: {e}", file.display())))?;
            // A closure's commit is looked up read-only before the ledger is locked (spec 1.4).
            let wanted = root.read(|l| suggest::commits_named(l.conn(), &input))?;
            let commits = match wanted.is_empty() {
                true => suggest::Commits::default(),
                false => suggest::check_commits(github()?.as_ref(), &wanted),
            };
            serde_json::to_value(root.mutate_with_clock(now, |tx| {
                let config = ConfigSet::load(&dir)?;
                suggest::record(tx, &config, &launch, &input, &commits)
            })?)?
        }
        Command::Launches { live, requested } => {
            let rows = root.read(|l| {
                let mut out = Vec::new();
                for x in launch::all(l.conn())? {
                    if live && !(x.state.holds() || x.state == launch::State::Queued) {
                        continue;
                    }
                    if requested && x.state != launch::State::Requested {
                        continue;
                    }
                    let outcome = ledger::latest(l.conn(), &x.id, Kind::Outcome)?.is_some();
                    out.push(json!({
                        "id": x.id,
                        "project": x.request.project,
                        "kind": x.request.kind,
                        "scope": x.request.triage_scope(),
                        "author": x.request.author,
                        "aliases": x.request.aliases,
                        "recipe": x.request.recipe,
                        "routing": x.request.routing,
                        "state": x.state,
                        "token": x.token,
                        "dispatcher": x.dispatcher,
                        "session": x.session,
                        "reserved_at": x.reserved_at,
                        "outcome": outcome,
                    }));
                }
                Ok(out)
            })?;
            json!({"launches": rows})
        }
        Command::Cycle {
            project,
            pages,
            sessions,
        } => {
            let pages: Pages = match &pages {
                Some(path) => {
                    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
                    serde_json::from_str(&text)
                        .map_err(|e| Error::refused(format!("{}: not a pages document: {e}", path.display())))?
                }
                None => Pages::default(),
            };
            let sessions: Option<SessionList> = match &sessions {
                Some(path) => {
                    let text = std::fs::read_to_string(path).with_context(|| path.display().to_string())?;
                    Some(
                        serde_json::from_str(&text)
                            .map_err(|e| Error::refused(format!("{}: not a session list: {e}", path.display())))?,
                    )
                }
                None => None,
            };
            let gh = github()?;
            // A replayed GitHub is never written to: a close then stays pending.
            let writer = std::env::var(ENV_REPLAY)
                .is_err()
                .then_some(&GhIssues as &dyn IssueWriter);
            serde_json::to_value(cycle::cycle_full(
                &root,
                gh.as_ref(),
                writer,
                &project,
                &pages,
                sessions.as_ref(),
                &now,
                page_size()?,
            )?)?
        }
        Command::SyncNow { project, issue } => serde_json::to_value(root.mutate_with_clock(now, |tx| {
            // Read under the lock, as `reserve` does.
            let config = ConfigSet::load(&dir)?;
            config.record(tx)?;
            sync::request(tx, &config, &project, issue.as_deref())
        })?)?,
        Command::TriageRequest { project } => serde_json::to_value(root.mutate_with_clock(now, |tx| {
            // Read under the lock, as `reserve` does.
            let config = ConfigSet::load(&dir)?;
            config.record(tx)?;
            issue_dashboard_core::daily::request(tx, &config, &project)
        })?)?,
        Command::Snapshot { project } => {
            let gh = github()?;
            json!({"repos": cycle::snapshot(&root, gh.as_ref(), &project, &now, page_size()?)?})
        }
        Command::Eval(EvalCommand::RefreshHistory { project }) => {
            let config = ConfigSet::load(&dir)?;
            let gh = github()?;
            let page = match std::env::var(ENV_PAGE_SIZE) {
                Ok(_) => page_size()?,
                Err(_) => history::HISTORY_PAGE,
            };
            // History is files beside the ledger, not events: it takes no lock, so a long read
            // never blocks a sync cycle; each page is written whole, by rename.
            let repos = history::refresh(&dir, &config, gh.as_ref(), &project, now(), page)?;
            json!({"repos": repos})
        }
        Command::Eval(EvalCommand::Prepare {
            project,
            decision,
            backend,
            out,
            exclude,
            repo,
            stage,
            answers,
        }) => {
            let config = ConfigSet::load(&dir)?;
            let first = read_answers(&answers)?;
            let stage = match stage {
                Some(StageArg::First) => eval::EvalStage::First,
                Some(StageArg::Priority) | None => eval::EvalStage::Scored,
            };
            let r = eval::Replay {
                exclude: eval::exclusions(&config, &project, &exclude)?,
                project,
                decision,
                backend,
                repo,
            };
            serde_json::to_value(root.read(|l| eval::prepare(l.conn(), &dir, &config, &r, stage, &first, &out))?)?
        }
        Command::Eval(EvalCommand::Score {
            project,
            decision,
            backend,
            answers,
            exclude,
            repo,
            live,
        }) => {
            let answers = read_answers(&answers)?;
            serde_json::to_value(root.mutate_with_clock(now, |tx| {
                let config = ConfigSet::load(&dir)?;
                if live {
                    return eval::score_live(tx, &dir, &config, &project, &decision, &backend, &exclude);
                }
                let r = eval::Replay {
                    exclude: eval::exclusions(&config, &project, &exclude)?,
                    project: project.clone(),
                    decision: decision.clone(),
                    backend: backend.clone(),
                    repo: repo.clone(),
                };
                eval::score(tx, &dir, &config, &r, &answers)
            })?)?
        }
        Command::Eval(EvalCommand::Status { project }) => {
            json!({"keys": root.read(|l| eval::status(l.conn(), &project))?})
        }
        Command::Truth(TruthCommand::Tag {
            project,
            name,
            decision,
            from,
            to,
            issues,
            yes,
        }) => {
            let (from, to) = (parse_time(&from)?, parse_time(&to)?);
            let config = ConfigSet::load(&dir)?;
            let history = history::project_history(&dir, &config, &project)?;
            let tagged = truth::taggable(&config, &history, &decision, from, to, &issues)?;
            let recorded = match yes {
                true => {
                    Some(root.mutate_with_clock(now, |tx| truth::record_tags(tx, &project, &name, &decision, &tagged))?)
                }
                false => None,
            };
            json!({"name": name, "decision": decision, "events": tagged, "recorded": recorded})
        }
        Command::Pages { .. } | Command::PublishPlan { .. } | Command::CleanupPlan { .. } => {
            unreachable!("run_sync answered")
        }
    })
}

fn main() -> ExitCode {
    let cli = Cli::parse();
    match run(cli) {
        Ok(v) => {
            // The command has committed; a closed stdout must not turn that into a panic.
            let _ = writeln!(std::io::stdout(), "{v}");
            ExitCode::SUCCESS
        }
        Err(e) => {
            let code = e.downcast_ref::<Error>().map_or(2, Error::exit_code);
            eprintln!("issue-dashboard: {e:#}");
            ExitCode::from(code as u8)
        }
    }
}
