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
