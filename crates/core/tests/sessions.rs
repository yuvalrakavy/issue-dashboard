//! Session tracking (10.10): the app's session list ingested as `session_seen` and
//! `session_gone`, and each session matched to the items it works on.

mod common;

use std::fs;

use common::TempRoot;
use common::fixture::*;
use issue_dashboard_core::cycle::{self, CycleReport};
use issue_dashboard_core::github::GitHub;
use issue_dashboard_core::ingest::Pages;
use issue_dashboard_core::launch::{self, Reservation, Via};
use issue_dashboard_core::sessions::SessionList;
use issue_dashboard_core::{Kind, ledger};
use serde_json::{Value, json};

const STORE_CHECKOUT: &str = "/Users/yuval/Documents/Projects/Store";
const RHAI_CHECKOUT: &str = "/work/rhai";
const FIX_403: &str = "local_23265ad0-0776-4092-bfb7-8a21ffcc7a54";

/// The fixture root with Store's and rhai's checkouts named in the registry.
fn root() -> TempRoot {
    let t = fixture_root();
    let path = t.path().join("repos.toml");
    let text = fs::read_to_string(&path).unwrap();
    let text = text
        .replacen(
            "[repo.\"yuvalrakavy/Store\"]\n",
            &format!("[repo.\"yuvalrakavy/Store\"]\nlocal_path = \"{STORE_CHECKOUT}\"\n"),
            1,
        )
        .replacen(
            "[repo.\"rhaiscript/rhai\"]\n",
            &format!("[repo.\"rhaiscript/rhai\"]\nlocal_path = \"{RHAI_CHECKOUT}\"\n"),
            1,
        );
    assert_eq!(text.matches("local_path").count(), 2);
    fs::write(&path, text).unwrap();
    t
}

fn recorded() -> SessionList {
    serde_json::from_str(&fs::read_to_string(fixtures().join("sessions/list.json")).unwrap()).unwrap()
}

fn list(limit: u32, rows: Vec<Value>) -> SessionList {
    serde_json::from_value(json!({"listed_at": "2026-10-23T16:47:00Z", "limit": limit, "sessions": rows})).unwrap()
}

fn row(id: &str, title: &str, cwd: &str) -> Value {
    json!({
        "sessionId": id,
        "title": title,
        "cwd": cwd,
        "isArchived": false,
        "isRunning": true,
        "lastActivityAt": "2026-10-23T16:46:02.205Z",
        "link": format!("claude://claude.ai/epitaxy/{id}"),
    })
}

fn run(t: &TempRoot, sessions: Option<&SessionList>, minutes: i64) -> (CycleReport, Value) {
    let r = cycle::cycle_full(
        &t.root,
        &gh(),
        None,
        "home",
        &Pages::default(),
        sessions,
        &move || at(minutes),
        PAGE,
    )
    .unwrap();
    let d = serde_json::from_str(&fs::read_to_string(&r.outbox).unwrap()).unwrap();
    (r, d)
}

fn by_alias<'a>(d: &'a Value, alias: &str) -> &'a Value {
    d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["alias"] == alias)
        .unwrap_or_else(|| panic!("no {alias}"))
}

