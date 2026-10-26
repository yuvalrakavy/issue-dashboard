//! The sync job's publish helper (7.2): an outbox becomes a generation of page documents in
//! batches, a pinned switch of `meta/active`, and a cleanup that keeps the previous
//! generation. The page database is only reachable through Claude's artifact tools, so the
//! helper plans the writes and the sync skill performs them; these tests drive the plans.

mod common;

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use common::fixture::*;
use issue_dashboard_core::cycle;
use issue_dashboard_core::publish::{self, Active, BATCH_BYTES, BATCH_WRITES};
use serde_json::{Value, json};

/// A real outbox: the fixture's home cycle (Store, airzoned and rhai, ~150 issues).
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
            "parts": 2,
            "length": 1000,
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

fn out_dir() -> tempfile::TempDir {
    tempfile::tempdir().unwrap()
}

/// The parts a plan writes, in the order of their `index`, and the marker it writes first.
fn parts_of(bs: &[Vec<Value>], name: &str) -> (Value, Vec<Value>) {
    let all: Vec<&Value> = bs.iter().flatten().collect();
    let m = read(all[0]["file_path"].as_str().unwrap());
    assert_eq!((all[0]["collection"].as_str(), all[0]["doc_id"].as_str()), (Some("gens"), Some(name)));
    let mut parts: Vec<Value> = all[1..]
        .iter()
        .map(|w| {
            assert_eq!(w["collection"], json!(format!("gens/{name}/parts")), "{w}");
            read(w["file_path"].as_str().unwrap())
        })
        .collect();
    parts.sort_by_key(|p| p["index"].as_u64().unwrap());
    (m, parts)
}

fn joined(parts: &[Value]) -> String {
    parts.iter().map(|p| p["text"].as_str().unwrap()).collect()
}

#[test]
fn a_first_publish_writes_the_marker_and_the_outbox_packed_into_parts_in_one_batch_and_an_unpinned_switch() {
    let outbox = real_outbox();
    let seq = outbox["sequence"].as_i64().unwrap();
    let name = format!("gen-{seq}");
    let out = out_dir();
    let s = publish::plan(&outbox, None, &BTreeMap::new(), now(), out.path()).unwrap();
    assert_eq!(s["decision"], "publish", "{s}");
    assert_eq!(s["generation"], json!(name));
    let bs = batches(&s, "batches");
    assert_eq!(bs.len(), 1, "a whole outbox is one batch: {:?}", bs.iter().map(Vec::len).collect::<Vec<_>>());
    let mut ids = BTreeSet::new();
    for w in &bs[0] {
        assert_eq!(w["op"], "set");
        // A generation is only ever created: no entry is pinned, so one that exists is refused.
        assert_eq!(w.get("if_version"), None, "{w}");
        assert!(Path::new(w["file_path"].as_str().unwrap()).is_absolute());
        assert!(ids.insert((w["collection"].to_string(), w["doc_id"].to_string())), "{w}");
    }
    let (m, parts) = parts_of(&bs, &name);
    assert_eq!(m["sequence"], json!(seq));
    assert_eq!(m["parts"], json!(parts.len()));
    assert_eq!(
        parts.iter().map(|p| p["index"].as_u64().unwrap()).collect::<Vec<_>>(),
        (1..=parts.len() as u64).collect::<Vec<_>>()
    );
    // The page joins the parts and parses the outbox back, exactly.
    let text = joined(&parts);
    assert_eq!(m["length"], json!(text.encode_utf16().count()));
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), outbox);
    let switch = batches(&s, "switch");
    assert_eq!(switch.len(), 1);
    let sw = &switch[0][0];
    assert_eq!(
        (sw["collection"].as_str(), sw["doc_id"].as_str()),
        (Some("meta"), Some("active"))
    );
    assert_eq!(sw.get("if_version"), None, "nothing to pin on a first publish");
    // The outbox's meta, plus the fingerprint the next run compares its outbox with.
    let mut switched = outbox["meta"].clone();
    switched["fingerprint"] = json!(publish::fingerprint(&outbox));
    assert_eq!(read(sw["file_path"].as_str().unwrap()), switched);
    assert_eq!(s["clean"], json!([]));
}

