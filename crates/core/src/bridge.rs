//! The AgentBridge connector client (runner spec 1.2): one long-lived `agentbridge-dashboard
//! --config <abs>` child speaking JSON lines (AgentBridge `docs/DASHBOARD-CONTRACT-V2.md`).
//!
//! [`Bridge`] is the one seam: a request in, the response's `result` out. The typed calls
//! below are written once over it, so the runner's flow runs unchanged against [`Process`] and
//! against the scripted [`fake::Fake`] its tests drive.
//!
//! A request either answers or fails as [`Error::Bridge`]. An `error` response carries its
//! code; a dead child, a closed pipe or no answer within the bound carries none. Both mean the
//! outcome is unknown, and nothing here retries: the runner holds and reconciles (3.1 row 1).

use std::collections::HashMap;
use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{Error, Result};

/// The contract the runner was qualified against (runner spec 1.2).
pub const CONTRACT_VERSION: u32 = 2;
pub const METHODS: &[&str] = &["start", "send", "status", "wait", "transfer"];
pub const FEATURES: &[&str] = &["bulk_transfer", "stage_results", "event_wait"];
/// `wait`'s longest timeout (the connector's `limits.event_wait_timeout_ms`).
pub const MAX_WAIT_MS: u64 = 60_000;

/// One request to the connector.
pub trait Bridge: Send + Sync {
    /// `method` with `params`; `Ok` is the response's `result`.
    fn call(&self, method: &str, params: Value) -> Result<Value>;
}

fn unreachable(method: &str, message: impl Into<String>) -> Error {
    Error::Bridge {
        method: method.to_string(),
        code: None,
        message: message.into(),
    }
}

// --- Typed calls -------------------------------------------------------------------------------

/// The pin the runner refuses to start without (runner spec 1.2, `runner pin`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pin {
    pub package_version: String,
    pub contract_version: u32,
    /// The connector executable's SHA-256, as its `capabilities.executable` observes it.
    pub executable_sha256: String,
}

/// The handshake: requires [`CONTRACT_VERSION`], [`METHODS`] and [`FEATURES`] (the connector
/// refuses itself if one is missing), then compares the answer to `pin`.
pub fn handshake(b: &dyn Bridge, pin: &Pin) -> Result<Value> {
    let caps = b.call(
        "capabilities",
        json!({"require": {"contract_version": CONTRACT_VERSION, "methods": METHODS, "features": FEATURES}}),
    )?;
    let seen = Pin {
        package_version: caps["package_version"].as_str().unwrap_or_default().to_string(),
        contract_version: caps["connector_contract_version"].as_u64().unwrap_or(0) as u32,
        executable_sha256: caps["executable"]["sha256"].as_str().unwrap_or_default().to_string(),
    };
    if &seen != pin {
        return Err(Error::refused(format!(
            "the connector is not the one the runner is pinned to: pinned {pin:?}, running {seen:?}; \
             qualify it and `runner pin` again"
        )));
    }
    Ok(caps)
}

/// `start`'s parameters (AgentBridge `dashboard/triage.rs` `Start`, which denies unknown
/// fields). The whole request is at most 24 KiB.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Start {
    pub operation_id: String,
    pub launch_id: String,
    pub issue_id: String,
    pub issue_revision: String,
    pub profile: String,
    pub model: String,
    pub effort: String,
    pub brief: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub access: Option<String>,
    #[serde(skip_serializing_if = "Value::is_null")]
    pub prepared: Value,
}

pub const START_MAX_BYTES: usize = 24 * 1024;

pub fn start(b: &dyn Bridge, s: &Start) -> Result<Value> {
    let params = serde_json::to_value(s)?;
    let size = params.to_string().len();
    if size > START_MAX_BYTES {
        return Err(Error::refused(format!(
            "the start request for {} is {size} bytes; the connector takes at most {START_MAX_BYTES}",
            s.operation_id
        )));
    }
    b.call("start", params)
}