/// The sessions shown on an item, as (session id, match kind, via).
fn shown(d: &Value, alias: &str) -> Vec<(String, String, String)> {
    by_alias(d, alias)["sessions"]
        .as_array()
        .unwrap_or_else(|| panic!("{alias} has no sessions field"))
        .iter()
        .map(|s| {
            (
                s["id"].as_str().unwrap().to_string(),
                s["match"]["kind"].as_str().unwrap().to_string(),
                s["match"]["via"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

/// Every item showing a session.
fn matched(d: &Value) -> Vec<String> {
    d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["sessions"].as_array().is_some_and(|s| !s.is_empty()))
        .map(|i| i["alias"].as_str().unwrap().to_string())
        .collect()
}

/// An open Store issue whose status is `ready` without a session: (alias, number).
fn ready_store_issue(d: &Value) -> (String, u64) {
    let i = d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| {
            i["alias"].as_str().unwrap().starts_with("yuvalrakavy/Store#")
                && i["kind"] == "issue"
                && i["state"] == "open"
                && i["status"] == "ready"
        })
        .expect("a ready Store issue");
    let alias = i["alias"].as_str().unwrap().to_string();
    let n = alias.rsplit('#').next().unwrap().parse().unwrap();
    (alias, n)
}

fn events(t: &TempRoot, kind: Kind) -> Vec<issue_dashboard_core::Event> {
    t.root.read(|l| ledger::events_by_kind(l.conn(), kind)).unwrap()
}

#[test]
fn the_recorded_list_records_each_unarchived_session_and_nothing_for_the_archived() {
    let t = root();
    let (r, _) = run(&t, Some(&recorded()), 0);
    assert_eq!(r.sessions.seen, 4, "{:?}", r.sessions);
    assert_eq!(r.sessions.gone, 0);
    assert!(r.sessions.skipped.is_empty(), "{:?}", r.sessions.skipped);
    let seen = events(&t, Kind::SessionSeen);
    assert_eq!(seen.len(), 4);
    let fix = seen.iter().find(|e| e.subject == FIX_403).unwrap();
    assert_eq!(
        fix.data["title"],
        "Fix gh #403: one parked DeleteStore stops the server"
    );
    assert_eq!(fix.data["running"], true);
    assert_eq!(fix.data["last_active_at"], "2026-10-23T16:46:02.205Z");
    assert!(events(&t, Kind::SessionGone).is_empty());
}

#[test]
fn a_session_event_holds_only_the_listed_fields_never_the_rest_of_the_row() {
    let t = root();
    run(&t, Some(&recorded()), 0);
    let allowed = [
        "title",
        "cwd",
        "branch",
        "pr_number",
        "running",
        "last_active_at",
        "link",
        "listed_at",
    ];
    for e in events(&t, Kind::SessionSeen) {
        for k in e.data.as_object().unwrap().keys() {
            assert!(allowed.contains(&k.as_str()), "{k} in {}", e.data);
        }
        assert!(e.data.get("startedBy").is_none() && e.data.get("name").is_none());
    }
}

#[test]
fn ingesting_the_same_list_again_records_nothing() {
    let t = root();
    run(&t, Some(&recorded()), 0);
    let before = events(&t, Kind::SessionSeen).len() + events(&t, Kind::SessionGone).len();
    let (r, _) = run(&t, Some(&recorded()), 5);
    assert_eq!((r.sessions.seen, r.sessions.gone), (0, 0));
    assert_eq!(
        events(&t, Kind::SessionSeen).len() + events(&t, Kind::SessionGone).len(),
        before
    );
}

#[test]
fn a_cycle_without_a_session_list_records_no_session_event() {
    let t = root();
    run(&t, Some(&recorded()), 0);
    run(&t, None, 5);
    assert!(
        events(&t, Kind::SessionGone).is_empty(),
        "no list says nothing about any session"
    );
}

#[test]
fn activity_within_the_same_hour_records_nothing_and_the_next_hour_does() {
    let t = root();
    let mut r = row("s1", "work", STORE_CHECKOUT);
    run(&t, Some(&list(20, vec![r.clone()])), 0);
    r["lastActivityAt"] = json!("2026-10-23T16:59:59.000Z");
    let (same, _) = run(&t, Some(&list(20, vec![r.clone()])), 5);
    assert_eq!(same.sessions.seen, 0);
    r["lastActivityAt"] = json!("2026-10-23T17:00:01.000Z");
    let (next, _) = run(&t, Some(&list(20, vec![r.clone()])), 10);
    assert_eq!(next.sessions.seen, 1);
    // A change of running or title is recorded at once.
    r["isRunning"] = json!(false);
    let (idle, _) = run(&t, Some(&list(20, vec![r])), 15);
    assert_eq!(idle.sessions.seen, 1);
}

#[test]
fn archiving_a_session_records_it_gone_and_unarchiving_records_it_seen_again() {
    let t = root();
    let mut r = row("s1", "work", STORE_CHECKOUT);
    run(&t, Some(&list(20, vec![r.clone()])), 0);
    r["isArchived"] = json!(true);
    let (gone, _) = run(&t, Some(&list(20, vec![r.clone()])), 5);
    assert_eq!((gone.sessions.seen, gone.sessions.gone), (0, 1));
    let g = &events(&t, Kind::SessionGone)[0];
    assert_eq!(g.data["why"], "archived");
    // Still archived: nothing more.
    let (again, _) = run(&t, Some(&list(20, vec![r.clone()])), 10);
    assert_eq!((again.sessions.seen, again.sessions.gone), (0, 0));
    // Unarchived with the same content as before it went: seen again.
    r["isArchived"] = json!(false);
    let (back, _) = run(&t, Some(&list(20, vec![r])), 15);
    assert_eq!((back.sessions.seen, back.sessions.gone), (1, 0));
}

#[test]
fn a_session_missing_from_a_complete_list_is_gone() {
    let t = root();
    run(
        &t,
        Some(&list(
            20,
            vec![row("s1", "a", STORE_CHECKOUT), row("s2", "b", STORE_CHECKOUT)],
        )),
        0,
    );
    let (r, _) = run(&t, Some(&list(20, vec![row("s1", "a", STORE_CHECKOUT)])), 5);
    assert_eq!(r.sessions.gone, 1);
    let g = &events(&t, Kind::SessionGone)[0];
    assert_eq!(g.subject, "s2");
    assert_eq!(g.data["why"], "absent");
}

#[test]
fn a_session_that_only_fell_off_the_end_of_a_cut_list_is_not_gone() {
    let t = root();
    let mut old = row("old", "a", STORE_CHECKOUT);
    old["lastActivityAt"] = json!("2026-10-20T09:00:00.000Z");
    let new = row("new", "b", STORE_CHECKOUT);
    run(&t, Some(&list(20, vec![new.clone(), old.clone()])), 0);
    // A list cut at 1 row: `old` is older than every listed row, so it may just be past the cut.
    let (cut, _) = run(&t, Some(&list(1, vec![new.clone()])), 5);
    assert_eq!(cut.sessions.gone, 0, "{:?}", cut.sessions);
    // A list cut at 1 that shows an older row than `new`'s last seen activity: `new` would have
    // been listed, so it is gone.
    let (gone, _) = run(&t, Some(&list(1, vec![old])), 10);
    assert_eq!(gone.sessions.gone, 1);
    assert_eq!(events(&t, Kind::SessionGone)[0].subject, "new");
}

#[test]
fn a_row_that_does_not_validate_is_skipped_with_its_reason_and_the_rest_are_recorded() {
    let t = root();
    let mut bad = row("bad", "x", STORE_CHECKOUT);
    bad.as_object_mut().unwrap().remove("lastActivityAt");
    let (r, _) = run(&t, Some(&list(20, vec![bad, row("good", "y", STORE_CHECKOUT)])), 0);
    assert_eq!(r.sessions.seen, 1);
    assert_eq!(r.sessions.skipped.len(), 1);
    assert_eq!(r.sessions.skipped[0].id.as_deref(), Some("bad"));
}

#[test]
fn a_title_naming_an_issue_of_the_sessions_checkout_is_a_probable_match() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, n) = ready_store_issue(&base);
    let (_, d) = run(
        &t,
        Some(&list(
            20,
            vec![row("s1", &format!("Fix gh #{n}: something"), STORE_CHECKOUT)],
        )),
        5,
    );
    assert_eq!(
        shown(&d, &alias),
        vec![("s1".into(), "probable".into(), "title".into())]
    );
    assert_eq!(matched(&d), vec![alias.clone()]);
    let s = &by_alias(&d, &alias)["sessions"][0];
    assert_eq!(s["title"], format!("Fix gh #{n}: something"));
    assert_eq!(s["running"], true);
    assert_eq!(s["last_active_at"], "2026-10-23T16:46:02.205Z");
    assert_eq!(s["link"], "claude://claude.ai/epitaxy/s1");
    assert!(s.get("cwd").is_none(), "the page never gets the working directory");
    assert_eq!(by_alias(&d, &alias)["status"], "in-progress");
}

#[test]
fn each_reference_form_matches_and_lookalikes_do_not() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, n) = ready_store_issue(&base);
    let forms = [
        (format!("#{n}"), None),
        (format!("work on issue {n} today"), None),
        (format!("Issue #{n}"), None),
        (format!("yuvalrakavy/Store#{n} fix"), None),
        ("tidy".to_string(), Some(format!("fix/issue-{n}"))),
        ("tidy".to_string(), Some(format!("gh-{n}-parked"))),
    ];
    for (i, (title, branch)) in forms.iter().enumerate() {
        let mut r = row(&format!("s{i}"), title, STORE_CHECKOUT);
        if let Some(b) = branch {
            r["branch"] = json!(b);
        }
        let (_, d) = run(&t, Some(&list(20, vec![r])), 5 + i as i64);
        let via = if branch.is_some() { "branch" } else { "title" };
        assert_eq!(
            shown(&d, &alias),
            vec![(format!("s{i}"), "probable".into(), via.into())],
            "{title} {branch:?}"
        );
    }
    let lookalikes = [
        (format!("x{n}"), None),
        (format!("abc#{n}"), None),
        (format!("issues {n}"), None),
        (format!("tissue {n}"), None),
        (format!("#{n}0"), None),
        // The number runs into a word: not that number.
        (format!("#{n}a"), None),
        (format!("issue {n}b"), None),
        ("tidy".to_string(), Some(format!("tissue-{n}"))),
        ("tidy".to_string(), Some(format!("fix/issue-{n}0"))),
        ("tidy".to_string(), Some(format!("fix/issue-{n}x"))),
    ];
    for (i, (title, branch)) in lookalikes.iter().enumerate() {
        let id = format!("l{i}");
        let mut r = row(&id, title, STORE_CHECKOUT);
        if let Some(b) = branch {
            r["branch"] = json!(b);
        }
        let (_, d) = run(&t, Some(&list(20, vec![r])), 50 + i as i64);
        assert!(
            !d["issues"].as_array().unwrap().iter().any(|it| it["sessions"]
                .as_array()
                .is_some_and(|s| s.iter().any(|s| s["id"] == id))),
            "{title} {branch:?} matched {:?}",
            matched(&d)
        );
    }
}

