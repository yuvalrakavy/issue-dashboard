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
    /// Owns the directory the root lives in.
    dir: tempfile::TempDir,
    /// The tracker root: the temporary directory itself, or a directory named in it.
    root: PathBuf,
    now: String,
}

impl Tracker {
    fn new() -> Tracker {
        Tracker::named("")
    }

    /// A tracker whose root directory is called `name` (inside the temporary directory).
    fn named(name: &str) -> Tracker {
        let dir = tempfile::tempdir().unwrap();
        let root = if name.is_empty() {
            dir.path().to_path_buf()
        } else {
            dir.path().join(name)
        };
        copy_dir(&fixtures().join("config"), &root);
        let meta: Value =
            serde_json::from_str(&fs::read_to_string(fixtures().join("replay-meta.json")).unwrap()).unwrap();
        Tracker {
            dir,
            root,
            now: meta["recorded_at"].as_str().unwrap().to_string(),
        }
    }

    fn cmd(&self, args: &[&str]) -> Output {
        self.cmd_at(&self.now, args)
    }

    /// A command run at the fixed time `now`.
    fn cmd_at(&self, now: &str, args: &[&str]) -> Output {
        Command::new(BIN)
            .args(args)
            .env("ISSUE_DASHBOARD_ROOT", &self.root)
            .env("ISSUE_DASHBOARD_NOW", now)
            .env("ISSUE_DASHBOARD_REPLAY", fixtures().join("replay"))
            .env("ISSUE_DASHBOARD_PAGE_SIZE", "25")
            .env_remove("ISSUE_DASHBOARD_RECORD")
            .output()
            .unwrap()
    }