#[test]
fn the_switch_is_pinned_to_the_version_of_the_active_meta_that_was_read() {
    let outbox = with_sequence(real_outbox(), 500);
    let out = out_dir();
    let markers = BTreeMap::from([marker(400, now() - Duration::minutes(30))]);
    let s = publish::plan(&outbox, Some(&active(400, 7)), &markers, now(), out.path()).unwrap();
    assert_eq!(batches(&s, "switch")[0][0]["if_version"], json!(7));
    // The active generation becomes the previous one, and is kept.
    assert_eq!(s["clean"], json!([]));
}

#[test]
fn an_outbox_no_newer_than_the_active_generation_publishes_nothing() {
    for older in [400, 399] {
        let outbox = with_sequence(real_outbox(), older);
        let out = out_dir();
        let markers = BTreeMap::from([marker(400, now())]);
        let s = publish::plan(&outbox, Some(&active(400, 3)), &markers, now(), out.path()).unwrap();
        assert_eq!(s["decision"], "stop", "{s}");
        assert!(s["reason"].as_str().unwrap().contains("gen-400"), "{s}");
        assert_eq!(fs::read_dir(out.path()).unwrap().count(), 0, "a stop writes no files");
    }
}

#[test]
fn a_generation_name_is_never_reused_even_when_the_earlier_attempt_was_abandoned() {
    let outbox = with_sequence(real_outbox(), 500);
    let out = out_dir();
    let markers = BTreeMap::from([marker(400, now()), marker(500, now() - Duration::hours(5))]);
    let e = publish::plan(&outbox, Some(&active(400, 3)), &markers, now(), out.path()).unwrap_err();
    assert_eq!(e.exit_code(), 1);
    assert!(e.to_string().contains("gen-500"), "{e}");
    assert_eq!(fs::read_dir(out.path()).unwrap().count(), 0);
}

#[test]
fn cleanup_keeps_the_new_and_the_previous_generation_and_an_attempt_still_in_flight() {
    let outbox = with_sequence(real_outbox(), 500);
    let out = out_dir();
    let markers = BTreeMap::from([
        marker(300, now() - Duration::minutes(5)), // older than the active one: done with
        marker(390, now() - Duration::days(2)),    // older still
        marker(400, now() - Duration::days(1)),    // the active one: becomes the previous
        marker(450, now() - Duration::hours(2)),   // abandoned: its switch never happened
        marker(470, now() - Duration::minutes(20)), // another run's, perhaps still writing
    ]);
    let s = publish::plan(&outbox, Some(&active(400, 3)), &markers, now(), out.path()).unwrap();
    let clean: Vec<&str> = s["clean"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["generation"].as_str().unwrap())
        .collect();
    assert_eq!(clean, ["gen-300", "gen-390", "gen-450"]);
    // A packed generation holds only its parts.
    assert_eq!(s["clean"][0]["list"], json!(["gens/gen-300/parts"]));
}

#[test]
fn a_marker_without_parts_or_a_readable_time_is_listed_in_full_and_counts_as_old() {
    let outbox = with_sequence(real_outbox(), 500);
    let out = out_dir();
    let markers = BTreeMap::from([
        marker(400, now()),
        ("gen-450".to_string(), json!({"generation": "gen-450", "sequence": 450})),
        // A generation published before packing: a document per outbox document.
        (
            "gen-460".to_string(),
            json!({"generation": "gen-460", "sequence": 460, "created_at": now().to_rfc3339(),
                   "counts": {"issues": 3, "acks": 0}}),
        ),
    ]);
    let s = publish::plan(&outbox, Some(&active(400, 3)), &markers, now(), out.path()).unwrap();
    assert_eq!(s["clean"][0]["generation"], "gen-450");
    let full = json!([
        "gens/gen-450/parts",
        "gens/gen-450/issues",
        "gens/gen-450/acks",
        "gens/gen-450/launch_status",
        "gens/gen-450/suggestions",
        "gens/gen-450/plans",
        "gens/gen-450/forks",
        "gens/gen-450/metrics"
    ]);
    assert_eq!(s["clean"][0]["list"], full);
    // gen-460 is newer than the active one and young, so it is kept; it is not asked about here.
    assert_eq!(s["clean"].as_array().unwrap().len(), 1, "{s}");
}

