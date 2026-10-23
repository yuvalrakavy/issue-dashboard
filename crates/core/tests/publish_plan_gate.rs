//! Gate (mutation pass, slice 3): guards of the publish and cleanup plans that no test noticed
//! losing. Each test names the mutation it kills.

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use chrono::{DateTime, Duration, Utc};
use common::fixture::*;
use issue_dashboard_core::cycle;
use issue_dashboard_core::publish::{self, Active, BATCH_BYTES, BATCH_WRITES, DOC_BYTES};
use serde_json::{Value, json};

fn real_outbox() -> Value {
    let t = fixture_root();
    let r = cycle::cycle(&t.root, &gh(), "home", &no_pages(), &|| at(0), PAGE).unwrap();
    serde_json::from_str(&fs::read_to_string(&r.outbox).unwrap()).unwrap()
}

fn with_sequence(mut outbox: Value, seq: i64) -> Value {
    outbox["sequence"] = json!(seq);
    outbox["meta"]["sequence"] = json!(seq);
    outbox["meta"]["generation"] = json!(format!("gen-{seq}"));
    outbox
}

fn now() -> DateTime<Utc> {
    at(60)
}

fn marker(seq: i64, created: DateTime<Utc>) -> (String, Value) {
    (
        format!("gen-{seq}"),
        json!({
            "generation": format!("gen-{seq}"),
            "sequence": seq,
            "project": "home",
            "created_at": created.to_rfc3339(),
            "counts": {"issues": 3, "acks": 0, "launch_status": 1, "suggestions": 0, "plans": 0, "forks": 0, "metrics": 0},
        }),
    )
}

fn active(seq: i64, version: u64) -> Active {
    Active {
        meta: json!({"generation": format!("gen-{seq}"), "sequence": seq}),
        version,
    }
}

fn read(path: &str) -> Value {
    serde_json::from_str(&fs::read_to_string(path).unwrap()).unwrap()
}

fn batches(summary: &Value, key: &str) -> Vec<Vec<Value>> {
    summary[key]
        .as_array()
        .unwrap()
        .iter()
        .map(|p| read(p.as_str().unwrap()).as_array().unwrap().clone())
        .collect()
}

fn batch_bytes(b: &[Value]) -> usize {
    b.iter()
        .map(|w| fs::metadata(w["file_path"].as_str().unwrap()).unwrap().len() as usize)
        .sum()
}

/// An outbox of only `issues`, so the sizes of a batch are exactly the test's.
fn bare(issues: Vec<Value>) -> Value {
    let mut o = with_sequence(real_outbox(), 500);
    for c in publish::COLLECTIONS {
        o[c] = json!([]);
    }
    o["issues"] = json!(issues);
    o
}

fn padded(id: &str, pad: usize) -> Value {
    json!({"id": id, "pad": "x".repeat(pad)})
}

/// A document of exactly `size` serialized bytes.
fn sized(id: &str, size: usize) -> Value {
    let base = serde_json::to_string(&padded(id, 0)).unwrap().len();
    let d = padded(id, size - base);
    assert_eq!(serde_json::to_string(&d).unwrap().len(), size);
    d
}

fn nested(dir: &tempfile::TempDir) -> std::path::PathBuf {
    dir.path().join("not/yet/there")
}

// Kills: `id != "."` and `id != ".."` removed from doc_id.
#[test]
fn a_document_id_of_dots_cannot_name_a_database_document() {
    for bad in [".", ".."] {
        let mut outbox = real_outbox();
        outbox["acks"] = json!([{"id": bad}]);
        let out = tempfile::tempdir().unwrap();
        let e = publish::plan(&outbox, None, &BTreeMap::new(), now(), &nested(&out)).unwrap_err();
        assert_eq!(e.exit_code(), 1, "{bad}: {e}");
    }
}

// Kills: `(1..=200)` widened to `(0..=200)` in doc_id.
#[test]
fn an_empty_document_id_is_refused_and_a_200_byte_one_is_not() {
    let mut outbox = real_outbox();
    outbox["acks"] = json!([{"id": ""}]);
    let out = tempfile::tempdir().unwrap();
    let e = publish::plan(&outbox, None, &BTreeMap::new(), now(), &nested(&out)).unwrap_err();
    assert_eq!(e.exit_code(), 1, "{e}");
    let mut ok = real_outbox();
    ok["acks"] = json!([{"id": "a".repeat(200)}]);
    let s = publish::plan(&ok, None, &BTreeMap::new(), now(), &nested(&out)).unwrap();
    assert_eq!(s["decision"], "publish");
}