#[test]
fn a_bare_reference_from_a_session_outside_every_member_checkout_matches_nothing() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (_, n) = ready_store_issue(&base);
    let (_, d) = run(
        &t,
        Some(&list(
            20,
            vec![row("s1", &format!("Fix #{n}"), "/Users/yuval/Documents/Projects/Other")],
        )),
        5,
    );
    assert!(matched(&d).is_empty(), "{:?}", matched(&d));
    // A sibling directory whose name only starts with the checkout's is not under it.
    let (_, d) = run(
        &t,
        Some(&list(
            20,
            vec![row("s2", &format!("Fix #{n}"), &format!("{STORE_CHECKOUT}-old"))],
        )),
        10,
    );
    assert!(matched(&d).is_empty(), "{:?}", matched(&d));
}

#[test]
fn a_worktree_under_a_member_checkout_resolves_to_that_repo() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, n) = ready_store_issue(&base);
    let cwd = format!("{STORE_CHECKOUT}/.claude/worktrees/fix-{n}");
    let (_, d) = run(&t, Some(&list(20, vec![row("s1", &format!("Fix #{n}"), &cwd)])), 5);
    assert_eq!(
        shown(&d, &alias),
        vec![("s1".into(), "probable".into(), "title".into())]
    );
}

#[test]
fn a_title_with_two_different_issue_numbers_matches_nothing() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, n) = ready_store_issue(&base);
    let (_, d) = run(
        &t,
        Some(&list(
            20,
            vec![row("s1", &format!("Fix #{n} and #{}", n + 1), STORE_CHECKOUT)],
        )),
        5,
    );
    assert!(matched(&d).is_empty(), "{:?}", matched(&d));
    // The same number twice is one reference.
    let (_, d) = run(
        &t,
        Some(&list(
            20,
            vec![row("s2", &format!("Fix #{n}, see #{n}"), STORE_CHECKOUT)],
        )),
        10,
    );
    assert_eq!(shown(&d, &alias).len(), 1);
}