    fn ok(&self, args: &[&str]) -> Value {
        let out = self.cmd(args);
        assert!(
            out.status.success(),
            "{args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        serde_json::from_slice(&out.stdout).unwrap()
    }

    /// Runs a command that must be refused (exit 1) and returns its message.
    fn refused(&self, args: &[&str]) -> String {
        let out = self.cmd(args);
        assert_eq!(
            out.status.code(),
            Some(1),
            "{args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
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
        .filter(|i| {
            i["alias"].as_str().unwrap().starts_with("yuvalrakavy/Store#")
                && i["state"] == "open"
                && i["kind"] == "issue"
        })
        .map(|i| {
            (
                i["id"].as_str().unwrap().to_string(),
                i["alias"].as_str().unwrap().to_string(),
            )
        })
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
    assert!(
        text.contains(&format!("{BIN} --root {}", t.root.display())),
        "the brief names the root"
    );
    assert!(text.contains("record-suggestions --launch L1"), "{text}");
    assert!(b["pieces"].as_array().unwrap().iter().all(|p| p["source"].is_string()));

    t.ok(&["launch-started", "--token", &token, "--session", "local_recommender"]);
    let live = t.ok(&["launches", "--live"]);
    let row = &live["launches"][0];
    assert_eq!(
        (row["id"].as_str(), row["state"].as_str()),
        (Some("L1"), Some("started")),
        "{live}"
    );
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
    let kinds: Vec<&str> = d["suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds.len(), 2, "{kinds:?}");
    assert!(kinds.contains(&"obsolete") && kinds.contains(&"left_out"), "{kinds:?}");
    let meta_repos = d["meta"]["repos"].as_array().unwrap();
    assert!(
        meta_repos
            .iter()
            .all(|r| r["campaign_skill"] == false && r.get("recipe").is_some()),
        "{meta_repos:?}"
    );
}

#[test]
fn a_launch_document_not_written_by_the_owner_is_refused() {
    let t = Tracker::new();
    let open = open_store_issues(&t.outbox());
    let doc = t.write("L1.json", &launch_doc("L1", "user_someone_else", &[open[0].clone()]));
    let m = t.refused(&["reserve", "--request", &doc, "--dispatcher", D]);
    assert!(
        m.contains("author user_someone_else is not project home's owner_id"),
        "{m}"
    );
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

/// `sh` reads `prefix` back as words, the way a session would run the command.
fn shell_words(prefix: &str) -> Vec<String> {
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("for w in {prefix}; do printf '%s\\n' \"$w\"; done"))
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "sh could not read the command prefix {prefix:?}: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(str::to_string)
        .collect()
}

#[test]
fn a_brief_names_this_binary_and_a_root_with_a_space_and_an_apostrophe_as_shell_words() {
    let t = Tracker::named("my root's checkout");
    let open = open_store_issues(&t.outbox());
    let doc = t.write("L1.json", &launch_doc("L1", "user_owner", &open[..1]));
    t.ok(&["reserve", "--request", &doc, "--dispatcher", D]);

    let b = t.ok(&["brief", "--launch", "L1"]);
    let text = b["text"].as_str().unwrap();
    let root = t.root.display().to_string();
    assert!(root.contains(' ') && root.contains('\''), "{root}");

    // The prefix is built here from the test binary and the root, quoted the one standard way.
    let prefix = format!("{BIN} --root '{}'", root.replace('\'', r"'\''"));
    assert!(
        text.contains(&format!("{prefix} record-suggestions --launch L1 <file>")),
        "{text}"
    );
    // Read back by a shell, it is three words: this binary, the flag, the root as it is.
    assert_eq!(shell_words(&prefix), [BIN, "--root", &root]);
    // And so does every prefix in the brief, whatever follows it.
    let mut found = 0;
    for line in text.lines().filter(|l| l.contains(" --root ")) {
        let start = line.find(BIN).expect("the prefix starts with the binary");
        let end = line.find(" record-").expect("a command follows the prefix");
        assert_eq!(shell_words(&line[start..end]), [BIN, "--root", &root], "{line}");
        found += 1;
    }
    assert!(found >= 2, "the rules and the recording section both name it: {found}");
    // The command works as told: a session runs it with that root.
    let file = t.write(
        "one.json",
        &json!({"suggestions": [{"kind": "obsolete", "item": open[0].1, "confidence": 0.6, "reason": "r"}]}),
    );
    let out = Command::new("sh")
        .arg("-c")
        .arg(format!("{prefix} record-suggestions --launch L1 '{file}'"))
        .env("ISSUE_DASHBOARD_NOW", &t.now)
        .env_remove("ISSUE_DASHBOARD_ROOT")
        .env("ISSUE_DASHBOARD_REPLAY", fixtures().join("replay"))
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
}

#[test]
fn live_launches_are_those_holding_a_slot_or_queued_with_every_field() {
    let t = Tracker::new();
    let open = open_store_issues(&t.outbox());
    let doc =
        |id: &str, items: &[(String, String)]| t.write(&format!("{id}.json"), &launch_doc(id, "user_owner", items));

    // L3 takes the only read-only slot and ends; L1 takes it and starts; L2 queues behind it;
    // L4 is withdrawn by the owner before it is reserved.
    let r3 = t.ok(&["reserve", "--request", &doc("L3", &open[4..5]), "--dispatcher", D]);
    let t3 = r3["token"].as_str().unwrap().to_string();
    t.ok(&["launch-started", "--token", &t3, "--session", "local_three"]);
    t.ok(&["launch-ended", "--token", &t3, "--state", "landed"]);

    let r1 = t.ok(&["reserve", "--request", &doc("L1", &open[..2]), "--dispatcher", D]);
    assert_eq!(r1["result"], "reserved", "{r1}");
    let t1 = r1["token"].as_str().unwrap().to_string();
    t.ok(&["launch-started", "--token", &t1, "--session", "local_one"]);
    let outcome = t.ok(&[
        "record-outcome",
        "--launch",
        "L1",
        "--landed",
        "true",
        "--commit",
        "abc1234",
        "--wall-clock-minutes",
        "12",
        "--gate",
        "quick=1",
        "--findings-fixed",
        "0",
        "--context-tokens",
        "900",
    ]);
    assert_eq!(outcome["launch"], "L1");

    let r2 = t.ok(&["reserve", "--request", &doc("L2", &open[2..3]), "--dispatcher", D]);
    assert_eq!(r2["result"], "queued", "{r2}");

    let pages = t.dir.path().join("cancel.json");
    fs::write(
        &pages,
        json!({"decisions": [{
            "id": "D_cancel", "project": "home", "author": "user_owner",
            "created_at": "2026-10-23T10:00:00Z", "action": "cancel_launch", "target": {"launch": "L4"}
        }]})
        .to_string(),
    )
    .unwrap();
    t.ok(&["cycle", "--project", "home", "--pages", pages.to_str().unwrap()]);
    let r4 = t.ok(&["reserve", "--request", &doc("L4", &open[3..4]), "--dispatcher", D]);
    assert_eq!(r4["result"], "withdrawn", "{r4}");

    // Everything is known; only L1 and L2 are live.
    let all = t.ok(&["launches"]);
    let ids = |v: &Value| -> Vec<String> {
        v["launches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["id"].as_str().unwrap().to_string())
            .collect()
    };
    let mut known = ids(&all);
    known.sort();
    assert_eq!(known, ["L1", "L2", "L3", "L4"]);
    let states: Vec<(String, String)> = all["launches"]
        .as_array()
        .unwrap()
        .iter()
        .map(|r| (r["id"].as_str().unwrap().into(), r["state"].as_str().unwrap().into()))
        .collect();
    assert!(states.contains(&("L3".into(), "released".into())), "{states:?}");
    assert!(states.contains(&("L4".into(), "withdrawn".into())), "{states:?}");

    let live = t.ok(&["launches", "--live"]);
    let mut live_ids = ids(&live);
    live_ids.sort();
    assert_eq!(live_ids, ["L1", "L2"], "{live}");
    let row = |id: &str| {
        live["launches"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["id"] == id)
            .unwrap()
            .clone()
    };

    let routing = json!({"model": "claude-opus-5-5", "effort": "medium"});
    assert_eq!(
        row("L1"),
        json!({
            "id": "L1", "project": "home", "kind": "recommend_closures",
            "scope": "closures", "author": "user_owner",
            "aliases": [open[0].1, open[1].1], "recipe": "read-only", "routing": routing,
            "state": "started", "token": t1, "dispatcher": D, "session": "local_one",
            "reserved_at": t.now.replace('Z', ".000Z"), "outcome": true,
        })
    );
    let queued = row("L2");
    assert_eq!(
        (
            queued["project"].clone(),
            queued["kind"].clone(),
            queued["aliases"].clone(),
            queued["recipe"].clone()
        ),
        (
            json!("home"),
            json!("recommend_closures"),
            json!([open[2].1]),
            json!("read-only")
        )
    );
    assert_eq!(queued["routing"], routing);
    assert_eq!(queued["state"], "queued");
    assert_eq!(queued["outcome"], false, "{queued}");
    assert_eq!(queued["session"], json!(null), "a queued launch has no session");
    assert_eq!(queued["token"], json!(null), "a queued launch has no token yet");
    assert_eq!(queued["reserved_at"], json!(null), "a queued launch has no slot time");
    assert_eq!(queued["dispatcher"], json!(null), "{queued}");
}

// --- triage phase 4/5: the daily run (triage spec 5.2) ------------------------------------

#[test]
fn the_daily_run_from_request_to_reservation() {
    let t = Tracker::new();
    t.outbox();
    let r = t.ok(&["triage-request", "--project", "home"]);
    assert_eq!(r["result"], "requested", "{r}");
    assert_eq!(
        r["routing"],
        json!({"model": "claude-opus-5-5", "effort": "medium", "reason": "the tracker's default routing (triage spec 5.3)"})
    );
    assert_eq!(r["aliases"].as_array().unwrap().len(), 40, "{r}");
    let id = r["launch"].as_str().unwrap().to_string();

    // It waits as requested, the schedule's, at scope full.
    let waiting = t.ok(&["launches", "--requested"]);
    let rows = waiting["launches"].as_array().unwrap();
    assert_eq!(rows.len(), 1, "{waiting}");
    assert_eq!(
        (
            rows[0]["id"].as_str(),
            rows[0]["author"].as_str(),
            rows[0]["kind"].as_str(),
            rows[0]["scope"].as_str(),
            rows[0]["state"].as_str()
        ),
        (
            Some(id.as_str()),
            Some("schedule"),
            Some("triage"),
            Some("full"),
            Some("requested")
        )
    );
    assert_eq!(
        t.ok(&["launches", "--live"])["launches"],
        json!([]),
        "requested is not live"
    );

    // A second run while it is live is refused, and records nothing.
    let m = t.refused(&["triage-request", "--project", "home"]);
    assert!(
        m.contains(&format!(
            "triage launch {id} of project home is live (requested); the daily run requests nothing until it ends"
        )),
        "{m}"
    );
    assert_eq!(t.ok(&["launches"])["launches"].as_array().unwrap().len(), 1);

    // The dispatcher reserves it as it reserves the owner's.
    let res = t.ok(&["reserve", "--launch", &id, "--dispatcher", D]);
    assert_eq!(res["result"], "reserved", "{res}");
    let token = res["token"].as_str().unwrap().to_string();
    let b = t.ok(&["brief", "--launch", &id]);
    assert_eq!(b["title"], json!(format!("Triage (40) {token}")));
    let text = b["text"].as_str().unwrap();
    assert!(
        text.contains(&format!(
            "prepare --launch {id} --session-model <model>@<effort> --out questions.json"
        )),
        "{text}"
    );
    assert!(text.contains("--stage priority"), "a full triage has priority's stage");
    assert!(!text.contains("labels:"), "the triage brief prints no label (1.1)");

    // The page sees it: its kind, scope and author, and what the sheet needs.
    let d = t.outbox();
    let st = d["launch_status"]
        .as_array()
        .unwrap()
        .iter()
        .find(|s| s["launch"] == json!(id))
        .unwrap()
        .clone();
    assert_eq!(
        (
            st["kind"].clone(),
            st["scope"].clone(),
            st["author"].clone(),
            st["state"].clone()
        ),
        (json!("triage"), json!("full"), json!("schedule"), json!("reserved"))
    );
    assert_eq!(d["meta"]["routing"]["triage"]["model"], "claude-opus-5-5");
    assert_eq!(d["meta"]["routing"]["stronger_look"]["model"], "claude-fable-5-1");
    assert_eq!(d["meta"]["routing"]["stronger_look"]["effort"], "high");
    assert_eq!(d["meta"]["max_triage_items"], 40);
    assert_eq!(d["meta"]["evaluated_backends"], json!([]));
    assert_eq!(d["meta"]["triage_queue"]["full"].as_array().unwrap().len(), 40);
}

#[test]
fn reserve_refuses_a_schedule_launch_of_another_kind() {
    let t = Tracker::new();
    let open = open_store_issues(&t.outbox());
    let mut doc = launch_doc("L1", "schedule", &[open[0].clone()]);
    doc["kind"] = json!("fix");
    doc["recipe"] = json!("lanes");
    let file = t.write("L1.json", &doc);
    let m = t.refused(&["reserve", "--request", &file, "--dispatcher", D]);
    assert!(
        m.contains("launch L1: author schedule requests triage launches only, not fix"),
        "{m}"
    );
}

#[test]
fn reserve_by_id_checks_the_author_against_today_s_owner() {
    let t = Tracker::new();
    let open = open_store_issues(&t.outbox());
    // The owner's launch is ingested by a cycle, then the project changes owner before the
    // dispatcher reserves it by id.
    let pages = t.write(
        "pages.json",
        &json!({"decisions": [], "launches": [launch_doc("L1", "user_owner", &[open[0].clone()])], "views": [], "requests": []}),
    );
    t.ok(&["cycle", "--project", "home", "--pages", &pages]);
    let cfg = t.root.join("projects/home.toml");
    let text = fs::read_to_string(&cfg).unwrap();
    fs::write(
        &cfg,
        text.replace("owner_id = \"user_owner\"", "owner_id = \"user_new_owner\""),
    )
    .unwrap();
    let m = t.refused(&["reserve", "--launch", "L1", "--dispatcher", D]);
    assert!(
        m.contains("launch L1: author user_owner is not project home's owner_id"),
        "{m}"
    );
}

#[test]
fn an_eval_launch_is_briefed_for_the_backend_it_measures() {
    let t = Tracker::new();
    t.outbox();
    let doc = json!({
        "id": "E1", "project": "home", "author": "user_owner", "created_at": "2026-10-23T08:00:00Z",
        "kind": "eval", "items": [], "aliases": [], "recipe": "read-only",
        "routing": {"model": "claude-sonnet-5-5", "effort": "high"},
    });
    let file = t.write("E1.json", &doc);
    let r = t.ok(&["reserve", "--request", &file, "--dispatcher", D]);
    assert_eq!(r["result"], "reserved", "{r}");
    let token = r["token"].as_str().unwrap();
    let b = t.ok(&["brief", "--launch", "E1"]);
    // The label is cut to leave the token room within 60 characters.
    assert_eq!(b["title"], json!(format!("Evaluate claude-sonnet- {token}")));
    let text = b["text"].as_str().unwrap();
    for d in ["type", "area", "priority"] {
        assert!(
            text.contains(&format!("--decision {d} --backend claude-sonnet-5-5@high")),
            "{d}: {text}"
        );
    }
}

#[test]
fn record_suggestions_stamps_the_time_of_the_command_not_of_the_clock() {
    let t = Tracker::new();
    let open = open_store_issues(&t.outbox());
    let doc = t.write("L1.json", &launch_doc("L1", "user_owner", &open[..2]));
    t.ok(&["reserve", "--request", &doc, "--dispatcher", D]);
    let file = t.write(
        "s.json",
        &json!({"suggestions": [
            {"kind": "obsolete", "item": open[0].1, "confidence": 0.6, "reason": "superseded"}],
            "left_out": [{"item": open[1].1, "why": "keep_open", "reason": "wanted"}]}),
    );
    let at = "2026-10-23T12:34:56Z";
    let out = t.cmd_at(at, &["record-suggestions", "--launch", "L1", &file]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

    let d = t.outbox();
    let made: Vec<&str> = d["suggestions"]
        .as_array()
        .unwrap()
        .iter()
        .map(|s| s["made_at"].as_str().unwrap())
        .collect();
    let stamped = "2026-10-23T12:34:56.000Z";
    assert_eq!(made, [stamped, stamped], "{made:?}");
}