// Kills: the meta.generation check removed from plan.
#[test]
fn an_outbox_whose_meta_names_another_generation_or_no_meta_is_refused_before_any_file() {
    let mut other = with_sequence(real_outbox(), 500);
    other["meta"]["generation"] = json!("gen-499");
    let mut no_meta = with_sequence(real_outbox(), 500);
    no_meta.as_object_mut().unwrap().remove("meta");
    let mut no_seq = real_outbox();
    no_seq.as_object_mut().unwrap().remove("sequence");
    for outbox in [other, no_meta, no_seq] {
        let out = tempfile::tempdir().unwrap();
        let dir = nested(&out);
        let e = publish::plan(&outbox, None, &BTreeMap::new(), now(), &dir).unwrap_err();
        assert_eq!(e.exit_code(), 1, "{e}");
        assert!(!dir.exists(), "a refusal writes nothing");
    }
}

// Kills: `size > DOC_BYTES` becoming `>=`; and the check running after files are written.
#[test]
fn a_document_of_exactly_the_database_limit_is_accepted_and_one_byte_more_is_refused_before_any_file() {
    let out = tempfile::tempdir().unwrap();
    let ok = bare(vec![sized("I_1", DOC_BYTES)]);
    let s = publish::plan(&ok, None, &BTreeMap::new(), now(), &out.path().join("ok")).unwrap();
    assert_eq!(s["decision"], "publish");
    let over = bare(vec![sized("I_1", DOC_BYTES + 1)]);
    let dir = out.path().join("over");
    let e = publish::plan(&over, None, &BTreeMap::new(), now(), &dir).unwrap_err();
    assert!(e.to_string().contains("256 KiB"), "{e}");
    assert!(!dir.exists(), "an over-limit document is refused before any file is written");
}

// Kills: `(bytes + size) > BATCH_BYTES` becoming `>=`.
#[test]
fn a_batch_may_hold_exactly_the_byte_limit() {
    // Learn the marker's size, then fill one batch to exactly BATCH_BYTES.
    let probe_out = tempfile::tempdir().unwrap();
    let probe = bare(vec![sized("I_1", 1000)]);
    let s = publish::plan(&probe, None, &BTreeMap::new(), now(), probe_out.path()).unwrap();
    let marker_size = fs::metadata(batches(&s, "batches")[0][0]["file_path"].as_str().unwrap())
        .unwrap()
        .len() as usize;
    let rest = BATCH_BYTES - marker_size - 3 * DOC_BYTES;
    assert!(rest <= DOC_BYTES && rest > 1000);
    let outbox = bare(vec![
        sized("I_1", DOC_BYTES),
        sized("I_2", DOC_BYTES),
        sized("I_3", DOC_BYTES),
        sized("I_4", rest),
    ]);
    let out = tempfile::tempdir().unwrap();
    let s = publish::plan(&outbox, None, &BTreeMap::new(), now(), out.path()).unwrap();
    let bs = batches(&s, "batches");
    assert_eq!(batch_bytes(&bs[0]), BATCH_BYTES, "the setup must fill the batch exactly");
    assert_eq!(bs.len(), 1, "a batch of exactly the limit is not split");
    assert_eq!(bs[0].len(), 5);
}

// Kills: `bytes = 0` removed when a batch is started (every later document then gets its own batch).
#[test]
fn a_new_batch_starts_counting_bytes_from_zero() {
    let mut outbox = real_outbox();
    let issues = outbox["issues"].as_array_mut().unwrap();
    issues.truncate(20);
    for i in issues.iter_mut() {
        i["body"] = json!("x".repeat(200 * 1024));
    }
    for c in publish::COLLECTIONS.iter().filter(|c| **c != "issues") {
        outbox[*c] = json!([]);
    }
    let out = tempfile::tempdir().unwrap();
    let s = publish::plan(&outbox, None, &BTreeMap::new(), now(), out.path()).unwrap();
    let bs = batches(&s, "batches");
    for b in &bs {
        assert!(batch_bytes(b) <= BATCH_BYTES);
    }
    // 4 documents of ~200 KiB fit under 900 KiB, a fifth does not: 20 documents are 5 batches.
    assert_eq!(bs.len(), 5, "{:?}", bs.iter().map(Vec::len).collect::<Vec<_>>());
    assert_eq!(bs.iter().map(Vec::len).sum::<usize>(), 21, "the marker and 20 documents");
}

// Kills: the marker's project, created_at.
#[test]
fn the_marker_carries_the_project_and_a_creation_time_cleanup_can_read_back() {
    let outbox = with_sequence(real_outbox(), 500);
    let out = tempfile::tempdir().unwrap();
    let s = publish::plan(&outbox, None, &BTreeMap::new(), now(), out.path()).unwrap();
    let m = read(batches(&s, "batches")[0][0]["file_path"].as_str().unwrap());
    assert_eq!(m["project"], outbox["project"]);
    assert!(outbox["project"].is_string());
    let created = DateTime::parse_from_rfc3339(m["created_at"].as_str().unwrap()).unwrap();
    assert_eq!(created.with_timezone(&Utc), now());
    assert_eq!(m["generation"], "gen-500");
}