/// A token's status body (`store.rs` `token_status`). Fields the runner reads are typed; the
/// whole body is kept in `body`, since it is the evidence a runner transition records.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct Status {
    pub state: String,
    #[serde(default)]
    pub task_state: String,
    #[serde(default)]
    pub session_state: String,
    #[serde(default)]
    pub never_started: bool,
    #[serde(default)]
    pub reason: Value,
    #[serde(default)]
    pub result: Option<Value>,
    #[serde(default)]
    pub routing: Value,
    #[serde(default)]
    pub native_session_id: Option<String>,
    #[serde(default)]
    pub question: Option<Value>,
    #[serde(default)]
    pub stage: Option<Value>,
    #[serde(skip)]
    pub body: Value,
}

impl Status {
    /// The routing AgentBridge verified natively, if it matches `model` and `effort` (3.1 row 8).
    /// `None` when there is no verified routing yet; `Some(false)` on a mismatch.
    pub fn routing_matches(&self, model: &str, effort: &str) -> Option<bool> {
        if self.routing["verified"] != true {
            return None;
        }
        Some(self.routing["model"] == model && self.routing["effort"] == effort)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum TokenStatus {
    /// AgentBridge has no job for the token: it was never started (3.1 rows 2-3).
    Unknown,
    Known(Box<Status>),
}

/// The connector's answer for a token AgentBridge does not hold (`store.rs` `token_status`).
const UNKNOWN_TOKEN: &str = "unknown tracker token";

pub fn status(b: &dyn Bridge, operation_id: &str) -> Result<TokenStatus> {
    match b.call("status", json!({"operation_id": operation_id})) {
        Ok(body) => {
            let mut s: Status = serde_json::from_value(body.clone())?;
            s.body = body;
            Ok(TokenStatus::Known(Box::new(s)))
        }
        Err(Error::Bridge {
            code: Some(_), message, ..
        }) if message.contains(UNKNOWN_TOKEN) => Ok(TokenStatus::Unknown),
        Err(e) => Err(e),
    }
}

/// A message to the token's worker (`stage_input`, a finish instruction).
pub fn send(b: &dyn Bridge, operation_id: &str, interaction_id: &str, message: Value) -> Result<Value> {
    b.call(
        "send",
        json!({"operation_id": operation_id, "interaction_id": interaction_id, "message": message}),
    )
}

pub fn archive(b: &dyn Bridge, operation_id: &str, interaction_id: &str) -> Result<Value> {
    b.call(
        "archive",
        json!({"operation_id": operation_id, "interaction_id": interaction_id}),
    )
}

/// Events strictly after `after`, or none at the timeout (at most [`MAX_WAIT_MS`]).
pub fn wait(b: &dyn Bridge, after: u64, timeout_ms: u64, limit: u64) -> Result<Vec<Value>> {
    let v = b.call(
        "wait",
        json!({"after": after, "timeout_ms": timeout_ms.min(MAX_WAIT_MS), "limit": limit}),
    )?;
    match v {
        Value::Array(events) => Ok(events),
        Value::Object(ref o) if o.get("events").is_some_and(Value::is_array) => {
            Ok(o["events"].as_array().cloned().unwrap_or_default())
        }
        other => Err(Error::Bridge {
            method: "wait".into(),
            code: Some("unexpected_shape".into()),
            message: format!("wait answered neither an event array nor {{events}}: {other}"),
        }),
    }
}

/// Per-profile readiness, keyed by profile (`triage.rs` `health`).
pub fn health(b: &dyn Bridge, profiles: &[String]) -> Result<HashMap<String, bool>> {
    let v = b.call("health", json!({"profiles": profiles}))?;
    Ok(v["harnesses"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|h| Some((h["profile"].as_str()?.to_string(), h["ready_to_attempt"] == true)))
        .collect())
}

pub fn transfer(b: &dyn Bridge, params: Value) -> Result<Value> {
    b.call("transfer", params)
}

// --- The process client ------------------------------------------------------------------------

type Pending = Arc<Mutex<HashMap<String, mpsc::Sender<Value>>>>;

/// The `agentbridge-dashboard` child. Responses may arrive in any order: a reader thread routes
/// each to its request by id. When the runner dies, even by SIGKILL, the child's stdin closes
/// and the connector unregisters and exits on that EOF; nothing depends on `Drop` running.
pub struct Process {
    child: Mutex<Child>,
    stdin: Mutex<Option<ChildStdin>>,
    pending: Pending,
    dead: Arc<Mutex<Option<String>>>,
    next: AtomicU64,
    /// How long any request but `wait` may take; `wait` gets its own timeout on top.
    pub bound: Duration,
}

impl Process {
    pub fn spawn(exe: &Path, args: &[OsString]) -> Result<Process> {
        let mut child = Command::new(exe)
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()?;
        let stdout = child.stdout.take().expect("stdout is piped");
        let stdin = child.stdin.take();
        let pending: Pending = Arc::default();
        let dead: Arc<Mutex<Option<String>>> = Arc::default();
        let (p, d) = (pending.clone(), dead.clone());
        thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { break };
                let Ok(v) = serde_json::from_str::<Value>(&line) else {
                    eprintln!("agentbridge: unparseable line ignored: {line}");
                    continue;
                };
                let id = v["id"].as_str().map(str::to_string);
                match id.and_then(|id| p.lock().expect("pending").remove(&id)) {
                    Some(tx) => {
                        let _ = tx.send(v);
                    }
                    None => eprintln!("agentbridge: a response for no open request: {line}"),
                }
            }
            *d.lock().expect("dead") = Some("the connector closed its output".into());
            // Dropping every sender wakes every open request with a disconnect.
            p.lock().expect("pending").clear();
        });
        Ok(Process {
            child: Mutex::new(child),
            stdin: Mutex::new(stdin),
            pending,
            dead,
            next: AtomicU64::new(1),
            bound: Duration::from_secs(120),
        })
    }

    /// `None` while the child runs; else why it stopped.
    pub fn stopped(&self) -> Option<String> {
        if let Some(why) = self.dead.lock().expect("dead").clone() {
            return Some(why);
        }
        match self.child.lock().expect("child").try_wait() {
            Ok(Some(status)) => Some(format!("the connector exited: {status}")),
            Ok(None) => None,
            Err(e) => Some(format!("the connector's state is unreadable: {e}")),
        }
    }

    /// Closes stdin (the connector's shutdown signal) and waits up to `grace` for it to exit,
    /// then kills it.
    pub fn shutdown(&self, grace: Duration) {
        self.stdin.lock().expect("stdin").take();
        let deadline = std::time::Instant::now() + grace;
        let mut child = self.child.lock().expect("child");
        while std::time::Instant::now() < deadline {
            if let Ok(Some(_)) = child.try_wait() {
                return;
            }
            thread::sleep(Duration::from_millis(50));
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Bridge for Process {
    fn call(&self, method: &str, params: Value) -> Result<Value> {
        if let Some(why) = self.stopped() {
            return Err(unreachable(method, why));
        }
        let id = format!("r-{}", self.next.fetch_add(1, Ordering::SeqCst));
        let (tx, rx) = mpsc::channel();
        self.pending.lock().expect("pending").insert(id.clone(), tx);
        let line = json!({"id": id, "method": method, "params": params}).to_string();
        let wrote = match self.stdin.lock().expect("stdin").as_mut() {
            Some(w) => writeln!(w, "{line}").and_then(|_| w.flush()),
            None => Err(std::io::Error::other("the connector's input is closed")),
        };
        if let Err(e) = wrote {
            self.pending.lock().expect("pending").remove(&id);
            return Err(unreachable(method, format!("could not write the request: {e}")));
        }
        let bound = match method {
            "wait" => Duration::from_millis(params["timeout_ms"].as_u64().unwrap_or(MAX_WAIT_MS)) + self.bound,
            _ => self.bound,
        };
        let response = match rx.recv_timeout(bound) {
            Ok(v) => v,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.pending.lock().expect("pending").remove(&id);
                return Err(unreachable(method, format!("no answer within {}s", bound.as_secs())));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                let why = self.stopped().unwrap_or_else(|| "the connector stopped".into());
                return Err(unreachable(method, why));
            }
        };
        match response.get("error") {
            Some(e) => Err(Error::Bridge {
                method: method.to_string(),
                code: Some(e["code"].as_str().unwrap_or("unknown").to_string()),
                message: e["message"].as_str().unwrap_or_default().to_string(),
            }),
            None => Ok(response.get("result").cloned().unwrap_or(Value::Null)),
        }
    }
}

// --- The scripted fake -------------------------------------------------------------------------

pub mod fake {
    //! A scripted connector for the runner's tests: every call is recorded, and each method
    //! answers from a queue the test fills, or from a per-token status table.

    use std::collections::{HashMap, VecDeque};
    use std::sync::Mutex;

    use serde_json::{Value, json};

    use super::Bridge;
    use crate::{Error, Result};

    /// One scripted answer.
    #[derive(Debug, Clone)]
    pub enum Answer {
        Ok(Value),
        /// An `error` response with this code and message.
        Err(&'static str, String),
        /// The child is gone: no response.
        Down,
    }

    #[derive(Default)]
    pub struct Fake {
        /// Answers per method, in order; an empty queue falls back to the defaults below.
        pub script: Mutex<HashMap<String, VecDeque<Answer>>>,
        /// `status` bodies by operation id; a token not here is unknown to AgentBridge.
        pub tokens: Mutex<HashMap<String, Value>>,
        /// Every call, in order.
        pub calls: Mutex<Vec<(String, Value)>>,
    }

    impl Fake {
        pub fn new() -> Fake {
            Fake::default()
        }

        pub fn push(&self, method: &str, a: Answer) {
            self.script
                .lock()
                .expect("script")
                .entry(method.to_string())
                .or_default()
                .push_back(a);
        }

        pub fn set_status(&self, operation_id: &str, body: Value) {
            self.tokens.lock().expect("tokens").insert(operation_id.to_string(), body);
        }

        /// The params of every call to `method`.
        pub fn calls_to(&self, method: &str) -> Vec<Value> {
            self.calls
                .lock()
                .expect("calls")
                .iter()
                .filter(|(m, _)| m == method)
                .map(|(_, p)| p.clone())
                .collect()
        }
    }

    impl Bridge for Fake {
        fn call(&self, method: &str, params: Value) -> Result<Value> {
            self.calls.lock().expect("calls").push((method.to_string(), params.clone()));
            let scripted = self
                .script
                .lock()
                .expect("script")
                .get_mut(method)
                .and_then(VecDeque::pop_front);
            let answer = match scripted {
                Some(a) => a,
                None => match method {
                    "status" => {
                        let op = params["operation_id"].as_str().unwrap_or_default();
                        match self.tokens.lock().expect("tokens").get(op) {
                            Some(body) => Answer::Ok(body.clone()),
                            None => Answer::Err("request_failed", "unknown tracker token".into()),
                        }
                    }
                    "start" => {
                        let op = params["operation_id"].as_str().unwrap_or_default().to_string();
                        let mut tokens = self.tokens.lock().expect("tokens");
                        let body = tokens
                            .entry(op.clone())
                            .or_insert_with(|| json!({"operation_id": op, "state": "queued", "task_state": "queued", "session_state": "pending", "never_started": false, "reason": null, "result": null, "routing": null}))
                            .clone();
                        Answer::Ok(body)
                    }
                    "wait" => Answer::Ok(json!([])),
                    // A prepared file is ready at once, its manifest naming its key.
                    "transfer" => match params["action"].as_str() {
                        Some("prepare") => {
                            let key = params["key"].as_str().unwrap_or_default();
                            Answer::Ok(json!({"transfer_id": format!("t-{key}"), "state": "preparing"}))
                        }
                        Some("status") => {
                            let id = params["transfer_id"].clone();
                            Answer::Ok(json!({
                                "transfer_id": id,
                                "preparation": {"state": "ready", "error": null},
                                "manifest": {"id": id, "name": "staged"},
                            }))
                        }
                        _ => Answer::Ok(json!({})),
                    },
                    _ => Answer::Ok(json!({})),
                },
            };
            match answer {
                Answer::Ok(v) => Ok(v),
                Answer::Err(code, message) => Err(Error::Bridge {
                    method: method.to_string(),
                    code: Some(code.to_string()),
                    message,
                }),
                Answer::Down => Err(Error::Bridge {
                    method: method.to_string(),
                    code: None,
                    message: "the connector is down (scripted)".into(),
                }),
            }
        }
    }
}