#[test]
fn a_number_not_in_the_project_and_a_repo_not_a_member_match_nothing() {
    let t = root();
    let rows = vec![
        row("s1", "Fix #999999", STORE_CHECKOUT),
        row("s2", "someone/elsewhere#1", STORE_CHECKOUT),
    ];
    let (_, d) = run(&t, Some(&list(20, rows)), 5);
    assert!(matched(&d).is_empty(), "{:?}", matched(&d));
}

#[test]
fn a_sessions_pull_request_matches_it_and_the_issues_it_closes_exactly() {
    let t = root();
    // Recorded: rhai#1194 closes rhai#1193.
    let mut r = row(
        "s1",
        "Fix #1188 meanwhile",
        &format!("{RHAI_CHECKOUT}/.claude/worktrees/x"),
    );
    r["prNumber"] = json!(1194);
    r["branch"] = json!("issue-1188");
    let (_, d) = run(&t, Some(&list(20, vec![r])), 5);
    let exact = |a: &str| shown(&d, a);
    assert_eq!(
        exact("rhaiscript/rhai#1194"),
        vec![("s1".into(), "exact".into(), "pull_request".into())]
    );
    assert_eq!(
        exact("rhaiscript/rhai#1193"),
        vec![("s1".into(), "exact".into(), "pull_request".into())]
    );
    let mut m = matched(&d);
    m.sort();
    assert_eq!(
        m,
        vec!["rhaiscript/rhai#1193", "rhaiscript/rhai#1194"],
        "an exact match drops the title's reference"
    );
}

#[test]
fn a_pull_request_number_without_a_resolvable_repo_matches_nothing() {
    let t = root();
    let mut r = row("s1", "tidy", "/elsewhere");
    r["prNumber"] = json!(1194);
    let (_, d) = run(&t, Some(&list(20, vec![r])), 5);
    assert!(matched(&d).is_empty(), "{:?}", matched(&d));
}

#[test]
fn a_pull_request_number_that_is_an_issues_number_matches_nothing() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (_, n) = ready_store_issue(&base);
    let mut r = row("s1", "tidy", STORE_CHECKOUT);
    r["prNumber"] = json!(n);
    let (_, d) = run(&t, Some(&list(20, vec![r])), 5);
    assert!(matched(&d).is_empty(), "{:?}", matched(&d));
}

#[test]
fn a_launched_session_matches_its_launchs_items_exactly() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, _) = ready_store_issue(&base);
    let item = by_alias(&base, &alias)["id"].as_str().unwrap().to_string();
    let token = t
        .root
        .mutate(at(1), |tx| {
            launch::request(tx, &common::launch_doc("L_1", "home", &[&item], "lanes"), Via::Ingest)?;
            match launch::reserve(tx, "L_1", "disp", &|_| Ok(1))? {
                Reservation::Reserved { token, .. } => Ok(token),
                other => panic!("{other:?}"),
            }
        })
        .unwrap();
    // By the session the dispatcher recorded as started.
    t.root
        .mutate(at(2), |tx| launch::started(tx, &token, "s_started"))
        .unwrap();
    let (_, d) = run(&t, Some(&list(20, vec![row("s_started", "Fix it", "/elsewhere")])), 5);
    assert_eq!(
        shown(&d, &alias),
        vec![("s_started".into(), "exact".into(), "launch".into())]
    );
    // By the token in a session's title, before `started` is recorded for it.
    let (_, d) = run(
        &t,
        Some(&list(
            20,
            vec![row("s_titled", &format!("Fix it [{token}]"), "/elsewhere")],
        )),
        10,
    );
    assert_eq!(
        shown(&d, &alias),
        vec![("s_titled".into(), "exact".into(), "launch".into())]
    );
}