#[test]
fn a_legacy_generation_older_than_the_active_one_is_listed_in_every_collection() {
    let outbox = with_sequence(real_outbox(), 500);
    let out = out_dir();
    let legacy = json!({"generation": "gen-300", "sequence": 300, "created_at": now().to_rfc3339(),
                        "counts": {"issues": 3, "acks": 0}});
    let markers = BTreeMap::from([marker(400, now()), ("gen-300".to_string(), legacy)]);
    let s = publish::plan(&outbox, Some(&active(400, 3)), &markers, now(), out.path()).unwrap();
    assert_eq!(s["clean"][0]["list"].as_array().unwrap().len(), 8, "{s}");
}

fn listing(dir: &Path, generation: &str, docs: &[(&str, &str)]) {
    for (collection, id) in docs {
        let d = dir.join("gens").join(generation).join(collection);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(format!("{id}.json")), json!({"id": id}).to_string()).unwrap();
    }
}

fn plan_with_clean(out: &Path, clean: &[&str]) -> PathBuf {
    let p = out.join("plan.json");
    let clean: Vec<Value> = clean.iter().map(|g| json!({"generation": g, "list": []})).collect();
    fs::write(
        &p,
        json!({"decision": "publish", "generation": "gen-500", "clean": clean}).to_string(),
    )
    .unwrap();
    p
}

#[test]
fn cleanup_deletes_what_the_listing_holds_pinned_to_version_one_with_each_marker_last() {
    let out = out_dir();
    let list = out_dir();
    let mut many: Vec<(&str, String)> = (0..70).map(|i| ("issues", format!("I_{i}"))).collect();
    many.push(("launch_status", "L_1".into()));
    let many: Vec<(&str, &str)> = many.iter().map(|(c, i)| (*c, i.as_str())).collect();
    listing(list.path(), "gen-300", &many);
    // A partly cleaned generation: only what is still there is deleted.
    listing(list.path(), "gen-450", &[("issues", "I_x")]);
    // The kept generations' documents may be in the listing too; they are never touched.
    listing(list.path(), "gen-400", &[("issues", "I_keep")]);
    let plan = plan_with_clean(out.path(), &["gen-300", "gen-450"]);
    let s = publish::cleanup(&plan, list.path(), out.path()).unwrap();
    let bs = batches(&s, "batches");
    let all: Vec<&Value> = bs.iter().flatten().collect();
    assert!(bs.iter().all(|b| b.len() <= BATCH_WRITES));
    assert!(
        all.iter()
            .all(|w| w["op"] == "delete" && w["if_version"] == json!(1) && w.get("file_path").is_none())
    );
    assert_eq!(all.len(), 71 + 1 + 1 + 1);
    assert!(!all.iter().any(|w| w["doc_id"] == "I_keep" || w["doc_id"] == "gen-400"));
    for g in ["gen-300", "gen-450"] {
        let marker = all
            .iter()
            .position(|w| w["collection"] == "gens" && w["doc_id"] == g)
            .unwrap();
        let last_doc = all
            .iter()
            .rposition(|w| w["collection"].as_str().unwrap().starts_with(&format!("gens/{g}/")))
            .unwrap();
        assert!(marker > last_doc, "{g}: the marker goes after its documents");
    }
}

#[test]
fn cleanup_refuses_a_plan_that_did_not_publish() {
    let out = out_dir();
    let p = out.path().join("plan.json");
    fs::write(&p, json!({"decision": "stop", "reason": "older"}).to_string()).unwrap();
    assert_eq!(publish::cleanup(&p, out.path(), out.path()).unwrap_err().exit_code(), 1);
}

