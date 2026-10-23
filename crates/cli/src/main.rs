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
use issue_dashboard_core::config::ConfigSet;
use issue_dashboard_core::docs::LaunchRequest;
use issue_dashboard_core::launch::{self, EndState, Via};
use issue_dashboard_core::outcome::{self, GateRuns, Outcome, RecordedBy};
use issue_dashboard_core::root::{Options, parse_time};
use issue_dashboard_core::{Error, Root};
use serde_json::{Value, json};

/// Test seams, read from the environment and not shown in --help: a fixed clock, a simulated
/// crash point, and a shorter lock wait.
const ENV_NOW: &str = "ISSUE_DASHBOARD_NOW";
const ENV_FAILPOINT: &str = "ISSUE_DASHBOARD_FAILPOINT";
const ENV_LOCK_WAIT_MS: &str = "ISSUE_DASHBOARD_LOCK_WAIT_MS";

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

fn run(cli: Cli) -> anyhow::Result<Value> {
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
                let capacity = |recipe: &str| config.recipe(recipe).map(|r| r.capacity);
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
                        launch::request(tx, doc, Via::Reserve)?;
                        doc.id.clone()
                    }
                    None => a.launch.clone().expect("clap requires --launch without --request"),
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