#[test]
fn an_archived_session_no_longer_shows_and_the_status_returns() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, n) = ready_store_issue(&base);
    let mut r = row("s1", &format!("Fix #{n}"), STORE_CHECKOUT);
    let (_, d) = run(&t, Some(&list(20, vec![r.clone()])), 5);
    assert_eq!(by_alias(&d, &alias)["status"], "in-progress");
    r["isArchived"] = json!(true);
    let (_, d) = run(&t, Some(&list(20, vec![r])), 10);
    assert_eq!(by_alias(&d, &alias)["sessions"], json!([]));
    assert_eq!(by_alias(&d, &alias)["status"], "ready");
}

#[test]
fn several_sessions_on_one_issue_are_shown_newest_first() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, n) = ready_store_issue(&base);
    let a = row("older", &format!("Fix #{n}"), STORE_CHECKOUT);
    let mut b = row("newer", &format!("Review #{n}"), STORE_CHECKOUT);
    b["lastActivityAt"] = json!("2026-10-23T16:50:00.000Z");
    let (_, d) = run(&t, Some(&list(20, vec![a, b])), 5);
    let ids: Vec<String> = shown(&d, &alias).into_iter().map(|(id, _, _)| id).collect();
    assert_eq!(ids, vec!["newer", "older"]);
}

#[test]
fn the_recorded_fix_403_session_is_matched_when_store_403_is_tracked() {
    let t = root();
    let (_, d) = run(&t, Some(&recorded()), 0);
    let tracked = d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .any(|i| i["alias"] == "yuvalrakavy/Store#403");
    if tracked {
        assert_eq!(
            shown(&d, "yuvalrakavy/Store#403"),
            vec![(FIX_403.into(), "probable".into(), "title".into())]
        );
    }
    // The other recorded titles name no issue.
    let m = matched(&d);
    assert!(m.iter().all(|a| a == "yuvalrakavy/Store#403"), "{m:?}");
}

// ---- the deep gate's mutation pass: behaviour the tests above left unpinned ----

/// A launch of `items` in `project`, reserved and recorded as started by `session`.
fn launched(t: &TempRoot, project: &str, items: &[&str], session: &str) {
    t.root
        .mutate(at(1), |tx| {
            launch::request(tx, &common::launch_doc("L_9", project, items, "lanes"), Via::Ingest)?;
            let token = match launch::reserve(tx, "L_9", "disp", &|_| Ok(1))? {
                Reservation::Reserved { token, .. } => token,
                other => panic!("{other:?}"),
            };
            launch::started(tx, &token, session)
        })
        .unwrap();
}

fn id_of(d: &Value, alias: &str) -> String {
    by_alias(d, alias)["id"].as_str().unwrap().to_string()
}

#[test]
fn the_longest_checkout_holding_the_working_directory_wins() {
    let t = root();
    // rhai's checkout lies inside Store's.
    let path = t.path().join("repos.toml");
    let text = fs::read_to_string(&path).unwrap();
    let nested = format!("{STORE_CHECKOUT}/vendor/rhai");
    let text = text.replace(RHAI_CHECKOUT, &nested);
    assert!(text.contains(&nested));
    fs::write(&path, text).unwrap();
    let (_, d) = run(
        &t,
        Some(&list(20, vec![row("s1", "Fix #1193", &format!("{nested}/src"))])),
        5,
    );
    assert_eq!(
        shown(&d, "rhaiscript/rhai#1193"),
        vec![("s1".into(), "probable".into(), "title".into())]
    );
}

#[test]
fn a_session_the_launch_and_its_pull_request_both_name_is_shown_once_per_item() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let pr = id_of(&base, "rhaiscript/rhai#1194");
    let issue = id_of(&base, "rhaiscript/rhai#1193");
    launched(&t, "home", &[&pr, &issue], "s1");
    let mut r = row("s1", "tidy", &format!("{RHAI_CHECKOUT}/x"));
    r["prNumber"] = json!(1194);
    let (_, d) = run(&t, Some(&list(20, vec![r])), 5);
    for a in ["rhaiscript/rhai#1194", "rhaiscript/rhai#1193"] {
        assert_eq!(
            shown(&d, a),
            vec![("s1".into(), "exact".into(), "launch".into())],
            "{a}"
        );
    }
}

#[test]
fn a_title_and_a_branch_naming_the_same_item_say_the_title() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, n) = ready_store_issue(&base);
    let mut r = row("s1", &format!("Fix #{n}"), STORE_CHECKOUT);
    r["branch"] = json!(format!("fix/issue-{n}"));
    let (_, d) = run(&t, Some(&list(20, vec![r])), 5);
    assert_eq!(
        shown(&d, &alias),
        vec![("s1".into(), "probable".into(), "title".into())]
    );
    // A title that names nothing leaves the branch to say it.
    let mut r = row("s2", "tidy up", STORE_CHECKOUT);
    r["branch"] = json!(format!("fix/issue-{n}"));
    let (_, d) = run(&t, Some(&list(20, vec![r])), 10);
    assert_eq!(
        shown(&d, &alias),
        vec![("s2".into(), "probable".into(), "branch".into())]
    );
}

