//! The replay fixture: the tracker config under `tests/fixtures/config` and real GitHub
//! responses under `tests/fixtures/replay`, recorded from yuvalrakavy/Store,
//! yuvalrakavy/airzoned and rhaiscript/rhai (2.7). Tests edit the responses through a hook.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Duration, Utc};
use issue_dashboard_core::github::{GitHub, Replay, Request};
use issue_dashboard_core::ingest::Pages;
use issue_dashboard_core::root::{Options, parse_time};
use issue_dashboard_core::suggest;
use issue_dashboard_core::{Error, Result, Root};
use serde_json::{Value, json};

use super::TempRoot;

pub const PAGE: u32 = 25;
pub const STORE: &str = "yuvalrakavy/Store";
pub const AIRZONED: &str = "yuvalrakavy/airzoned";
pub const RHAI: &str = "rhaiscript/rhai";

pub fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// When the recordings were taken: the tests' clock starts here, so the history window
/// matches what was recorded.
pub fn recorded_at() -> DateTime<Utc> {
    let meta: Value = serde_json::from_str(&fs::read_to_string(fixtures().join("replay-meta.json")).unwrap()).unwrap();
    parse_time(meta["recorded_at"].as_str().unwrap()).unwrap()
}

/// `recorded_at` plus `minutes`.
pub fn at(minutes: i64) -> DateTime<Utc> {
    recorded_at() + Duration::minutes(minutes)
}

fn copy_dir(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for e in fs::read_dir(from).unwrap() {
        let e = e.unwrap();
        let target = to.join(e.file_name());
        if e.file_type().unwrap().is_dir() {
            copy_dir(&e.path(), &target);
        } else {
            fs::copy(e.path(), target).unwrap();
        }
    }
}

/// A temporary root holding the fixture config.
pub fn fixture_root() -> TempRoot {
    fixture_root_with(Options::default())
}

pub fn fixture_root_with(options: Options) -> TempRoot {
    let t = super::temp_root_with(options);
    copy_dir(&fixtures().join("config"), t.path());
    t
}

/// The recordings, for looking up real nodes.
pub fn library() -> Arc<Replay> {
    Arc::new(Replay::load(&fixtures().join("replay")).unwrap())
}

/// A real node in the shape `nodes` returns it, found anywhere in the recordings.
pub fn real_node(lib: &Replay, id: &str) -> Option<Value> {
    let mut n = lib.node(id)?;
    if n.get("__typename").is_none() {
        n["__typename"] = json!(if n.get("isDraft").is_some() {
            "PullRequest"
        } else {
            "Issue"
        });
    }
    Some(n)
}

/// Answers any `nodes` request from the recordings, whatever ids it asks for; an id the
/// recordings do not hold resolves to null, with GitHub's NOT_FOUND error.
pub fn synth_nodes(lib: &Replay, req: &Request) -> Value {
    let ids: Vec<String> = req.variables["ids"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i.as_str().unwrap().to_string())
        .collect();
    let mut errors = Vec::new();
    let nodes: Vec<Value> = ids
        .iter()
        .enumerate()
        .map(|(i, id)| match real_node(lib, id) {
            Some(n) => n,
            None => {
                errors.push(json!({"type": "NOT_FOUND", "path": ["nodes", i], "message": format!("Could not resolve to a node with the global id of '{id}'")}));
                Value::Null
            }
        })
        .collect();
    let mut resp = json!({"data": {"nodes": nodes}});
    if !errors.is_empty() {
        resp["errors"] = json!(errors);
    }
    resp
}

/// A test's edit of one response: the recorded response (an error when there is none) in,
/// the response to serve out.
pub type Edit = Box<dyn Fn(&Request, Result<Value>) -> Result<Value> + Send + Sync>;

