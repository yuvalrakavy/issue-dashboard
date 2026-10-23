//! All GitHub access goes through one trait (2.7): the real implementation runs
//! `gh api graphql`, and [`Replay`] serves recorded real responses, so the tests see GitHub's
//! own label spellings, missing fields and errors rather than what a hand-written fixture
//! assumes. [`Recorder`] wraps the real one to capture those responses.
//!
//! Phase 1 is read-only toward GitHub (10.0): every request here is a query.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;

use serde_json::{Value, json};

use crate::canonical::{canonical_json, sha256_hex};
use crate::{Error, Result};

/// One GraphQL request. `name` is stable across edits of the query text, so a recording is
/// found by name and variables.
#[derive(Debug, Clone, PartialEq)]
pub struct Request {
    pub name: &'static str,
    pub query: &'static str,
    pub variables: Value,
}

/// GitHub, as the snapshot sees it.
pub trait GitHub {
    /// The whole response document of one GraphQL request (`data` and any `errors`). An `Err`
    /// means no response document came back: the network, authentication, or a non-JSON reply.
    /// Errors inside a document (a node that does not resolve) are for the reader to judge.
    fn graphql(&self, req: &Request) -> Result<Value>;
}

/// The real GitHub, through the `gh` CLI and its stored authentication.
#[derive(Debug, Clone, Default)]
pub struct GhCli;