#[test]
fn a_title_and_a_branch_naming_different_items_match_nothing() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (_, n) = ready_store_issue(&base);
    let mut r = row("s1", &format!("Fix #{n}"), STORE_CHECKOUT);
    r["branch"] = json!(format!("fix/issue-{}", n + 1));
    let (_, d) = run(&t, Some(&list(20, vec![r])), 5);
    assert!(matched(&d).is_empty(), "{:?}", matched(&d));
}

#[test]
fn another_projects_launch_does_not_make_its_session_exact_here() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, _) = ready_store_issue(&base);
    let item = id_of(&base, &alias);
    launched(&t, "elsewhere", &[&item], "s1");
    let (_, d) = run(&t, Some(&list(20, vec![row("s1", "tidy", "/elsewhere")])), 5);
    assert!(matched(&d).is_empty(), "{:?}", matched(&d));
}

#[test]
fn a_launch_whose_items_the_project_does_not_show_does_not_hide_the_titles_reference() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, n) = ready_store_issue(&base);
    launched(&t, "home", &["I_not_in_the_project"], "s1");
    let (_, d) = run(
        &t,
        Some(&list(20, vec![row("s1", &format!("Fix #{n}"), STORE_CHECKOUT)])),
        5,
    );
    assert_eq!(
        shown(&d, &alias),
        vec![("s1".into(), "probable".into(), "title".into())]
    );
}

#[test]
fn only_a_ready_or_blocked_issue_becomes_in_progress() {
    let t = root();
    // In the fixture Store#78 needs a decision and Store#71 is untriaged.
    let rows = vec![
        row("s78", "Fix #78", STORE_CHECKOUT),
        row("s71", "Fix #71", STORE_CHECKOUT),
    ];
    let (_, d) = run(&t, Some(&list(20, rows)), 5);
    for (a, status) in [
        ("yuvalrakavy/Store#78", "needs-decision"),
        ("yuvalrakavy/Store#71", "untriaged"),
    ] {
        assert_eq!(by_alias(&d, a)["status"], status, "{a} keeps its status");
        assert_eq!(by_alias(&d, a)["sessions"].as_array().unwrap().len(), 1, "{a} shows it");
    }
}

#[test]
fn a_missing_session_as_active_as_the_oldest_listed_row_is_gone_from_a_cut_list() {
    let t = root();
    let a = row("a", "a", STORE_CHECKOUT);
    let b = row("b", "b", STORE_CHECKOUT);
    run(&t, Some(&list(20, vec![a.clone(), b])), 0);
    // `b` was as active as the one row listed, so a list cut at 1 would have shown either.
    let (r, _) = run(&t, Some(&list(1, vec![a])), 5);
    assert_eq!(r.sessions.gone, 1, "{:?}", r.sessions);
    assert_eq!(events(&t, Kind::SessionGone)[0].subject, "b");
}

#[test]
fn a_session_listed_twice_in_one_list_is_recorded_once_from_its_first_row() {
    let t = root();
    let (r, _) = run(
        &t,
        Some(&list(
            20,
            vec![row("s1", "first", STORE_CHECKOUT), row("s1", "second", STORE_CHECKOUT)],
        )),
        0,
    );
    assert_eq!(r.sessions.seen, 1);
    let seen = events(&t, Kind::SessionSeen);
    assert_eq!(seen.len(), 1);
    assert_eq!(seen[0].data["title"], "first");
}

#[test]
fn a_change_of_title_directory_branch_or_pull_request_is_recorded_at_once() {
    let t = root();
    let mut r = row("s1", "work", STORE_CHECKOUT);
    run(&t, Some(&list(20, vec![r.clone()])), 0);
    let changes: [(&str, Value); 4] = [
        ("title", json!("other work")),
        ("cwd", json!("/elsewhere")),
        ("branch", json!("fix/x")),
        ("prNumber", json!(7)),
    ];
    for (i, (k, v)) in changes.into_iter().enumerate() {
        r[k] = v;
        let (rep, _) = run(&t, Some(&list(20, vec![r.clone()])), 5 + i as i64);
        assert_eq!(rep.sessions.seen, 1, "a change of {k}");
    }
}

#[test]
fn an_idle_session_is_shown_idle_and_equal_activity_orders_by_id() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, n) = ready_store_issue(&base);
    let mut a = row("b_second", &format!("Fix #{n}"), STORE_CHECKOUT);
    a["isRunning"] = json!(false);
    let b = row("a_first", &format!("Fix #{n}"), STORE_CHECKOUT);
    let (_, d) = run(&t, Some(&list(20, vec![a, b])), 5);
    let s = by_alias(&d, &alias)["sessions"].as_array().unwrap().clone();
    assert_eq!(s[0]["id"], "a_first");
    assert_eq!(s[1]["id"], "b_second");
    assert_eq!(s[0]["running"], true);
    assert_eq!(s[1]["running"], false);
}

