//! Shared test helpers: a temporary tracker root, fixed times, sample page documents and the
//! JSON Schema validator over `schemas/`.
#![allow(dead_code)]

use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, TimeZone, Utc};
use issue_dashboard_core::Root;
use issue_dashboard_core::docs::{Action, Decision, DecisionValue, LaunchKind, LaunchRequest, Priority, Routing, Target};
use issue_dashboard_core::root::Options;
use serde_json::Value;

pub const ID_BASE: &str = "https://github.com/yuvalrakavy/issue-dashboard/schemas/";

pub fn schemas_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../schemas")
}

struct LocalRetriever {
    dir: PathBuf,
}

impl jsonschema::Retrieve for LocalRetriever {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let name = uri
            .as_str()
            .strip_prefix(ID_BASE)
            .ok_or_else(|| format!("refusing to fetch a schema outside this repo: {uri}"))?;
        let text = fs::read_to_string(self.dir.join(name))?;
        Ok(serde_json::from_str(&text)?)
    }
}

pub fn schema(stem: &str) -> Value {
    let text = fs::read_to_string(schemas_dir().join(format!("{stem}.schema.json"))).unwrap();
    serde_json::from_str(&text).unwrap()
}

pub fn validator_for(schema: &Value) -> jsonschema::Validator {
    jsonschema::options()
        .with_draft(jsonschema::Draft::Draft202012)
        .should_validate_formats(true)
        .with_retriever(LocalRetriever { dir: schemas_dir() })
        .build(schema)
        .unwrap_or_else(|e| panic!("schema does not compile: {e}"))
}

/// Panics with every error if `instance` does not validate against `schemas/<stem>`.
pub fn assert_valid(stem: &str, instance: &Value) {
    let v = validator_for(&schema(stem));
    let errors: Vec<String> = v.iter_errors(instance).map(|e| e.to_string()).collect();
    assert!(errors.is_empty(), "{stem}: {instance}\n{errors:#?}");
}

/// The valid (non-`bad-`) examples of one schema, as JSON.
pub fn good_examples(stem: &str) -> Vec<(String, Value)> {
    let dir = schemas_dir().join("examples").join(stem);
    let mut out = Vec::new();
    for entry in fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.starts_with("bad-") {
            continue;
        }
        let text = fs::read_to_string(&path).unwrap();
        let value = if name.ends_with(".toml") {
            serde_json::to_value(toml::from_str::<toml::Table>(&text).unwrap()).unwrap()
        } else {
            serde_json::from_str(&text).unwrap()
        };
        out.push((name, value));
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    assert!(!out.is_empty(), "{stem} has no examples");
    out
}

/// A temporary tracker root; removed when dropped.
pub struct TempRoot {
    pub dir: tempfile::TempDir,
    pub root: Root,
}

impl TempRoot {
    pub fn path(&self) -> &Path {
        self.dir.path()
    }
}

pub fn temp_root() -> TempRoot {
    temp_root_with(Options::default())
}

pub fn temp_root_with(options: Options) -> TempRoot {
    let dir = tempfile::tempdir().unwrap();
    let root = Root::new(dir.path(), options);
    TempRoot { dir, root }
}

/// 2026-10-23 08:00:00 UTC plus `minutes`.
pub fn t(minutes: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 23, 8, 0, 0).unwrap() + Duration::minutes(minutes)
}

pub const OWNER: &str = "user_owner";
pub const ISSUE_A: &str = "I_kwDOissueA";
pub const ISSUE_B: &str = "I_kwDOissueB";
pub const ISSUE_C: &str = "I_kwDOissueC";

pub fn launch_doc(id: &str, project: &str, items: &[&str], recipe: &str) -> LaunchRequest {
    LaunchRequest {
        id: id.to_string(),
        project: project.to_string(),
        author: OWNER.to_string(),
        created_at: "2026-10-23T08:00:00Z".to_string(),
        kind: if items.len() > 1 { LaunchKind::Batch } else { LaunchKind::Fix },
        items: items.iter().map(|s| s.to_string()).collect(),
        aliases: items.iter().enumerate().map(|(i, _)| format!("example-org/server#{}", 100 + i)).collect(),
        recipe: recipe.to_string(),
        routing: Routing { model: "claude-opus-5-5".into(), effort: "medium".into(), workspace: None, reason: None },
        suggested_routing: None,
    }
}

pub fn decision(id: &str, author: &str, action: Action, target: Target, value: Option<DecisionValue>) -> Decision {
    Decision {
        id: id.to_string(),
        project: "home".to_string(),
        author: author.to_string(),
        created_at: "2026-10-23T08:00:00Z".to_string(),
        action,
        target,
        value,
        reason: None,
    }
}

pub fn set_priority(id: &str, item: &str, p: Priority) -> Decision {
    decision(
        id,
        OWNER,
        Action::SetPriority,
        Target { item: Some(item.into()), ..Default::default() },
        Some(DecisionValue { priority: Some(p), ..Default::default() }),
    )
}

pub fn undo(id: &str, of: &str) -> Decision {
    decision(id, OWNER, Action::Undo, Target { decision: Some(of.into()), ..Default::default() }, None)
}

/// Every line of every backup file, as text.
pub fn backup_lines(root: &Root) -> Vec<String> {
    let mut out = Vec::new();
    for f in root.backup().files().unwrap() {
        let text = fs::read_to_string(f).unwrap();
        out.extend(text.lines().map(str::to_string));
    }
    out
}

/// The `seq` of every backup line, in file order.
pub fn backup_seqs(root: &Root) -> Vec<i64> {
    backup_lines(root)
        .iter()
        .map(|l| serde_json::from_str::<Value>(l).unwrap()["event"]["seq"].as_i64().unwrap())
        .collect()
}