/// Text that serializes larger than it reads: the outbox's JSON already escapes quotes,
/// backslashes, newlines and control characters, so packing that text escapes them a second
/// time, and multi-byte characters take several bytes.
const AWKWARD: &str = "é\"\\\n\u{1}😀 plain ";

#[test]
fn every_part_stays_under_240_kib_once_serialized_and_no_part_splits_a_character() {
    let mut outbox = real_outbox();
    // One document far over the database's limit: packing splits it across parts.
    outbox["issues"][0]["body"] = json!(AWKWARD.repeat(60_000));
    let name = format!("gen-{}", outbox["sequence"]);
    let out = out_dir();
    let s = publish::plan(&outbox, None, &BTreeMap::new(), now(), out.path()).unwrap();
    let bs = batches(&s, "batches");
    for w in bs.iter().flatten().skip(1) {
        let size = fs::metadata(w["file_path"].as_str().unwrap()).unwrap().len();
        assert!(size <= 240 * 1024, "{} is {size} bytes", w["doc_id"]);
    }
    let (m, parts) = parts_of(&bs, &name);
    assert!(parts.len() >= 4, "{} parts", parts.len());
    // A part read back is valid JSON text on its own (no half character), and the parts
    // join back to the outbox.
    let text = joined(&parts);
    assert_eq!(serde_json::from_str::<Value>(&text).unwrap(), outbox);
    assert_eq!(m["length"], json!(text.encode_utf16().count()));
    // Packed tightly: every part but the last is within a few bytes of the limit.
    for w in &bs.iter().flatten().skip(1).collect::<Vec<_>>()[..parts.len() - 1] {
        let size = fs::metadata(w["file_path"].as_str().unwrap()).unwrap().len();
        assert!(size > 240 * 1024 - 80, "{} is only {size} bytes", w["doc_id"]);
    }
}

#[test]
fn an_outbox_larger_than_a_batch_splits_into_batches_by_size() {
    let mut outbox = real_outbox();
    let issues = outbox["issues"].as_array_mut().unwrap();
    issues.truncate(20);
    for i in issues.iter_mut() {
        i["body"] = json!("x".repeat(200 * 1024));
    }
    let out = out_dir();
    let s = publish::plan(&outbox, None, &BTreeMap::new(), now(), out.path()).unwrap();
    let bs = batches(&s, "batches");
    // About 4 MiB of text: 17 parts of 240 KiB, three to a 900 KiB batch.
    assert!(bs.len() >= 5, "{} batches", bs.len());
    for b in &bs {
        assert!(b.len() <= BATCH_WRITES);
        let bytes: u64 = b
            .iter()
            .map(|w| fs::metadata(w["file_path"].as_str().unwrap()).unwrap().len())
            .sum();
        assert!(bytes as usize <= BATCH_BYTES, "{bytes}");
    }
}

#[test]
fn pages_are_assembled_from_the_listed_page_collections() {
    let dir = out_dir();
    for (c, id) in [
        ("decisions", "D_2"),
        ("decisions", "D_1"),
        ("launches", "_thread"),
        ("requests", "R_1"),
    ] {
        let d = dir.path().join(c);
        fs::create_dir_all(&d).unwrap();
        fs::write(d.join(format!("{id}.json")), json!({"id": id}).to_string()).unwrap();
    }
    fs::write(dir.path().join("decisions/notes.txt"), "not a document").unwrap();
    let p = publish::pages(dir.path()).unwrap();
    let ids = |v: &Vec<Value>| {
        v.iter()
            .map(|d| d["id"].as_str().unwrap().to_string())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&p.decisions), ["D_1", "D_2"]);
    assert_eq!(ids(&p.launches), ["_thread"]);
    assert!(p.views.is_empty());
    assert_eq!(ids(&p.requests), ["R_1"]);
    fs::create_dir_all(dir.path().join("views")).unwrap();
    fs::write(dir.path().join("views/V_1.json"), "{not json").unwrap();
    let e = publish::pages(dir.path()).unwrap_err();
    assert!(e.to_string().contains("V_1.json"), "{e}");
}
