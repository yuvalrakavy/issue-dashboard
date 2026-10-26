//! The sync job's helper (7.2): the page database is reachable only through Claude's artifact
//! tools, so this module plans the writes and the sync skill performs them with `ArtifactData`.
//!
//! A cycle's outbox becomes a *generation*: a marker document `gens/gen-<seq>` (written
//! first), then the outbox's JSON text cut into a few parts under `gens/gen-<seq>/parts/`,
//! then a switch of `meta/active` pinned to the version the skill read. Every write entry is
//! retyped by the model that runs the sync, so a generation is a handful of large documents
//! rather than one per outbox document (10.10). Generation documents are only ever
//! created, so no write is pinned and a name already used is refused by the database itself;
//! a delete of one is pinned to version 1. The switch carries the outbox's fingerprint, and an
//! outbox with the active generation's fingerprint publishes nothing: it writes `meta/heartbeat`
//! alone, so the page still shows when the tracker last synced. Cleanup keeps the new generation and the previous
//! active one, and plans its deletes from a listing of what is there, so a cleanup that
//! stopped part way is finished by the next one.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;
use serde_json::{Value, json};

use crate::canonical::{canonical_json, hash_value};
use crate::ingest::Pages;
use crate::root::format_time;
use crate::{Error, Result, schema};

/// `ArtifactData`'s batch limit.
pub const BATCH_WRITES: usize = 50;
/// Below the 1 MiB batch body, leaving room for the entries themselves.
pub const BATCH_BYTES: usize = 900 * 1024;
/// A part's serialized document, under the database's 256 KiB document limit.
pub const PART_BYTES: usize = 240 * 1024;
/// What a part's document adds to its text: `{"index":<n>,"text":"…"}`, with room for any index.
const PART_ENVELOPE: usize = 64;
/// A generation newer than the active one is another run's until it is this old.
pub const ABANDONED_AFTER: Duration = Duration::hours(1);
/// The outbox's document collections, as the page reads them.
pub const COLLECTIONS: [&str; 7] = [
    "issues",
    "acks",
    "launch_status",
    "suggestions",
    "plans",
    "forks",
    "metrics",
];
/// The collections a generation can hold: `parts`, and one per outbox collection for a
/// generation published before packing, which cleanup lists for a marker without `parts`.
pub const GENERATION_COLLECTIONS: [&str; 8] = [
    "parts",
    "issues",
    "acks",
    "launch_status",
    "suggestions",
    "plans",
    "forks",
    "metrics",
];
/// The page's own collections, which the tracker ingests (5.3).
pub const PAGE_COLLECTIONS: [&str; 4] = ["decisions", "launches", "views", "requests"];

/// The fields of an outbox's `meta` that differ between two runs over the same state.
const PER_RUN_META: [&str; 4] = ["generation", "sequence", "taken_at", "fingerprint"];
/// The fields of each `meta.repos` entry that differ between two runs over the same state: a
/// good read moves `read_at` every run, and the daily full read changes `read`.
const PER_RUN_REPO: [&str; 2] = ["read_at", "read"];

/// `meta/active` as the skill read it.
#[derive(Debug, Clone)]
pub struct Active {
    pub meta: Value,
    pub version: u64,
}

/// `meta/heartbeat` as the skill read it: the last quiet run's document and its version.
#[derive(Debug, Clone)]
pub struct Heartbeat {
    pub doc: Value,
    pub version: u64,
}

/// One `ArtifactData` batch entry.
#[derive(Debug, Clone, Serialize)]
struct Write {
    op: &'static str,
    collection: String,
    doc_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    file_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    if_version: Option<u64>,
}

/// What `c` costs inside a JSON string as `serde_json` writes it.
fn escaped_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Cuts `text` into the fewest parts whose documents stay within `PART_BYTES`, each cut on a
/// character boundary.
fn pack(text: &str) -> Vec<&str> {
    let budget = PART_BYTES - PART_ENVELOPE;
    let mut parts = Vec::new();
    let (mut start, mut size) = (0, 0);
    for (i, c) in text.char_indices() {
        let n = escaped_len(c);
        if size + n > budget {
            parts.push(&text[start..i]);
            (start, size) = (i, 0);
        }
        size += n;
    }
    parts.push(&text[start..]);
    parts
}

fn sequence_of(v: &Value) -> Option<i64> {
    v.get("sequence").and_then(Value::as_i64)
}