/// The replay with `edit` applied to every response. `nodes` requests are answered from the
/// recordings whatever ids they ask for.
pub fn gh_with(edit: impl Fn(&Request, Result<Value>) -> Result<Value> + Send + Sync + 'static) -> Replay {
    let lib = library();
    Replay::load(&fixtures().join("replay"))
        .unwrap()
        .with_hook(Box::new(move |req, recorded| {
            let base = if req.name == "nodes" {
                Ok(synth_nodes(&lib, req))
            } else {
                recorded.ok_or_else(|| Error::GitHub(format!("no recording for {}", req.name)))
            };
            edit(req, base)
        }))
}

pub fn gh() -> Replay {
    gh_with(|_, v| v)
}

/// A replay whose edit a test can change between cycles.
pub struct Switch {
    pub edit: Arc<Mutex<Edit>>,
}

impl Switch {
    pub fn new() -> Switch {
        Switch {
            edit: Arc::new(Mutex::new(Box::new(|_, v| v))),
        }
    }

    pub fn set(&self, edit: impl Fn(&Request, Result<Value>) -> Result<Value> + Send + Sync + 'static) {
        *self.edit.lock().unwrap() = Box::new(edit);
    }

    pub fn clear(&self) {
        self.set(|_, v| v);
    }

    pub fn gh(&self) -> Replay {
        let edit = self.edit.clone();
        gh_with(move |req, v| (edit.lock().unwrap())(req, v))
    }
}

/// Is `req` a membership search of `repo` (any qualifiers)?
pub fn is_search_of(req: &Request, repo: &str) -> bool {
    req.name == "search"
        && req.variables["q"]
            .as_str()
            .is_some_and(|q| q.split(' ').any(|w| w.eq_ignore_ascii_case(&format!("repo:{repo}"))))
}

/// Removes `id` from a search response.
pub fn drop_from_search(mut v: Value, id: &str) -> Value {
    if let Some(nodes) = v["data"]["search"]["nodes"].as_array_mut() {
        nodes.retain(|n| n["id"] != id);
    }
    v
}

/// Replaces every string equal to `from` (and every `from#N` alias prefix) in a document.
pub fn rename(v: &mut Value, from: &str, to: &str) {
    match v {
        Value::String(s) if s == from => *s = to.to_string(),
        Value::Array(a) => a.iter_mut().for_each(|x| rename(x, from, to)),
        Value::Object(o) => o.values_mut().for_each(|x| rename(x, from, to)),
        _ => {}
    }
}

pub fn no_pages() -> Pages {
    Pages::default()
}

pub fn root_of(t: &TempRoot) -> &Root {
    &t.root
}

/// Calls a [`GitHub`] through a reference, for passing a `Replay` where a trait object is needed.
pub fn dyn_gh(r: &Replay) -> &dyn GitHub {
    r
}

/// A recorded Store commit, and one Store does not have (`commit-store-*.json`).
pub const STORE_COMMIT: &str = "c96bdcb";
pub const MISSING_COMMIT: &str = "0000000deadbeef";

/// GitHub for `record-suggestions`: the recorded commit lookups, and a commit no recording
/// names found, in the recorded shape (the fixtures' made-up hashes are not real commits).
pub fn commit_lookup() -> Replay {
    Replay::load(&fixtures().join("replay")).unwrap().with_hook(Box::new(
        |req: &Request, recorded: Option<Value>| match (req.name, recorded) {
            (_, Some(r)) => Ok(r),
            ("commit", None) => Ok(json!({
                "data": {"repository": {"object": {"__typename": "Commit", "oid": req.variables["rev"]}}}
            })),
            (name, None) => Err(Error::GitHub(format!("no recording for {name}"))),
        },
    ))
}

/// What GitHub holds of the commits `input` names: the lookup `record-suggestions` runs first.
pub fn commits_for(t: &TempRoot, input: &suggest::Input, gh: &dyn GitHub) -> suggest::Commits {
    let wanted = t.root.read(|l| suggest::commits_named(l.conn(), input)).unwrap();
    suggest::check_commits(gh, &wanted)
}
