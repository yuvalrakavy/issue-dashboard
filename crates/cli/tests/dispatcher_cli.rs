//! The dispatcher's commands end to end, as separate processes over the recorded GitHub
//! responses: a recommend launch reserved from the owner's document, its brief, the
//! suggestions its session records, and the refusals (7.3, 7.5, 5.8).

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

use serde_json::{Value, json};

const BIN: &str = env!("CARGO_BIN_EXE_issue-dashboard");
const D: &str = "local_dispatcher";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../core/tests/fixtures")
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

struct Tracker {
    dir: tempfile::TempDir,
    now: String,
}

impl Tracker {
    fn new() -> Tracker {
        let dir = tempfile::tempdir().unwrap();
        copy_dir(&fixtures().join("config"), dir.path());
        let meta: Value =
            serde_json::from_str(&fs::read_to_string(fixtures().join("replay-meta.json")).unwrap()).unwrap();
        Tracker {
            dir,
            now: meta["recorded_at"].as_str().unwrap().to_string(),
        }
    }

    fn cmd(&self, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env("ISSUE_DASHBOARD_ROOT", self.dir.path())
            .env("ISSUE_DASHBOARD_NOW", &self.now)
            .env("ISSUE_DASHBOARD_REPLAY", fixtures().join("replay"))
            .env("ISSUE_DASHBOARD_PAGE_SIZE", "25")
            .env_remove("ISSUE_DASHBOARD_RECORD")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Value {
        let out = self.cmd(args);
        assert!(out.status.success(), "{args:?} failed: {}", String::from_utf8_lossy(&out.stderr));
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Runs a command that must be refused (exit 1) and returns its message.
    fn refused(&self, args: &[&str]) -> String {
        let out = self.cmd(args);
        assert_eq!(out.status.code(), Some(1), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8_lossy(&out.stderr).into_owned()
    }

    fn write(&self, name: &str, v: &Value) -> String {
        let p = self.dir.path().join(name);
        fs::write(&p, v.to_string()).unwrap();
        p.display().to_string()
    }

    fn outbox(&self) -> Value {
        let r = self.ok(&["cycle", "--project", "home"]);
        serde_json::from_str(&fs::read_to_string(r["outbox"].as_str().unwrap()).unwrap()).unwrap()
    }
}

fn open_store_issues(d: &Value) -> Vec<(String, String)> {
    d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["alias"].as_str().unwrap().starts_with("yuvalrakavy/Store#") && i["state"] == "open" && i["kind"] == "issue")
        .map(|i| (i["id"].as_str().unwrap().to_string(), i["alias"].as_str().unwrap().to_string()))
        .collect()
}

fn launch_doc(id: &str, author: &str, items: &[(String, String)]) -> Value {
    json!({
        "id": id,
        "project": "home",
        "author": author,
        "created_at": "2026-10-23T08:00:00Z",
        "kind": "recommend_closures",
        "items": items.iter().map(|i| &i.0).collect::<Vec<_>>(),
        "aliases": items.iter().map(|i| &i.1).collect::<Vec<_>>(),
        "recipe": "read-only",
        "routing": {"model": "claude-opus-5-5", "effort": "medium"},
    })
}

#[test]
fn a_recommend_launch_from_reserve_to_inbox() {
    let t = Tracker::new();
    let open = open_store_issues(&t.outbox());
    let items = vec![open[0].clone(), open[1].clone()];
    let doc = t.write("L1.json", &launch_doc("L1", "user_owner", &items));

    let r = t.ok(&["reserve", "--request", &doc, "--dispatcher", D]);
    assert_eq!(r["result"], "reserved", "{r}");
    let token = r["token"].as_str().unwrap().to_string();

    let b = t.ok(&["brief", "--launch", "L1"]);
    let title = b["title"].as_str().unwrap();
    assert!(title.ends_with(&token) && title.chars().count() <= 60, "{title}");
    let text = b["text"].as_str().unwrap();
    assert!(text.contains(&format!("--root {}", t.dir.path().display())), "the brief names the root");
    assert!(text.contains("record-suggestions --launch L1"), "{text}");
    assert!(b["pieces"].as_array().unwrap().iter().all(|p| p["source"].is_string()));

    t.ok(&["launch-started", "--token", &token, "--session", "local_recommender"]);
    let live = t.ok(&["launches", "--live"]);
    let row = &live["launches"][0];
    assert_eq!((row["id"].as_str(), row["state"].as_str()), (Some("L1"), Some("started")), "{live}");
    assert_eq!(row["kind"], "recommend_closures");
    assert_eq!(row["outcome"], false);

    let file = t.write(
        "suggestions.json",
        &json!({"suggestions": [{"kind": "obsolete", "item": items[0].1, "confidence": 0.6, "reason": "superseded"}],
                "left_out": [{"item": items[1].1, "why": "keep_open", "reason": "still wanted"}]}),
    );
    let rec = t.ok(&["record-suggestions", "--launch", "L1", &file]);
    assert_eq!(rec["recorded"], 2, "{rec}");

    let d = t.outbox();
    let kinds: Vec<&str> = d["suggestions"].as_array().unwrap().iter().map(|s| s["kind"].as_str().unwrap()).collect();
    assert_eq!(kinds.len(), 2, "{kinds:?}");
    assert!(kinds.contains(&"obsolete") && kinds.contains(&"left_out"), "{kinds:?}");
    let meta_repos = d["meta"]["repos"].as_array().unwrap();
    assert!(meta_repos.iter().all(|r| r["campaign_skill"] == false && r.get("recipe").is_some()), "{meta_repos:?}");
}

#[test]
fn a_launch_document_not_written_by_the_owner_is_refused() {
    let t = Tracker::new();
    let open = open_store_issues(&t.outbox());
    let doc = t.write("L1.json", &launch_doc("L1", "user_someone_else", &[open[0].clone()]));
    let m = t.refused(&["reserve", "--request", &doc, "--dispatcher", D]);
    assert!(m.contains("author user_someone_else is not project home's owner_id"), "{m}");
    assert_eq!(t.ok(&["launches"])["launches"], json!([]), "nothing was recorded");
}

#[test]
fn a_bad_suggestions_file_exits_1_with_every_reason() {
    let t = Tracker::new();
    let open = open_store_issues(&t.outbox());
    let doc = t.write("L1.json", &launch_doc("L1", "user_owner", &[open[0].clone()]));
    t.ok(&["reserve", "--request", &doc, "--dispatcher", D]);
    let file = t.write(
        "bad.json",
        &json!({"suggestions": [
            {"kind": "likely_fixed", "item": open[0].1, "confidence": 0.5, "reason": "r"},
            {"kind": "obsolete", "item": "yuvalrakavy/Store#999999", "confidence": 0.5, "reason": "r"},
        ]}),
    );
    let m = t.refused(&["record-suggestions", "--launch", "L1", &file]);
    assert!(m.contains("likely_fixed needs the fixing commit"), "{m}");
    assert!(m.contains("yuvalrakavy/Store#999999 is not in launch L1"), "{m}");
    let unknown = t.write("unknown.json", &json!({"suggestions": [], "close_all": true}));
    let m = t.refused(&["record-suggestions", "--launch", "L1", &unknown]);
    assert!(m.contains("not a suggestions file"), "{m}");
}
