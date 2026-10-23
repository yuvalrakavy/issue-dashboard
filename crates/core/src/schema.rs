//! The JSON Schemas of `schemas/`, compiled into the binary, and validation against them
//! (10.3: the CLI validates what it records against the shipped schemas).

use std::collections::HashMap;
use std::sync::OnceLock;

use serde_json::Value;

use crate::{Error, Result};

const ID_BASE: &str = "https://github.com/yuvalrakavy/issue-dashboard/schemas/";

macro_rules! schemas {
    ($($stem:literal),* $(,)?) => {
        &[$(($stem, include_str!(concat!("../../../schemas/", $stem, ".schema.json")))),*]
    };
}

/// Every schema, by stem. A test checks this list against the files in `schemas/`.
pub const SCHEMAS: &[(&str, &str)] = schemas![
    "common",
    "config-project",
    "config-registry",
    "config-repo-file",
    "doc-acks",
    "doc-fork",
    "doc-issue",
    "doc-launch-status",
    "doc-meta",
    "doc-metrics",
    "doc-suggestion",
    "event",
    "event-config",
    "event-decided",
    "event-exported",
    "event-item-gone",
    "event-launch",
    "event-membership",
    "event-observed",
    "event-outbox",
    "event-outcome",
    "event-request",
    "event-suggested",
    "event-view-saved",
    "event-write-applied",
    "event-write-failed",
    "event-write-intended",
    "filing-block",
    "ledger-line",
    "page-decision",
    "page-launch",
    "page-request",
    "page-view",
];

struct Embedded;

impl jsonschema::Retrieve for Embedded {
    fn retrieve(
        &self,
        uri: &jsonschema::Uri<String>,
    ) -> std::result::Result<Value, Box<dyn std::error::Error + Send + Sync>> {
        let name = uri
            .as_str()
            .strip_prefix(ID_BASE)
            .ok_or_else(|| format!("not one of this tool's schemas: {uri}"))?;
        let stem = name.strip_suffix(".schema.json").unwrap_or(name);
        let text = SCHEMAS
            .iter()
            .find(|(s, _)| *s == stem)
            .ok_or_else(|| format!("no schema {name}"))?
            .1;
        Ok(serde_json::from_str(text)?)
    }
}

fn validators() -> &'static HashMap<&'static str, jsonschema::Validator> {
    static CELL: OnceLock<HashMap<&'static str, jsonschema::Validator>> = OnceLock::new();
    CELL.get_or_init(|| {
        SCHEMAS
            .iter()
            .filter(|(stem, _)| *stem != "common")
            .map(|(stem, text)| {
                let schema: Value = serde_json::from_str(text).expect("an embedded schema is JSON");
                let v = jsonschema::options()
                    .with_draft(jsonschema::Draft::Draft202012)
                    .should_validate_formats(true)
                    .with_retriever(Embedded)
                    .build(&schema)
                    .unwrap_or_else(|e| panic!("embedded schema {stem} does not compile: {e}"));
                (*stem, v)
            })
            .collect()
    })
}

/// The errors of `instance` against `schemas/<stem>.schema.json`; empty when it is valid.
pub fn errors(stem: &str, instance: &Value) -> Vec<String> {
    let v = validators().get(stem).unwrap_or_else(|| panic!("no schema {stem}"));
    v.iter_errors(instance)
        .map(|e| format!("{} at {}", e, e.instance_path()))
        .collect()
}

/// Refuses `instance` unless it validates against `schemas/<stem>.schema.json`.
pub fn validate(stem: &str, what: &str, instance: &Value) -> Result<()> {
    let errs = errors(stem, instance);
    if errs.is_empty() {
        return Ok(());
    }
    Err(Error::refused(format!(
        "{what} does not match {stem}.schema.json: {}",
        errs.join("; ")
    )))
}

/// The schema stem of an event kind.
pub fn event_stem(kind: crate::Kind) -> String {
    format!("event-{}", kind.as_str().replace('_', "-"))
}