impl GitHub for GhCli {
    fn graphql(&self, req: &Request) -> Result<Value> {
        let body = json!({"query": req.query, "variables": req.variables});
        // The header asks for the current node id format: without it GitHub still returns the
        // legacy base64 ids (with `=`) for objects created before the change, which are not
        // valid document ids (5.3). Probed 2026-10-23 on rhaiscript/rhai's repo id.
        let mut child = Command::new("gh")
            .args(["api", "graphql", "-H", "X-Github-Next-Global-ID: 1", "--input", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| Error::GitHub(format!("cannot run gh: {e}")))?;
        child
            .stdin
            .take()
            .expect("stdin is piped")
            .write_all(canonical_json(&body).as_bytes())?;
        let out = child.wait_with_output()?;
        // gh exits 1 when the document carries errors, with the document still on stdout.
        match serde_json::from_slice::<Value>(&out.stdout) {
            Ok(v) if v.get("data").is_some() || v.get("errors").is_some() => Ok(v),
            _ => Err(Error::GitHub(format!(
                "{}: gh exited with {}: {}",
                req.name,
                out.status,
                String::from_utf8_lossy(&out.stderr).trim()
            ))),
        }
    }
}

/// The variables a recording is found by. `since` is dropped and a search's `closed:>=` date is
/// blanked, since both move with the clock; the reader filters by time itself, so a recording
/// serves any watermark or history window.
pub fn replay_key(req: &Request) -> String {
    key_of(req.name, &req.variables)
}

fn key_of(name: &str, variables: &Value) -> String {
    let mut vars = variables.clone();
    if let Some(o) = vars.as_object_mut() {
        o.remove("since");
        if let Some(Value::String(q)) = o.get_mut("q") {
            *q = blank_dates(q);
        }
    }
    format!("{name}:{}", canonical_json(&vars))
}

/// The `closed:>=` date of a search, if it has one.
fn cutoff(variables: &Value) -> Option<String> {
    variables
        .get("q")?
        .as_str()?
        .split(' ')
        .find_map(|w| w.strip_prefix("closed:>="))
        .map(str::to_string)
}

fn blank_dates(q: &str) -> String {
    q.split(' ')
        .map(|w| match w.strip_prefix("closed:>=") {
            Some(_) => "closed:>=*".to_string(),
            None => w.to_string(),
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// A test's view of each request: the recorded response (if any) in, the response to serve out.
/// Returning an `Err` simulates a failed request.
pub type Hook = Box<dyn Fn(&Request, Option<Value>) -> Result<Value> + Send + Sync>;

/// Serves recorded responses (2.7). A request with no recording is an error unless a hook
/// answers it.
pub struct Replay {
    recordings: BTreeMap<String, Value>,
    /// The `closed:>=` date each search was recorded with: it serves a request for that window
    /// or a shorter one, never a longer one, whose older items it does not hold.
    cutoffs: BTreeMap<String, String>,
    hook: Option<Hook>,
    calls: Mutex<Vec<String>>,
}

impl Replay {
    /// Loads every recording (`*.json`, as [`Recorder`] writes them) in `dir`.
    pub fn load(dir: &Path) -> Result<Replay> {
        let mut recordings = BTreeMap::new();
        let mut cutoffs = BTreeMap::new();
        let mut files: Vec<PathBuf> = fs::read_dir(dir)?
            .map(|e| e.map(|e| e.path()))
            .collect::<std::io::Result<_>>()?;
        files.sort();
        for path in files {
            if path.extension().and_then(|e| e.to_str()) != Some("json") {
                continue;
            }
            let text = fs::read_to_string(&path)?;
            let rec: Value = serde_json::from_str(&text)
                .map_err(|e| Error::GitHub(format!("{}: not a recording: {e}", path.display())))?;
            let (Some(name), Some(vars), Some(resp)) = (
                rec.get("name").and_then(Value::as_str),
                rec.get("variables"),
                rec.get("response"),
            ) else {
                continue;
            };
            if let Some(c) = cutoff(vars) {
                cutoffs.insert(key_of(name, vars), c);
            }
            recordings.insert(key_of(name, vars), resp.clone());
        }
        Ok(Replay {
            recordings,
            cutoffs,
            hook: None,
            calls: Mutex::new(Vec::new()),
        })
    }

    /// Lets a test see and change every response, or fail a request.
    pub fn with_hook(mut self, hook: Hook) -> Replay {
        self.hook = Some(hook);
        self
    }

    /// The replay key of every request served so far, in order.
    pub fn calls(&self) -> Vec<String> {
        self.calls.lock().expect("calls lock").clone()
    }

    /// The first object with this `id` anywhere in the recordings: a real node to build a
    /// test's response from.
    pub fn node(&self, id: &str) -> Option<Value> {
        fn find(v: &Value, id: &str) -> Option<Value> {
            match v {
                Value::Object(o) => {
                    if o.get("id").and_then(Value::as_str) == Some(id) && o.contains_key("updatedAt") {
                        return Some(v.clone());
                    }
                    o.values().find_map(|c| find(c, id))
                }
                Value::Array(a) => a.iter().find_map(|c| find(c, id)),
                _ => None,
            }
        }
        self.recordings.values().find_map(|r| find(r, id))
    }
}

impl GitHub for Replay {
    fn graphql(&self, req: &Request) -> Result<Value> {
        let key = replay_key(req);
        self.calls.lock().expect("calls lock").push(key.clone());
        // ISO dates compare as strings.
        let uncovered = match (cutoff(&req.variables), self.cutoffs.get(&key)) {
            (Some(asked), Some(recorded)) if asked < *recorded => Some(recorded.clone()),
            _ => None,
        };
        let recorded = match uncovered {
            Some(_) => None,
            None => self.recordings.get(&key).cloned(),
        };
        match &self.hook {
            Some(h) => h(req, recorded),
            None => recorded.ok_or_else(|| match uncovered {
                Some(c) => Error::GitHub(format!(
                    "the recording for {key} holds items closed since {c} only; the request needs an earlier window"
                )),
                None => Error::GitHub(format!("no recording for {key}")),
            }),
        }
    }
}

/// Wraps a [`GitHub`] and writes each exchange to `dir` for [`Replay`]. Only the request's
/// name, variables and the response document are written: no headers, no token.
pub struct Recorder<G> {
    inner: G,
    dir: PathBuf,
}

impl<G: GitHub> Recorder<G> {
    pub fn new(inner: G, dir: impl Into<PathBuf>) -> Recorder<G> {
        Recorder { inner, dir: dir.into() }
    }
}

impl<G: GitHub> GitHub for Recorder<G> {
    fn graphql(&self, req: &Request) -> Result<Value> {
        let resp = self.inner.graphql(req)?;
        fs::create_dir_all(&self.dir)?;
        let key = replay_key(req);
        let file = self.dir.join(format!("{}-{}.json", req.name, &sha256_hex(&key)[..12]));
        let rec = json!({"name": req.name, "variables": req.variables, "response": resp});
        fs::write(file, serde_json::to_string_pretty(&rec)?)?;
        Ok(resp)
    }
}