fn write_json(path: &Path, v: &Value) -> Result<u64> {
    if let Some(dir) = path.parent() {
        fs::create_dir_all(dir)?;
    }
    let text = serde_json::to_string(v)?;
    fs::write(path, &text)?;
    Ok(text.len() as u64)
}

fn absolute(dir: &Path) -> Result<PathBuf> {
    fs::create_dir_all(dir)?;
    Ok(dir.canonicalize()?)
}

/// Groups writes into batches of at most `BATCH_WRITES` entries and `BATCH_BYTES` of files, in
/// order, and writes each as `<dir>/<n>.json`.
fn batches(writes: Vec<(Write, u64)>, dir: &Path) -> Result<Vec<String>> {
    let mut out: Vec<Vec<Write>> = Vec::new();
    let mut bytes = 0u64;
    for (w, size) in writes {
        let full = out
            .last()
            .is_none_or(|b| b.len() == BATCH_WRITES || (bytes + size) as usize > BATCH_BYTES);
        if full {
            out.push(Vec::new());
            bytes = 0;
        }
        bytes += size;
        out.last_mut().expect("pushed").push(w);
    }
    let mut paths = Vec::new();
    for (i, b) in out.iter().enumerate() {
        let p = dir.join(format!("{:03}.json", i + 1));
        write_json(&p, &serde_json::to_value(b)?)?;
        paths.push(p.display().to_string());
    }
    Ok(paths)
}

/// The outbox's content without what differs between two runs over the same state: `meta`'s
/// generation, sequence and sync time (and a fingerprint already on it), each repo's read time
/// and read mode, and the order of each collection's documents (the database keeps none). An
/// outbox with the active generation's fingerprint is published as `unchanged` (10.6).
pub fn fingerprint(outbox: &Value) -> String {
    let mut meta = outbox.get("meta").cloned().unwrap_or(Value::Null);
    if let Some(m) = meta.as_object_mut() {
        for k in PER_RUN_META {
            m.remove(k);
        }
        for r in m
            .get_mut("repos")
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
            .filter_map(Value::as_object_mut)
        {
            for k in PER_RUN_REPO {
                r.remove(k);
            }
        }
    }
    let mut content = serde_json::Map::new();
    content.insert("meta".into(), meta);
    for c in COLLECTIONS {
        let mut docs: Vec<String> = outbox
            .get(c)
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(canonical_json)
            .collect();
        docs.sort();
        content.insert(c.into(), json!(docs));
    }
    hash_value(&Value::Object(content))
}

/// [`plan_with_heartbeat`] with no `meta/heartbeat` read: an unchanged outbox creates it.
pub fn plan(
    outbox: &Value,
    active: Option<&Active>,
    markers: &BTreeMap<String, Value>,
    now: DateTime<Utc>,
    out: &Path,
) -> Result<Value> {
    plan_with_heartbeat(outbox, active, None, markers, now, out)
}

/// The `unchanged` plan: no generation, switch or cleanup, only one `meta/heartbeat` write
/// pinned to `heartbeat_version` (unpinned when none was read), naming the active generation,
/// this run's sync time and its per-repo reads.
fn unchanged(outbox: &Value, active: &Active, seq: i64, heartbeat_version: Option<u64>, out: &Path) -> Result<Value> {
    let meta = &outbox["meta"];
    let generation = active.meta.get("generation").cloned().unwrap_or(Value::Null);
    let repos: Vec<Value> = meta
        .get("repos")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|r| json!({"repo": r.get("repo"), "read_at": r.get("read_at"), "read": r.get("read")}))
        .collect();
    let heartbeat = json!({
        "schema": 1,
        "project": outbox.get("project"),
        "generation": generation,
        "sequence": seq,
        "synced_at": meta.get("taken_at"),
        "repos": repos,
    });
    schema::validate("doc-heartbeat", "the heartbeat", &heartbeat)?;
    let out = absolute(out)?;
    let path = out.join("docs/meta/heartbeat.json");
    write_json(&path, &heartbeat)?;
    let write = Write {
        op: "set",
        collection: "meta".into(),
        doc_id: "heartbeat".into(),
        file_path: Some(path.display().to_string()),
        if_version: heartbeat_version,
    };
    let summary = json!({
        "decision": "unchanged",
        "generation": generation,
        "reason": format!("{generation} already shows this outbox's content: only meta/heartbeat is written"),
        "heartbeat": [write],
    });
    write_json(&out.join("plan.json"), &summary)?;
    Ok(summary)
}