#[test]
fn reference_lookalikes_the_first_pass_did_not_try() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, n) = ready_store_issue(&base);
    let lookalikes = [
        // `issue` with no gap before the number.
        format!("issue{n}"),
        // A `#` after word-joining punctuation is not a bare reference.
        format!("fix-#{n}"),
        format!("v1.#{n}"),
        // An owner/repo with a second slash is not a repo reference.
        format!("a/b/c#{n}"),
    ];
    for (i, title) in lookalikes.iter().enumerate() {
        let (_, d) = run(
            &t,
            Some(&list(20, vec![row(&format!("l{i}"), title, STORE_CHECKOUT)])),
            5 + i as i64,
        );
        assert!(matched(&d).is_empty(), "{title} matched {:?}", matched(&d));
    }
    // A lookalike beside a real reference leaves the real one alone.
    let (_, d) = run(
        &t,
        Some(&list(
            20,
            vec![row("m", &format!("see a/b/c#9 for #{n}"), STORE_CHECKOUT)],
        )),
        30,
    );
    assert_eq!(shown(&d, &alias).len(), 1);
}

#[test]
fn references_are_case_insensitive_where_the_spec_has_words() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, n) = ready_store_issue(&base);
    let cases = [
        (format!("ISSUE {n}"), None),
        ("tidy".to_string(), Some(format!("Fix/Issue-{n}"))),
        ("tidy".to_string(), Some(format!("GH-{n}"))),
    ];
    for (i, (title, branch)) in cases.iter().enumerate() {
        let mut r = row(&format!("c{i}"), title, STORE_CHECKOUT);
        if let Some(b) = branch {
            r["branch"] = json!(b);
        }
        let (_, d) = run(&t, Some(&list(20, vec![r])), 5 + i as i64);
        assert_eq!(shown(&d, &alias).len(), 1, "{title} {branch:?}");
    }
}

#[test]
fn a_last_activity_that_is_not_a_time_is_skipped_not_fatal() {
    let t = root();
    let mut bad = row("bad", "x", STORE_CHECKOUT);
    bad["lastActivityAt"] = json!("yesterday");
    let (r, _) = run(&t, Some(&list(20, vec![bad, row("good", "y", STORE_CHECKOUT)])), 0);
    assert_eq!((r.sessions.seen, r.sessions.skipped.len()), (1, 1), "{:?}", r.sessions);
}

#[test]
fn a_session_already_gone_is_not_recorded_gone_again_by_later_lists() {
    let t = root();
    let a = row("a", "a", STORE_CHECKOUT);
    let mut b = row("b", "b", STORE_CHECKOUT);
    run(&t, Some(&list(20, vec![a.clone(), b.clone()])), 0);
    // Gone by absence from a complete list; later complete lists do not say so again.
    let (first, _) = run(&t, Some(&list(20, vec![a.clone()])), 5);
    assert_eq!(first.sessions.gone, 1);
    for m in [10, 15] {
        let (again, _) = run(&t, Some(&list(20, vec![a.clone()])), m);
        assert_eq!(again.sessions.gone, 0, "{:?}", again.sessions);
    }
    // Gone by archive; the archived row listed again does not say so again either.
    run(&t, Some(&list(20, vec![a.clone(), b.clone()])), 20);
    b["isArchived"] = json!(true);
    let (arch, _) = run(&t, Some(&list(20, vec![a.clone(), b.clone()])), 25);
    assert_eq!(arch.sessions.gone, 1);
    let (again, _) = run(&t, Some(&list(20, vec![a, b])), 30);
    assert_eq!((again.sessions.seen, again.sessions.gone), (0, 0));
    assert_eq!(events(&t, Kind::SessionGone).len(), 2);
}

fn run_with(
    t: &TempRoot,
    gh: &issue_dashboard_core::github::Replay,
    sessions: Option<&SessionList>,
    minutes: i64,
) -> Value {
    let r = cycle::cycle_full(
        &t.root,
        gh,
        None,
        "home",
        &Pages::default(),
        sessions,
        &move || at(minutes),
        PAGE,
    )
    .unwrap();
    serde_json::from_str(&fs::read_to_string(&r.outbox).unwrap()).unwrap()
}

#[test]
fn a_row_that_is_not_an_object_is_skipped_and_the_rest_are_recorded() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, n) = ready_store_issue(&base);
    let l = list(
        20,
        vec![json!(null), json!(7), row("a", &format!("Fix #{n}"), STORE_CHECKOUT)],
    );
    let (r, d) = run(&t, Some(&l), 5);
    assert_eq!(r.sessions.seen, 1, "{:?}", r.sessions);
    assert_eq!(r.sessions.skipped.len(), 2, "{:?}", r.sessions);
    assert_eq!(shown(&d, &alias).len(), 1);
}

