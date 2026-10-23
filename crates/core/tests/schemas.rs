//! Every JSON Schema in `schemas/` is a valid draft 2020-12 schema, every example in
//! `schemas/examples/<schema>/` validates against its schema, and every example whose file
//! name starts with `bad-` is refused by it.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use common::{ID_BASE, schemas_dir, validator_for};
use serde_json::Value;

/// Schemas that hold only shared definitions and have no instances of their own.
const DEFINITIONS_ONLY: &[&str] = &["common"];

fn load_schemas() -> BTreeMap<String, Value> {
    let mut out = BTreeMap::new();
    for entry in fs::read_dir(schemas_dir()).expect("schemas/ exists") {
        let path = entry.unwrap().path();
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if let Some(stem) = name.strip_suffix(".schema.json") {
            let text = fs::read_to_string(&path).unwrap();
            let value: Value = serde_json::from_str(&text).unwrap_or_else(|e| panic!("{name} is not JSON: {e}"));
            out.insert(stem.to_string(), value);
        }
    }
    out
}

fn load_example(path: &Path) -> Value {
    let text = fs::read_to_string(path).unwrap();
    match path.extension().and_then(|e| e.to_str()) {
        Some("json") => serde_json::from_str(&text).unwrap_or_else(|e| panic!("{} is not JSON: {e}", path.display())),
        Some("toml") => {
            let table: toml::Table =
                toml::from_str(&text).unwrap_or_else(|e| panic!("{} is not TOML: {e}", path.display()));
            serde_json::to_value(table).unwrap()
        }
        other => panic!("{}: unexpected example extension {other:?}", path.display()),
    }
}

#[test]
fn every_schema_is_a_valid_draft_2020_12_schema_with_its_id() {
    let schemas = load_schemas();
    assert!(
        schemas.len() >= 27,
        "expected every schema of the phase-1 surface, found {}",
        schemas.len()
    );
    for (stem, schema) in &schemas {
        assert_eq!(
            schema["$schema"], "https://json-schema.org/draft/2020-12/schema",
            "{stem}: wrong $schema"
        );
        assert_eq!(
            schema["$id"],
            format!("{ID_BASE}{stem}.schema.json"),
            "{stem}: wrong $id"
        );
        jsonschema::meta::validate(schema).unwrap_or_else(|e| panic!("{stem} is not a valid schema: {e}"));
    }
}

#[test]
fn every_example_validates_and_every_bad_example_is_refused() {
    let schemas = load_schemas();
    let mut good = 0;
    let mut bad = 0;
    for (stem, schema) in &schemas {
        if DEFINITIONS_ONLY.contains(&stem.as_str()) {
            continue;
        }
        let validator = validator_for(schema);
        let dir = schemas_dir().join("examples").join(stem);
        let mut good_here = 0;
        let mut bad_here = 0;
        for entry in fs::read_dir(&dir).unwrap_or_else(|_| panic!("{stem} has no examples dir")) {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap().to_string_lossy().into_owned();
            let instance = load_example(&path);
            let errors: Vec<String> = validator.iter_errors(&instance).map(|e| e.to_string()).collect();
            if name.starts_with("bad-") {
                assert!(
                    !errors.is_empty(),
                    "{stem}/{name} was meant to be refused but validates"
                );
                // Shown with --no-capture, to check each bad example fails for its own reason.
                eprintln!("{stem}/{name}: {}", errors.join(" | "));
                bad_here += 1;
            } else {
                assert!(errors.is_empty(), "{stem}/{name} does not validate: {errors:#?}");
                good_here += 1;
            }
        }
        assert!(good_here >= 1, "{stem} has no valid example");
        assert!(
            bad_here >= 1,
            "{stem} has no bad example, so nothing shows it refuses anything"
        );
        good += good_here;
        bad += bad_here;
    }
    assert!(good >= 26 && bad >= 26, "examples ran: {good} good, {bad} bad");
}

#[test]
fn every_examples_dir_names_a_schema() {
    let schemas = load_schemas();
    for entry in fs::read_dir(schemas_dir().join("examples")).unwrap() {
        let name = entry.unwrap().file_name().to_string_lossy().into_owned();
        assert!(schemas.contains_key(&name), "examples/{name} matches no schema");
    }
}