/// Plans the publish of `outbox` into `out`: the documents, the batches, the switch and the
/// generations to clean once the switch has succeeded. An outbox no newer than the active
/// generation, or than the quiet run that wrote `heartbeat`, is a stop that writes nothing; one
/// with the active generation's fingerprint is `unchanged`, which writes only `meta/heartbeat`
/// (pinned to the version the skill read); a generation name already used is refused.
pub fn plan_with_heartbeat(
    outbox: &Value,
    active: Option<&Active>,
    heartbeat: Option<&Heartbeat>,
    markers: &BTreeMap<String, Value>,
    now: DateTime<Utc>,
    out: &Path,
) -> Result<Value> {
    let heartbeat_version = heartbeat.map(|h| h.version);
    let seq = sequence_of(outbox).ok_or_else(|| Error::refused("the outbox has no sequence"))?;
    let generation = format!("gen-{seq}");
    let meta = outbox
        .get("meta")
        .ok_or_else(|| Error::refused("the outbox has no meta"))?;
    if meta.get("generation").and_then(Value::as_str) != Some(generation.as_str()) {
        return Err(Error::refused(format!("the outbox's meta does not name {generation}")));
    }
    let active_seq = active.and_then(|a| sequence_of(&a.meta));
    if let Some(a) = active_seq.filter(|a| *a >= seq) {
        return Ok(json!({
            "decision": "stop",
            "reason": format!("gen-{a} is active and {generation} is not newer: this outbox is older than what the page shows"),
        }));
    }
    // A quiet run moves no generation, so the last one's sequence counts as well: an outbox
    // older than it would show older state than the page has already confirmed.
    if let Some(q) = heartbeat.and_then(|h| sequence_of(&h.doc)).filter(|q| *q >= seq) {
        return Ok(json!({
            "decision": "stop",
            "reason": format!("a quiet run of sequence {q} already confirmed the page and {generation} is not newer: this outbox is older than what the page shows"),
        }));
    }
    let print = fingerprint(outbox);
    if let Some(a) = active
        && a.meta.get("fingerprint").and_then(Value::as_str) == Some(print.as_str())
    {
        return unchanged(outbox, a, seq, heartbeat_version, out);
    }
    if markers.contains_key(&generation) {
        return Err(Error::refused(format!(
            "{generation} was started before; a generation name is never reused, so run a new cycle"
        )));
    }

    // The page joins the parts in `index` order and checks the result against `parts` and
    // `length` (UTF-16 code units, as JavaScript counts a string) before parsing it.
    let text = serde_json::to_string(outbox)?;
    let parts = pack(&text);
    let out = absolute(out)?;
    let marker = json!({
        "generation": generation,
        "sequence": seq,
        "project": outbox.get("project"),
        "created_at": format_time(now),
        "parts": parts.len(),
        "length": text.encode_utf16().count(),
    });
    let create = |collection: String, doc_id: String, path: &Path| Write {
        op: "set",
        collection,
        doc_id,
        file_path: Some(path.display().to_string()),
        if_version: None,
    };
    let mut writes = Vec::new();
    let mpath = out.join("docs/gens").join(format!("{generation}.json"));
    writes.push((
        create("gens".into(), generation.clone(), &mpath),
        write_json(&mpath, &marker)?,
    ));
    let collection = format!("gens/{generation}/parts");
    for (i, part) in parts.iter().enumerate() {
        let id = format!("p{:04}", i + 1);
        let p = out.join("docs").join(&collection).join(format!("{id}.json"));
        let doc = json!({"index": i + 1, "text": part});
        writes.push((create(collection.clone(), id, &p), write_json(&p, &doc)?));
    }
    let batch_paths = batches(writes, &out.join("batches"))?;

    let spath = out.join("docs/meta/active.json");
    let mut switched = meta.clone();
    switched["fingerprint"] = json!(print);
    write_json(&spath, &switched)?;
    let switch = Write {
        if_version: active.map(|a| a.version),
        ..create("meta".into(), "active".into(), &spath)
    };
    let switch_path = out.join("switch.json");
    write_json(&switch_path, &json!([switch]))?;

    let mut clean = Vec::new();
    for (name, m) in markers {
        let Some(n) = sequence_of(m) else { continue };
        if Some(n) == active_seq || n == seq || name != &format!("gen-{n}") {
            continue;
        }
        let created = m
            .get("created_at")
            .and_then(Value::as_str)
            .and_then(|t| DateTime::parse_from_rfc3339(t).ok())
            .map(|t| t.with_timezone(&Utc));
        let older = active_seq.is_some_and(|a| n < a);
        let abandoned = created.is_none_or(|t| now - t > ABANDONED_AFTER);
        if !older && !abandoned {
            continue;
        }
        // A packed generation holds only its parts; any other marker (published before
        // packing, or unreadable) is listed in every collection a generation can hold.
        let packed = m.get("parts").and_then(Value::as_u64).is_some();
        let list: Vec<String> = GENERATION_COLLECTIONS
            .iter()
            .filter(|c| !packed || **c == "parts")
            .map(|c| format!("gens/{name}/{c}"))
            .collect();
        clean.push(json!({"generation": name, "list": list}));
    }
    let summary = json!({
        "decision": "publish",
        "generation": generation,
        "batches": batch_paths,
        "switch": [switch_path.display().to_string()],
        "clean": clean,
    });
    write_json(&out.join("plan.json"), &summary)?;
    Ok(summary)
}