#[test]
fn a_session_whose_row_is_invalid_is_still_listed_and_not_gone() {
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, n) = ready_store_issue(&base);
    run(
        &t,
        Some(&list(20, vec![row("a", &format!("Fix #{n}"), STORE_CHECKOUT)])),
        5,
    );
    let mut bad = row("a", &format!("Fix #{n}"), STORE_CHECKOUT);
    bad["lastActivityAt"] = json!("not a time");
    let (r, d) = run(&t, Some(&list(20, vec![bad])), 10);
    assert_eq!(r.sessions.skipped.len(), 1, "{:?}", r.sessions);
    assert_eq!(r.sessions.gone, 0, "{:?}", r.sessions);
    assert_eq!(shown(&d, &alias).len(), 1);
}

#[test]
fn a_missing_session_a_fraction_of_a_second_older_than_a_cut_list_is_not_gone() {
    let t = root();
    let mut old = row("old", "Untracked work", STORE_CHECKOUT);
    old["lastActivityAt"] = json!("2026-10-23T16:46:02.100Z");
    run(&t, Some(&list(20, vec![old])), 0);
    // A list cut at its limit of one, whose one row is 0.8 s newer: "old" may have been cut.
    let mut new = row("new", "Other work", STORE_CHECKOUT);
    new["lastActivityAt"] = json!("2026-10-23T16:46:02.900Z");
    let (r, _) = run(&t, Some(&list(1, vec![new])), 5);
    assert_eq!(r.sessions.gone, 0, "{:?}", r.sessions);
}

#[test]
fn after_a_rename_sessions_match_by_the_repos_current_name() {
    let t = root();
    let renamed = "yuvalrakavy/StoreRenamed";
    let lib = library();
    let gh = gh_with(move |req, v| {
        let v = match v {
            Ok(v) => v,
            // A request under the new name: serve the recording made under the old one.
            Err(_) => {
                let mut vars = req.variables.clone();
                rename(&mut vars, renamed, STORE);
                if let Some(q) = vars["q"].as_str() {
                    vars["q"] = json!(q.replace(renamed, STORE));
                }
                lib.graphql(&issue_dashboard_core::github::Request {
                    name: req.name,
                    query: req.query,
                    variables: vars,
                })?
            }
        };
        let mut v = v;
        rename(&mut v, STORE, renamed);
        Ok(v)
    });
    let base = run_with(&t, &gh, None, 0);
    let (alias, n) = ready_issue_in(&base, renamed);
    // One in the checkout the registry names under the old name; one naming the old name.
    let by_cwd = row("cwd", &format!("Fix #{n}"), STORE_CHECKOUT);
    let by_old_name = row("old", &format!("Look at {STORE}#{n}"), "/elsewhere");
    let d = run_with(&t, &gh, Some(&list(20, vec![by_cwd, by_old_name])), 5);
    let mut ids: Vec<String> = shown(&d, &alias).into_iter().map(|(id, _, _)| id).collect();
    ids.sort();
    assert_eq!(ids, vec!["cwd", "old"]);
}

fn ready_issue_in(d: &Value, repo: &str) -> (String, u64) {
    let prefix = format!("{repo}#");
    let i = d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| {
            i["alias"].as_str().unwrap().starts_with(&prefix)
                && i["kind"] == "issue"
                && i["state"] == "open"
                && i["status"] == "ready"
        })
        .unwrap_or_else(|| panic!("a ready {repo} issue"));
    let alias = i["alias"].as_str().unwrap().to_string();
    let n = alias.rsplit('#').next().unwrap().parse().unwrap();
    (alias, n)
}

#[test]
fn a_closed_issue_with_its_fix_session_is_closed_not_in_progress() {
    // Store#403's shape on the live page: closed as completed while the session that fixed it
    // is still listed. The session stays shown; the status is the issue's own.
    let t = root();
    let (_, base) = run(&t, None, 0);
    let (alias, n) = ready_store_issue(&base);
    let id = by_alias(&base, &alias)["id"].as_str().unwrap().to_string();
    let gh = gh_with(move |req, v| {
        let mut v = v?;
        if req.name == "nodes"
            && let Some(nodes) = v["data"]["nodes"].as_array_mut()
        {
            for x in nodes.iter_mut().filter(|x| x["id"] == id.as_str()) {
                x["state"] = json!("CLOSED");
                x["stateReason"] = json!("COMPLETED");
                x["closedAt"] = json!(at(4).to_rfc3339());
                x["updatedAt"] = json!(at(4).to_rfc3339());
            }
        }
        Ok(v)
    });
    let sessions = list(20, vec![row("fix", &format!("Fix gh #{n}: something"), STORE_CHECKOUT)]);
    // A full read, so the closed node is read by id.
    let r = cycle::cycle_full(
        &t.root,
        &gh,
        None,
        "home",
        &Pages::default(),
        Some(&sessions),
        &move || at(24 * 60 + 5),
        PAGE,
    )
    .unwrap();
    let d: Value = serde_json::from_str(&fs::read_to_string(&r.outbox).unwrap()).unwrap();
    let x = by_alias(&d, &alias);
    assert_eq!(x["state"], "closed", "{x}");
    assert_eq!(x["status"], "closed", "{x}");
    assert_eq!(shown(&d, &alias).len(), 1, "the session stays shown: {x}");
}