fn clean_names(s: &Value) -> Vec<String> {
    s["clean"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["generation"].as_str().unwrap().to_string())
        .collect()
}

// Kills: `name != gen-<n>` removed; `older` true when there is no active generation;
// `>` becoming `>=` at the abandon age.
#[test]
fn cleanup_only_plans_well_named_markers_and_waits_for_a_generation_with_no_active_one() {
    let outbox = with_sequence(real_outbox(), 500);
    let out = tempfile::tempdir().unwrap();
    let mut odd = marker(100, now() - Duration::days(3)).1;
    odd["generation"] = json!("notes");
    let mut renamed = marker(200, now() - Duration::days(3)).1;
    renamed["generation"] = json!("gen-300");
    let markers = BTreeMap::from([
        ("notes".to_string(), odd),
        ("gen-300".to_string(), renamed), // key says 300, its sequence says 200
        marker(310, now() - Duration::minutes(5)), // in flight: nothing is active to be older than
        marker(320, now() - Duration::hours(1)),   // exactly the abandon age: not over it
        marker(330, now() - Duration::hours(1) - Duration::seconds(1)),
    ]);
    let s = publish::plan(&outbox, None, &markers, now(), out.path()).unwrap();
    assert_eq!(clean_names(&s), ["gen-330"], "{s}");
}

// Kills: `None => counts.is_none()` becoming `true` (a count missing from a present counts object).
#[test]
fn a_collection_missing_from_a_markers_counts_is_not_listed() {
    let outbox = with_sequence(real_outbox(), 500);
    let out = tempfile::tempdir().unwrap();
    let mut m = marker(300, now() - Duration::days(1)).1;
    m["counts"] = json!({"issues": 2});
    let markers = BTreeMap::from([marker(400, now()), ("gen-300".to_string(), m)]);
    let s = publish::plan(&outbox, Some(&active(400, 1)), &markers, now(), out.path()).unwrap();
    assert_eq!(s["clean"][0]["list"], json!(["gens/gen-300/issues"]));
}

fn plan_naming(dir: &Path, generation: &str) -> std::path::PathBuf {
    let p = dir.join("plan.json");
    fs::write(
        &p,
        json!({"decision": "publish", "generation": "gen-500", "clean": [{"generation": generation, "list": []}]}).to_string(),
    )
    .unwrap();
    p
}

// Kills: the generation-name filter in cleanup removed or loosened.
#[test]
fn cleanup_refuses_a_clean_entry_that_is_not_a_numbered_generation() {
    for bad in ["meta", "gens", "gen-", "gen-abc", "gen-1/../../meta", "active"] {
        let dir = tempfile::tempdir().unwrap();
        let plan = plan_naming(dir.path(), bad);
        let e = publish::cleanup(&plan, dir.path(), &dir.path().join("o")).unwrap_err();
        assert_eq!(e.exit_code(), 1, "{bad}: {e}");
        assert!(!dir.path().join("o").exists(), "{bad}");
    }
}

// Kills: cleanup visiting only some of the collections; `generations` list; any delete outside gens.
#[test]
fn cleanup_deletes_every_collection_of_a_generation_and_nothing_outside_gens() {
    let dir = tempfile::tempdir().unwrap();
    let list = tempfile::tempdir().unwrap();
    for c in publish::COLLECTIONS {
        let d = list.path().join("gens/gen-300").join(c);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("D_1.json"), "{}").unwrap();
    }
    // Page collections and the pointer may sit in the same listing directory; they are not gens.
    for c in publish::PAGE_COLLECTIONS.iter().chain(&["meta"]) {
        let d = list.path().join(c);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("active.json"), "{}").unwrap();
        let d = list.path().join("gens/gen-300").join(c);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join("active.json"), "{}").unwrap();
    }
    let plan = plan_naming(dir.path(), "gen-300");
    let s = publish::cleanup(&plan, list.path(), &dir.path().join("o")).unwrap();
    assert_eq!(s["generations"], json!(["gen-300"]));
    let bs = batches(&s, "batches");
    let all: Vec<&Value> = bs.iter().flatten().collect();
    assert_eq!(all.len(), publish::COLLECTIONS.len() + 1);
    for c in publish::COLLECTIONS {
        let want = format!("gens/gen-300/{c}");
        assert!(all.iter().any(|w| w["collection"] == want.as_str()), "{want}");
    }
    for w in &all {
        let c = w["collection"].as_str().unwrap();
        assert!(c == "gens" || c.starts_with("gens/gen-300/"), "{c}");
        assert_ne!(w["doc_id"], "active");
    }
    assert!(BATCH_WRITES > all.len());
}