/// Plans the deletes of the generations `plan` (a publish plan's `plan.json`) marks for
/// cleaning, from `listing`: the skill's `out_dir` listings of them, `gens/<generation>/<collection>/
/// <id>.json`. Each generation's documents go before its marker, every delete pinned to
/// version 1.
pub fn cleanup(plan: &Path, listing: &Path, out: &Path) -> Result<Value> {
    let p: Value = serde_json::from_str(&fs::read_to_string(plan)?)
        .map_err(|e| Error::refused(format!("{}: not a publish plan: {e}", plan.display())))?;
    if p.get("decision").and_then(Value::as_str) != Some("publish") {
        return Err(Error::refused(format!(
            "{}: the plan did not publish, so nothing is cleaned",
            plan.display()
        )));
    }
    let delete = |collection: String, doc_id: String| Write {
        op: "delete",
        collection,
        doc_id,
        file_path: None,
        if_version: Some(1),
    };
    let mut writes = Vec::new();
    let mut cleaned = Vec::new();
    for c in p.get("clean").and_then(Value::as_array).into_iter().flatten() {
        let generation = c
            .get("generation")
            .and_then(Value::as_str)
            .filter(|g| g.strip_prefix("gen-").is_some_and(|n| n.parse::<u64>().is_ok()))
            .ok_or_else(|| Error::refused(format!("{}: a clean entry names no generation", plan.display())))?;
        for collection in GENERATION_COLLECTIONS {
            let dir = listing.join("gens").join(generation).join(collection);
            for id in json_stems(&dir)? {
                writes.push((delete(format!("gens/{generation}/{collection}"), id), 0));
            }
        }
        writes.push((delete("gens".into(), generation.to_string()), 0));
        cleaned.push(generation.to_string());
    }
    let out = absolute(out)?;
    let paths = batches(writes, &out.join("clean"))?;
    Ok(json!({"generations": cleaned, "batches": paths}))
}

/// The stems of the `.json` files in `dir`, sorted; none when it does not exist.
fn json_stems(dir: &Path) -> Result<Vec<String>> {
    let mut out = Vec::new();
    if !dir.exists() {
        return Ok(out);
    }
    for e in fs::read_dir(dir)? {
        let path = e?.path();
        if path.extension().is_some_and(|x| x == "json")
            && let Some(stem) = path.file_stem().and_then(|s| s.to_str())
        {
            out.push(stem.to_string());
        }
    }
    out.sort();
    Ok(out)
}

/// The page documents from the skill's `out_dir` listings of the page collections
/// (`<dir>/<collection>/<id>.json`), for `cycle --pages`.
pub fn pages(dir: &Path) -> Result<Pages> {
    let mut by: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    for c in PAGE_COLLECTIONS {
        let d = dir.join(c);
        let mut docs = Vec::new();
        for stem in json_stems(&d)? {
            let path = d.join(format!("{stem}.json"));
            let text = fs::read_to_string(&path)?;
            docs.push(
                serde_json::from_str(&text)
                    .map_err(|e| Error::refused(format!("{}: not a JSON document: {e}", path.display())))?,
            );
        }
        by.insert(c, docs);
    }
    let mut take = |c: &str| by.remove(c).unwrap_or_default();
    Ok(Pages {
        decisions: take("decisions"),
        launches: take("launches"),
        views: take("views"),
        requests: take("requests"),
    })
}
