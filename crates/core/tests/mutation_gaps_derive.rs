//! Derive, ingest and acks: tests that pin guards no other test caught when it was removed or
//! inverted (the pre-merge mutation pass). Each test names the mechanism it keeps.

mod common;

use chrono::Duration;
use common::fixture::*;
use common::*;
use issue_dashboard_core::config::ConfigSet;
use issue_dashboard_core::cycle::{self, CycleReport};
use issue_dashboard_core::derive;
use issue_dashboard_core::docs::{Action, Decision, DecisionValue, Priority, Target};
use issue_dashboard_core::github::Replay;
use issue_dashboard_core::ingest::Pages;
use issue_dashboard_core::launch::{self, EndState, Reservation, State, Via};
use issue_dashboard_core::{Error, ledger};
use serde_json::{Value, json};

fn pages(decisions: &[Decision]) -> Pages {
    Pages {
        decisions: decisions.iter().map(|d| serde_json::to_value(d).unwrap()).collect(),
        ..Pages::default()
    }
}

fn run_with(t: &TempRoot, gh: &Replay, p: &Pages, minutes: i64) -> CycleReport {
    cycle::cycle(&t.root, gh, "home", p, &move || at(minutes), PAGE).unwrap()
}

fn doc(r: &CycleReport) -> Value {
    serde_json::from_str(&std::fs::read_to_string(&r.outbox).unwrap()).unwrap()
}

fn issue<'a>(d: &'a Value, id: &str) -> &'a Value {
    d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["id"] == id)
        .unwrap_or_else(|| panic!("no issue {id}"))
}

fn ack<'a>(d: &'a Value, id: &str) -> &'a Value {
    d["acks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["decision"] == id)
        .unwrap_or_else(|| panic!("no ack for {id}: {}", d["acks"]))
}

/// The ids of the open issues of `repo` in `home`'s outbox of a plain first cycle, sorted.
fn open_ids(repo: &str) -> Vec<String> {
    let t = fixture_root();
    let d = doc(&run_with(&t, &gh(), &no_pages(), 0));
    let prefix = format!("{repo}#");
    let mut ids: Vec<String> = d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["alias"].as_str().unwrap().starts_with(&prefix) && i["state"] == "open")
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect();
    ids.sort();
    ids
}

/// What a test makes of one issue node before the first read of it.
#[derive(Clone)]
struct Sc {
    labels: Vec<&'static str>,
    assoc: &'static str,
    author: &'static str,
    /// (association, author) of each comment.
    comments: Vec<(&'static str, &'static str)>,
    state: &'static str,
    idle_days: i64,
    closed_ago: Option<i64>,
    /// Fields of the node set last, as GitHub would have them.
    raw: Vec<(&'static str, Value)>,
    /// The state of each issue blocking this one.
    blocked_by: Vec<&'static str>,
}

impl Sc {
    fn new() -> Sc {
        Sc {
            labels: vec!["bug", "p1", "kernel"],
            assoc: "MEMBER",
            author: "someone",
            comments: vec![],
            state: "OPEN",
            idle_days: 1,
            closed_ago: None,
            raw: vec![],
            blocked_by: vec![],
        }
    }
    fn labels(mut self, l: &[&'static str]) -> Sc {
        self.labels = l.to_vec();
        self
    }
    fn plus(mut self, l: &'static str) -> Sc {
        self.labels.push(l);
        self
    }
    fn author(mut self, assoc: &'static str, login: &'static str) -> Sc {
        self.assoc = assoc;
        self.author = login;
        self
    }
    fn comment(mut self, assoc: &'static str, login: &'static str) -> Sc {
        self.comments.push((assoc, login));
        self
    }
    fn closed(mut self) -> Sc {
        self.state = "CLOSED";
        self
    }
    /// Closed (or, with `state` open, carrying a stale closedAt) this many minutes before at(0).
    fn closed_minutes_ago(mut self, minutes: i64, state: &'static str) -> Sc {
        self.state = state;
        self.closed_ago = Some(minutes);
        self
    }
    fn idle(mut self, days: i64) -> Sc {
        self.idle_days = days;
        self
    }
    fn blocked(mut self, state: &'static str) -> Sc {
        self.blocked_by.push(state);
        self
    }
    fn raw(mut self, key: &'static str, v: Value) -> Sc {
        self.raw.push((key, v));
        self
    }
    fn apply(&self, n: &mut Value, k: usize) {
        n["labels"] = json!({"totalCount": self.labels.len(), "nodes": self.labels.iter().map(|l| json!({"name": l})).collect::<Vec<_>>()});
        n["authorAssociation"] = json!(self.assoc);
        n["author"] = json!({"login": self.author});
        n["comments"] = json!({
            "totalCount": self.comments.len(),
            "nodes": self.comments.iter().map(|(a, l)| json!({"author": {"login": l}, "authorAssociation": a, "createdAt": "2026-10-20T00:00:00Z"})).collect::<Vec<_>>()
        });
        n["state"] = json!(self.state);
        n["closedAt"] = match (self.closed_ago, self.state) {
            (Some(m), _) => json!(at(-m).to_rfc3339()),
            (None, "CLOSED") => json!(at(-24 * 60).to_rfc3339()),
            _ => Value::Null,
        };
        n["updatedAt"] = json!((at(0) - Duration::days(self.idle_days)).to_rfc3339());
        n["blockedBy"] = json!({"nodes": self.blocked_by.iter().enumerate().map(|(i, s)| json!({
            "id": format!("I_blocker{k}x{i}"), "number": 9000 + i, "state": s,
            "repository": {"nameWithOwner": STORE}
        })).collect::<Vec<_>>()});
        for (key, v) in &self.raw {
            n[*key] = v.clone();
        }
    }
}

/// The replay with each `(id, scenario)` applied to the node it names, whenever it is read.
fn gh_scenarios(list: Vec<(String, Sc)>) -> Replay {
    gh_with(move |req, v| {
        let mut v = v?;
        if req.name == "nodes"
            && let Some(nodes) = v["data"]["nodes"].as_array_mut()
        {
            for n in nodes.iter_mut().filter(|n| !n.is_null()) {
                if let Some((k, (_, sc))) = list.iter().enumerate().find(|(_, (id, _))| n["id"] == id.as_str()) {
                    sc.apply(n, k);
                }
            }
        }
        Ok(v)
    })
}

fn summary_matches_the_issues(d: &Value) {
    let open: Vec<&Value> = d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["state"] == "open")
        .collect();
    let n = |f: &dyn Fn(&&Value) -> bool| open.iter().filter(|i| f(i)).count() as u64;
    let s = &d["meta"]["summary"];
    assert_eq!(s["open"], open.len() as u64);
    assert_eq!(s["p0_open"], n(&|i| i["priority"]["value"] == "p0"));
    assert_eq!(
        s["waiting_on_you"],
        n(&|i| i["status"] == "needs-decision" || (i["status"] == "needs-review" && i["outside"] == true))
    );
    assert_eq!(
        s["untriaged"],
        n(&|i| i["status"] == "untriaged" || i["status"] == "new")
    );
    assert_eq!(s["over_time_limit"], n(&|i| i["limit_breached"] == true));
}

#[test]
fn an_issues_status_follows_the_chain_of_the_spec() {
    let ids = open_ids(STORE);
    assert!(ids.len() >= 30, "{}", ids.len());
    // (scenario, status, outside)
    let table: Vec<(Sc, &str, bool)> = vec![
        (Sc::new(), "ready", false),
        (Sc::new().plus("umbrella"), "umbrella", false),
        (Sc::new().plus("needs-decision"), "needs-decision", false),
        (Sc::new().plus("InProgress"), "in-progress", false),
        (
            Sc::new().plus("needs-decision").plus("InProgress"),
            "needs-decision",
            false,
        ),
        (Sc::new().blocked("OPEN"), "blocked", false),
        (Sc::new().blocked("CLOSED"), "ready", false),
        (Sc::new().blocked("CLOSED").blocked("OPEN"), "blocked", false),
        (Sc::new().plus("InProgress").blocked("OPEN"), "in-progress", false),
        (Sc::new().plus("umbrella").blocked("OPEN"), "umbrella", false),
        // An outside issue is new until an inside author has commented or labelled it.
        (Sc::new().labels(&[]).author("NONE", "stranger"), "new", true),
        (Sc::new().labels(&[]).author("CONTRIBUTOR", "stranger"), "new", true),
        (
            Sc::new()
                .labels(&[])
                .author("NONE", "stranger")
                .comment("NONE", "other"),
            "new",
            true,
        ),
        // More comments than were read: an earlier one may be from inside.
        (
            Sc::new()
                .labels(&[])
                .author("NONE", "stranger")
                .comment("NONE", "other")
                .raw(
                    "comments",
                    json!({"totalCount": 51, "nodes": [{"author": {"login": "other"}, "authorAssociation": "NONE", "createdAt": "2026-10-20T00:00:00Z"}]}),
                ),
            "untriaged",
            true,
        ),
        (Sc::new().labels(&["bug"]).author("NONE", "stranger"), "untriaged", true),
        (
            Sc::new().labels(&[]).author("NONE", "stranger").comment("MEMBER", "x"),
            "untriaged",
            true,
        ),
        (
            Sc::new().labels(&[]).author("NONE", "stranger").comment("OWNER", "x"),
            "untriaged",
            true,
        ),
        (
            Sc::new()
                .labels(&[])
                .author("NONE", "stranger")
                .comment("COLLABORATOR", "x"),
            "untriaged",
            true,
        ),
        (
            Sc::new()
                .labels(&[])
                .author("NONE", "stranger")
                .comment("NONE", "yuvalrakavy"),
            "untriaged",
            true,
        ),
        (
            Sc::new().labels(&[]).author("NONE", "stranger").closed(),
            "closed",
            true,
        ),
        // Inside authors: by association, or listed as a maintainer.
        (Sc::new().labels(&[]).author("OWNER", "x"), "untriaged", false),
        (Sc::new().labels(&[]).author("MEMBER", "x"), "untriaged", false),
        (Sc::new().labels(&[]).author("COLLABORATOR", "x"), "untriaged", false),
        (Sc::new().labels(&[]).author("NONE", "yuvalrakavy"), "untriaged", false),
        // Untriaged: an axis is missing, and it comes before needs-decision.
        (Sc::new().labels(&["bug", "kernel"]), "untriaged", false),
        (Sc::new().labels(&["bug", "p1"]), "untriaged", false),
        (Sc::new().labels(&["p1", "kernel"]), "untriaged", false),
        (Sc::new().labels(&["bug", "todo", "p1", "kernel"]), "untriaged", false),
        (Sc::new().labels(&["bug", "p1", "p2", "kernel"]), "untriaged", false),
        (
            Sc::new().labels(&["needs-decision", "p1", "kernel"]),
            "untriaged",
            false,
        ),
        // A closed issue is closed, whatever else would hold (the first of the chain).
        (Sc::new().closed(), "closed", false),
        (Sc::new().plus("umbrella").closed(), "closed", false),
        (Sc::new().plus("needs-decision").closed(), "closed", false),
        (Sc::new().plus("InProgress").closed(), "closed", false),
        (Sc::new().blocked("OPEN").closed(), "closed", false),
        (Sc::new().labels(&["bug", "kernel"]).closed(), "closed", false),
        (Sc::new().labels(&[]).author("OWNER", "x").closed(), "closed", false),
    ];
    let list: Vec<(String, Sc)> = ids
        .iter()
        .cloned()
        .zip(table.iter().map(|(sc, _, _)| sc.clone()))
        .collect();
    let t = fixture_root();
    let d = doc(&run_with(&t, &gh_scenarios(list), &no_pages(), 0));
    for (i, (sc, status, outside)) in table.iter().enumerate() {
        let x = issue(&d, &ids[i]);
        assert_eq!(
            (x["status"].as_str().unwrap(), x["outside"].as_bool().unwrap()),
            (*status, *outside),
            "scenario {i}: labels {:?} {} {} comments {:?} blocked {:?}: {x}",
            sc.labels,
            sc.assoc,
            sc.author,
            sc.comments,
            sc.blocked_by
        );
    }
    summary_matches_the_issues(&d);
}

#[test]
fn a_repo_that_declares_no_areas_does_not_need_one_to_be_triaged() {
    let ids = open_ids(AIRZONED);
    let t = fixture_root();
    let d = doc(&run_with(
        &t,
        &gh_scenarios(vec![(ids[0].clone(), Sc::new().labels(&["bug", "p1"]))]),
        &no_pages(),
        0,
    ));
    assert_eq!(issue(&d, &ids[0])["status"], "ready");
    assert_eq!(issue(&d, &ids[0])["type"], "bug");
    assert_eq!(issue(&d, &ids[0])["areas"], json!([]));
}

#[test]
fn the_idle_time_limit_of_each_priority_is_judged_on_open_issues_only() {
    let ids = open_ids(STORE);
    // (priority label, idle days, state closed, breached)
    let table: Vec<(&str, i64, bool, bool)> = vec![
        ("p0", 3, false, true),
        ("p0", 2, false, false),
        ("p1", 15, false, true),
        ("p1", 14, false, false),
        ("p2", 61, false, true),
        ("p2", 60, false, false),
        ("p3", 181, false, true),
        ("p3", 180, false, false),
        ("p0", 5, true, false),
        ("p1", 1, false, false),
    ];
    let mut list: Vec<(String, Sc)> = table
        .iter()
        .enumerate()
        .map(|(i, (p, idle, closed, _))| {
            let sc = Sc::new().labels(&["bug", p, "kernel"]).idle(*idle);
            (ids[i].clone(), if *closed { sc.closed() } else { sc })
        })
        .collect();
    // No priority at all: no limit to breach, however long it idles.
    let none = ids[table.len()].clone();
    list.push((none.clone(), Sc::new().labels(&["bug", "kernel"]).idle(400)));
    let t = fixture_root();
    let d = doc(&run_with(&t, &gh_scenarios(list), &no_pages(), 0));
    for (i, (p, idle, closed, breached)) in table.iter().enumerate() {
        let x = issue(&d, &ids[i]);
        assert_eq!(x["idle_days"], *idle, "{p} {idle}");
        assert_eq!(
            x["limit_breached"], *breached,
            "{p} idle {idle} days closed {closed}: {x}"
        );
    }
    assert_eq!(issue(&d, &none)["limit_breached"], false);
    // The tile counts the same issues.
    summary_matches_the_issues(&d);
    assert!(d["meta"]["summary"]["over_time_limit"].as_u64().unwrap() >= 4);
    assert!(d["meta"]["summary"]["p0_open"].as_u64().unwrap() >= 2);
}

fn set_override(id: &str, item: &str, value: DecisionValue, reason: Option<&str>) -> Decision {
    let mut d = decision(
        id,
        OWNER,
        Action::SetOverride,
        Target {
            item: Some(item.into()),
            ..Default::default()
        },
        Some(value),
    );
    d.reason = reason.map(str::to_string);
    d
}

fn target_item(action: Action, id: &str, item: &str) -> Decision {
    decision(
        id,
        OWNER,
        action,
        Target {
            item: Some(item.into()),
            ..Default::default()
        },
        None,
    )
}

fn p(priority: Priority) -> DecisionValue {
    DecisionValue {
        priority: Some(priority),
        ..Default::default()
    }
}

#[test]
fn priority_is_the_override_then_the_project_then_the_label() {
    let ids = open_ids(STORE);
    let labelled = Sc::new().labels(&["bug", "p2", "kernel"]);
    let unlabelled = Sc::new().labels(&["bug", "kernel"]);
    let list = vec![
        (ids[0].clone(), labelled.clone()),   // project p0 over label p2
        (ids[1].clone(), labelled.clone()),   // override p3 over project p0 over label p2
        (ids[2].clone(), labelled.clone()),   // label only
        (ids[3].clone(), unlabelled.clone()), // project only
        (ids[4].clone(), labelled.clone()),   // project p0 cleared: back to the label
        (ids[5].clone(), labelled.clone()),   // override cleared: back to the label
        (ids[6].clone(), labelled.clone()),   // override of type and areas
    ];
    let t = fixture_root();
    let ds = vec![
        set_priority("D_0", &ids[0], Priority::P0),
        set_priority("D_1a", &ids[1], Priority::P0),
        set_override("D_1b", &ids[1], p(Priority::P3), Some("parked")),
        set_priority("D_3", &ids[3], Priority::P1),
        set_priority("D_4a", &ids[4], Priority::P0),
        target_item(Action::ClearPriority, "D_4b", &ids[4]),
        set_override("D_5a", &ids[5], p(Priority::P0), Some("urgent")),
        target_item(Action::ClearOverride, "D_5b", &ids[5]),
        set_override(
            "D_6",
            &ids[6],
            DecisionValue {
                priority: Some(Priority::P0),
                type_: Some("docs".into()),
                areas: Some(vec!["sdl".into()]),
                ..Default::default()
            },
            Some("because"),
        ),
    ];
    let d = doc(&run_with(&t, &gh_scenarios(list), &pages(&ds), 0));

    let prio = |d: &Value, i: usize| issue(d, &ids[i])["priority"].clone();
    assert_eq!(prio(&d, 0)["value"], "p0");
    assert_eq!(prio(&d, 0)["source"], "project");
    assert_eq!(prio(&d, 0)["label"], "p2");
    assert_eq!(prio(&d, 0)["project"], "p0");
    assert_eq!(prio(&d, 0)["override"], Value::Null);

    assert_eq!(prio(&d, 1)["value"], "p3");
    assert_eq!(prio(&d, 1)["source"], "override");
    assert_eq!(prio(&d, 1)["project"], "p0");
    assert_eq!(prio(&d, 1)["label"], "p2");
    assert_eq!(prio(&d, 1)["override"], json!({"value": "p3", "reason": "parked"}));

    assert_eq!(prio(&d, 2)["value"], "p2");
    assert_eq!(prio(&d, 2)["source"], "label");

    assert_eq!(prio(&d, 3)["value"], "p1");
    assert_eq!(prio(&d, 3)["source"], "project");
    assert_eq!(prio(&d, 3)["label"], Value::Null);
    assert_eq!(
        issue(&d, &ids[3])["status"],
        "ready",
        "a project priority triages the issue"
    );

    assert_eq!(prio(&d, 4)["value"], "p2");
    assert_eq!(prio(&d, 4)["source"], "label", "a cleared priority is gone");
    assert_eq!(prio(&d, 4)["project"], Value::Null);
    assert_eq!(prio(&d, 5)["value"], "p2");
    assert_eq!(prio(&d, 5)["source"], "label", "a cleared override is gone");
    assert_eq!(prio(&d, 5)["override"], Value::Null);

    let six = issue(&d, &ids[6]);
    assert_eq!(six["type"], "docs", "an override replaces the type");
    assert_eq!(six["areas"], json!(["sdl"]), "an override replaces the areas");
    assert_eq!(six["priority"]["value"], "p0");
    // Without an override of its own the item keeps what its labels say.
    assert_eq!(issue(&d, &ids[0])["type"], "bug");
    assert_eq!(issue(&d, &ids[0])["areas"], json!(["kernel"]));

    summary_matches_the_issues(&d);
}

#[test]
fn a_pull_requests_status_is_the_first_of_the_list_that_holds() {
    // (draft, mergeable, checks, review decision, status)
    type Row = (
        bool,
        &'static str,
        Option<&'static str>,
        Option<&'static str>,
        &'static str,
    );
    let table: Vec<Row> = vec![
        (true, "CONFLICTING", Some("FAILURE"), Some("CHANGES_REQUESTED"), "draft"),
        (
            false,
            "CONFLICTING",
            Some("FAILURE"),
            Some("CHANGES_REQUESTED"),
            "conflicting",
        ),
        (
            false,
            "MERGEABLE",
            Some("FAILURE"),
            Some("CHANGES_REQUESTED"),
            "checks-failing",
        ),
        (false, "MERGEABLE", Some("ERROR"), Some("APPROVED"), "checks-failing"),
        (
            false,
            "MERGEABLE",
            Some("PENDING"),
            Some("CHANGES_REQUESTED"),
            "waiting-on-author",
        ),
        (
            false,
            "MERGEABLE",
            Some("SUCCESS"),
            Some("CHANGES_REQUESTED"),
            "waiting-on-author",
        ),
        (false, "MERGEABLE", Some("PENDING"), Some("APPROVED"), "checks-running"),
        (false, "MERGEABLE", Some("EXPECTED"), Some("APPROVED"), "checks-running"),
        (false, "MERGEABLE", Some("EXPECTED"), None, "checks-running"),
        (false, "MERGEABLE", Some("SUCCESS"), Some("APPROVED"), "approved"),
        (false, "UNKNOWN", None, Some("APPROVED"), "approved"),
        (false, "MERGEABLE", Some("SUCCESS"), None, "needs-review"),
        (false, "UNKNOWN", None, None, "needs-review"),
        (
            false,
            "MERGEABLE",
            Some("SUCCESS"),
            Some("REVIEW_REQUIRED"),
            "needs-review",
        ),
    ];
    for (draft, mergeable, checks, review, status) in table {
        let t = fixture_root();
        let gh = gh_with(move |req, v| {
            let mut v = v?;
            if req.name == "nodes"
                && let Some(nodes) = v["data"]["nodes"].as_array_mut()
            {
                for n in nodes.iter_mut().filter(|n| n["isDraft"].is_boolean()) {
                    n["isDraft"] = json!(draft);
                    n["mergeable"] = json!(mergeable);
                    n["reviewDecision"] = json!(review);
                    n["state"] = json!("OPEN");
                    n["closedAt"] = Value::Null;
                    n["authorAssociation"] = json!("NONE");
                    n["author"] = json!({"login": "stranger"});
                    n["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = match checks {
                        Some(c) => json!({"state": c}),
                        None => Value::Null,
                    };
                }
            }
            Ok(v)
        });
        let d = doc(&run_with(&t, &gh, &no_pages(), 0));
        let prs: Vec<&Value> = d["issues"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["kind"] == "pull_request")
            .collect();
        assert!(prs.len() >= 3, "{}", prs.len());
        for pr in &prs {
            assert_eq!(
                pr["status"], status,
                "draft {draft} {mergeable} {checks:?} {review:?}: {pr}"
            );
        }
        summary_matches_the_issues(&d);
        if status == "needs-review" {
            // Outside, open and waiting for review: it is waiting on you.
            assert!(d["meta"]["summary"]["waiting_on_you"].as_u64().unwrap() >= prs.len() as u64);
        }
    }
}

#[test]
fn changes_requested_wait_on_the_author_only_when_requested_on_the_latest_push() {
    // (the reviews are read, the changes were requested on the head, status)
    for (read, on_head, status) in [
        (true, true, "waiting-on-author"),
        (true, false, "needs-review"),
        (false, false, "waiting-on-author"),
    ] {
        let t = fixture_root();
        let gh = gh_with(move |req, v| {
            let mut v = v?;
            if req.name == "nodes"
                && let Some(nodes) = v["data"]["nodes"].as_array_mut()
            {
                for n in nodes.iter_mut().filter(|n| n["isDraft"].is_boolean()) {
                    n["isDraft"] = json!(false);
                    n["mergeable"] = json!("MERGEABLE");
                    n["reviewDecision"] = json!("CHANGES_REQUESTED");
                    n["state"] = json!("OPEN");
                    n["closedAt"] = Value::Null;
                    n["commits"]["nodes"][0]["commit"]["statusCheckRollup"] = json!({"state": "SUCCESS"});
                    let head = n["commits"]["nodes"][0]["commit"]["oid"].clone();
                    let reviewed = if on_head { head } else { json!("0000000aaaaaaa") };
                    if read {
                        n["latestOpinionatedReviews"] = json!({"nodes": [
                            {"state": "APPROVED", "commit": {"oid": "1111111bbbbbbb"}},
                            {"state": "CHANGES_REQUESTED", "commit": {"oid": reviewed}}
                        ]});
                    }
                }
            }
            Ok(v)
        });
        let d = doc(&run_with(&t, &gh, &no_pages(), 0));
        let prs: Vec<&Value> = d["issues"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["kind"] == "pull_request")
            .collect();
        assert!(prs.len() >= 3, "{}", prs.len());
        for pr in &prs {
            assert_eq!(pr["status"], status, "read {read} on head {on_head}: {pr}");
        }
    }
}

#[test]
fn a_closed_item_leaves_the_outbox_once_the_history_window_has_passed_it_by_even_when_reads_fail() {
    const DAY: i64 = 24 * 60;
    let ids = open_ids(STORE);
    let list = vec![
        // Closed 29 days before the first cycle: exactly at the window's edge one day later.
        (ids[0].clone(), Sc::new().closed_minutes_ago(29 * DAY, "CLOSED")),
        // Closed 31 days before the first cycle: never shown.
        (ids[1].clone(), Sc::new().closed_minutes_ago(31 * DAY, "CLOSED")),
        // Open, with a closedAt long ago (reopened): the window is for closed items only.
        (ids[2].clone(), Sc::new().closed_minutes_ago(100 * DAY, "OPEN")),
    ];
    let t = fixture_root();
    let g = gh_scenarios(list);
    let has = |d: &Value, id: &str| d["issues"].as_array().unwrap().iter().any(|i| i["id"] == id);
    let d = doc(&run_with(&t, &g, &no_pages(), 0));
    assert!(has(&d, &ids[0]));
    assert!(!has(&d, &ids[1]), "older than the window already");
    assert!(has(&d, &ids[2]));
    // Every read fails from now on: what is shown is what was seen.
    let down = gh_with(|_, _| Err(Error::GitHub("down".into())));
    let d1 = doc(&run_with(&t, &down, &no_pages(), DAY));
    assert!(
        has(&d1, &ids[0]),
        "closed exactly 30 days ago is still inside the window"
    );
    let d2 = doc(&run_with(&t, &down, &no_pages(), DAY + 1));
    assert!(!has(&d2, &ids[0]), "a minute older than the window");
    assert!(has(&d2, &ids[2]), "an open item is never evicted");
    assert!(
        d2["meta"]["repos"]
            .as_array()
            .unwrap()
            .iter()
            .all(|r| r["stale"] == true && r["error"].is_string())
    );
    summary_matches_the_issues(&d2);
}

#[test]
fn a_repos_read_state_in_the_meta_document() {
    let t = fixture_root();
    // Before any read: unread, so stale, with no node id.
    let config = ConfigSet::load(t.path()).unwrap();
    t.root
        .mutate(at(0), |tx| {
            config.record(tx)?;
            cycle::take_sequence(tx, "home").map(|_| ())
        })
        .unwrap();
    let seq = t.root.read(|l| ledger::max_seq(l.conn())).unwrap();
    let before = t
        .root
        .read(|l| derive::derive(l.conn(), &config, "home", seq, at(0)))
        .unwrap();
    for r in before.meta["repos"].as_array().unwrap() {
        assert_eq!(r["stale"], true, "{r}");
        assert_eq!(r["read"], "full");
        assert_eq!(r["read_at"], Value::Null);
        assert!(r.get("id").is_none() || r["id"].is_null());
    }
    assert!(before.issues.is_empty());

    // After a good read, a derive up to twice the sync interval (5 minutes) later is fresh.
    let r = run_with(&t, &gh(), &no_pages(), 100);
    let seq = r.sequence;
    let at_age = |minutes: i64| {
        let d = t
            .root
            .read(|l| derive::derive(l.conn(), &config, "home", seq, at(100 + minutes)))
            .unwrap();
        d.meta["repos"]
            .as_array()
            .unwrap()
            .iter()
            .map(|r| r["stale"].clone())
            .collect::<Vec<_>>()
    };
    assert!(at_age(0).iter().all(|s| s == false));
    assert!(
        at_age(10).iter().all(|s| s == false),
        "exactly twice the interval is fresh"
    );
    assert!(at_age(11).iter().all(|s| s == true));
    let fresh = t
        .root
        .read(|l| derive::derive(l.conn(), &config, "home", seq, at(100)))
        .unwrap();
    let repo = |name: &str| {
        fresh.meta["repos"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["repo"] == name)
            .unwrap()
            .clone()
    };
    assert_eq!(repo(STORE)["read"], "full");
    assert!(repo(STORE)["id"].as_str().unwrap().starts_with("R_"));
    assert!(repo(STORE)["read_at"].is_string());
    assert_eq!(repo(STORE)["read_only"], json!({"value": false, "why": null}));
    assert_eq!(repo(RHAI)["read_only"], json!({"value": true, "why": "declared"}));
    // The next read is incremental, and says so.
    let r2 = run_with(&t, &gh(), &no_pages(), 110);
    let d = doc(&r2);
    let store = d["meta"]["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["repo"] == STORE)
        .unwrap()
        .clone();
    assert_eq!(store["read"], "incremental");
}

#[test]
fn a_repo_the_account_cannot_write_to_is_read_only_whatever_the_config_says() {
    for (permission, read_only) in [
        ("READ", true),
        ("TRIAGE", true),
        ("NONE", true),
        ("WRITE", false),
        ("MAINTAIN", false),
        ("ADMIN", false),
    ] {
        let t = fixture_root();
        let gh = gh_with(move |req, v| {
            let mut v = v?;
            if req.name == "repo" && req.variables["name"] == "airzoned" {
                v["data"]["repository"]["viewerPermission"] = json!(permission);
            }
            Ok(v)
        });
        let d = doc(&run_with(&t, &gh, &no_pages(), 0));
        let r = d["meta"]["repos"]
            .as_array()
            .unwrap()
            .iter()
            .find(|r| r["repo"] == AIRZONED)
            .unwrap()
            .clone();
        let want = if read_only {
            json!({"value": true, "why": "no_write_access"})
        } else {
            json!({"value": false, "why": null})
        };
        assert_eq!(r["read_only"], want, "{permission}");
    }
}

// ---- acks ----

fn cancel(id: &str, launch: &str) -> Decision {
    decision(
        id,
        OWNER,
        Action::CancelLaunch,
        Target {
            launch: Some(launch.into()),
            ..Default::default()
        },
        None,
    )
}

fn with_launches(items: &[(&str, &str)]) -> Pages {
    Pages {
        launches: items
            .iter()
            .map(|(id, item)| serde_json::to_value(launch_doc(id, "home", &[item], "lanes")).unwrap())
            .collect(),
        ..Pages::default()
    }
}

#[test]
fn an_ack_says_why_it_was_refused_or_cancelled() {
    let ids = open_ids(STORE);
    let t = fixture_root();
    let g = gh();
    // L_one is cancelled by D_c1; D_c2 cancels it again, too late to do anything.
    let mut p = with_launches(&[("L_one", &ids[0])]);
    p.decisions = vec![
        serde_json::to_value(cancel("D_c1", "L_one")).unwrap(),
        serde_json::to_value(cancel("D_c2", "L_one")).unwrap(),
    ];
    let d = doc(&run_with(&t, &g, &p, 10));
    assert_eq!(ack(&d, "D_c1")["result"], "applied");
    assert!(ack(&d, "D_c1").get("reason").is_none());
    let c2 = ack(&d, "D_c2");
    assert_eq!(c2["result"], "refused");
    assert_eq!(c2["reason"], "launch L_one was already withdrawn by another decision");
    // An undone decision says by which.
    let mut p = pages(&[set_priority("D_s", &ids[1], Priority::P0), undo("D_u", "D_s")]);
    let d = doc(&run_with(&t, &g, &p, 20));
    assert_eq!(ack(&d, "D_s")["result"], "cancelled");
    assert_eq!(ack(&d, "D_s")["reason"], "undone by D_u");
    assert!(ack(&d, "D_u").get("reason").is_none());
    // A cancel of a launch nobody requested is applied, nothing to refuse.
    p = pages(&[cancel("D_ghost", "L_nobody")]);
    let d = doc(&run_with(&t, &g, &p, 30));
    assert_eq!(ack(&d, "D_ghost")["result"], "applied");
}

#[test]
fn an_ack_is_carried_for_exactly_thirty_days_after_the_cycle_that_consumed_the_decision() {
    let ids = open_ids(STORE);
    let t = fixture_root();
    let g = gh();
    run_with(&t, &g, &no_pages(), 0);
    run_with(&t, &g, &pages(&[set_priority("D_a", &ids[0], Priority::P1)]), 10);
    let day = 24 * 60;
    let on_the_day = doc(&run_with(&t, &g, &no_pages(), 10 + 30 * day));
    assert_eq!(ack(&on_the_day, "D_a")["result"], "applied");
    let a_minute_later = doc(&run_with(&t, &g, &no_pages(), 10 + 30 * day + 1));
    assert!(a_minute_later["acks"].as_array().unwrap().is_empty());
}

#[test]
fn an_ack_names_the_first_outbox_of_its_own_project_after_the_decision() {
    let ids = open_ids(STORE);
    let t = fixture_root();
    let g = gh();
    run_with(&t, &g, &no_pages(), 0);
    // The decision is recorded between cycles, then another project writes an outbox first.
    t.root
        .mutate(at(5), |tx| {
            issue_dashboard_core::decision::ingest(tx, "home", OWNER, &set_priority("D_x", &ids[0], Priority::P1))
        })
        .unwrap();
    let kernel = cycle::cycle(&t.root, &g, "kernel", &no_pages(), &|| at(6), PAGE).unwrap();
    let home = run_with(&t, &g, &no_pages(), 7);
    assert!(kernel.sequence < home.sequence);
    let d = doc(&home);
    assert_eq!(ack(&d, "D_x")["cycle"], home.sequence);
    let d2 = doc(&run_with(&t, &g, &no_pages(), 8));
    assert_eq!(ack(&d2, "D_x")["cycle"], home.sequence, "not the latest outbox");
}

#[test]
fn launch_status_reports_each_launch_of_the_project_with_its_current_facts() {
    let ids = open_ids(STORE);
    let t = fixture_root();
    let g = gh();
    run_with(&t, &g, &no_pages(), 0);
    let mut p = with_launches(&[("L_done", &ids[0]), ("L_waiting", &ids[1])]);
    p.launches
        .push(serde_json::to_value(launch_doc("L_other", "kernel", &[&ids[2]], "lanes")).unwrap());
    // L_other belongs to another project: ingest skips it for this one.
    run_with(&t, &g, &p, 10);
    let token = match t
        .root
        .mutate(at(11), |tx| launch::reserve(tx, "L_done", "dispatcher-1", &|_| Ok(2)))
        .unwrap()
    {
        Reservation::Reserved { token, .. } => token,
        other => panic!("{other:?}"),
    };
    t.root
        .mutate(at(12), |tx| launch::started(tx, &token, "session-1"))
        .unwrap();
    t.root
        .mutate(at(13), |tx| launch::started(tx, &token, "session-2"))
        .unwrap();
    t.root
        .mutate(at(14), |tx| launch::ended(tx, &token, EndState::Landed))
        .unwrap();
    let d = doc(&run_with(&t, &g, &no_pages(), 20));
    let list = d["launch_status"].as_array().unwrap();
    assert_eq!(list.len(), 2, "only this project's launches: {list:?}");
    let done = list.iter().find(|l| l["id"] == "L_done").unwrap();
    assert_eq!(done["state"], "released");
    assert_eq!(done["token"], token.as_str());
    assert_eq!(done["session"], "session-1");
    assert_eq!(done["dispatcher"], "dispatcher-1");
    assert_eq!(done["end_state"], "landed");
    assert!(
        done["reserved_at"]
            .as_str()
            .unwrap()
            .starts_with(&at(11).format("%Y-%m-%dT%H:%M").to_string())
    );
    assert_eq!(done["orphans"].as_array().unwrap().len(), 1);
    assert_eq!(done["orphans"][0]["session"], "session-2");
    assert!(
        done["updated_at"]
            .as_str()
            .unwrap()
            .starts_with(&at(14).format("%Y-%m-%dT%H:%M").to_string())
    );
    let waiting = list.iter().find(|l| l["id"] == "L_waiting").unwrap();
    assert_eq!(waiting["state"], "requested");
    assert_eq!(waiting["token"], Value::Null);
    assert_eq!(waiting["dispatcher"], Value::Null);
    assert_eq!(waiting["end_state"], Value::Null);
    assert!(
        waiting["updated_at"]
            .as_str()
            .unwrap()
            .starts_with(&at(10).format("%Y-%m-%dT%H:%M").to_string())
    );
}

// ---- ingest ----

fn doc_of(path: &str) -> Value {
    let p = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../schemas/examples")
        .join(path);
    serde_json::from_str(&std::fs::read_to_string(p).unwrap()).unwrap()
}

#[test]
fn launch_documents_that_are_not_launch_requests_are_skipped_and_reported() {
    let ids = open_ids(STORE);
    let t = fixture_root();
    let mut p = with_launches(&[("L_good", &ids[0])]);
    // A not_sent record (it names this project and its owner) and the thread document.
    let mut not_sent = doc_of("page-launch/not-sent.json");
    not_sent["project"] = json!("home");
    not_sent["author"] = json!(OWNER);
    p.launches.push(not_sent);
    p.launches.push(doc_of("page-launch/thread.json"));
    // A thread record of this project, by its owner: still not a launch request.
    let mut record = doc_of("page-launch/thread-record.json");
    record["project"] = json!("home");
    record["author"] = json!(OWNER);
    p.launches.push(record);
    // Against its schema: an id with a slash; no items.
    let mut bad = serde_json::to_value(launch_doc("L_bad", "home", &[&ids[1]], "lanes")).unwrap();
    bad["items"] = json!([]);
    p.launches.push(bad);
    p.launches.push(doc_of("page-launch/bad-id-with-slash.json"));
    // Not this project's, and not its owner's.
    p.launches
        .push(serde_json::to_value(launch_doc("L_elsewhere", "kernel", &[&ids[2]], "lanes")).unwrap());
    let mut stranger = launch_doc("L_stranger", "home", &[&ids[3]], "lanes");
    stranger.author = "user_other".into();
    p.launches.push(serde_json::to_value(stranger).unwrap());
    let r = run_with(&t, &gh(), &p, 10);
    assert_eq!(r.ingest.launches, 1, "{:?}", r.ingest.skipped);
    assert_eq!(r.ingest.skipped.len(), 7, "{:?}", r.ingest.skipped);
    let reason = |id: &str| {
        r.ingest
            .skipped
            .iter()
            .find(|s| s.id.as_deref() == Some(id))
            .unwrap_or_else(|| panic!("{id} not skipped: {:?}", r.ingest.skipped))
            .reason
            .clone()
    };
    assert!(
        reason("L_20261023_080001_cd34").contains("not_sent"),
        "{}",
        reason("L_20261023_080001_cd34")
    );
    assert!(reason("_thread").contains("thread"), "{}", reason("_thread"));
    let t_rec = "T_20261023_080100_k3m9x2qa";
    assert!(reason(t_rec).contains("thread records"), "{}", reason(t_rec));
    assert!(reason("L_elsewhere").contains("project"), "{}", reason("L_elsewhere"));
    assert!(reason("L_stranger").contains("owner_id"), "{}", reason("L_stranger"));
    for id in [
        "L_bad",
        "L_elsewhere",
        "L_stranger",
        "L_20261023_080001_cd34",
        "_thread",
        "T_20261023_080100_k3m9x2qa",
    ] {
        assert!(t.root.read(|l| launch::load(l.conn(), id)).unwrap().is_none(), "{id}");
    }
    assert!(t.root.read(|l| launch::load(l.conn(), "L_good")).unwrap().is_some());
    // The same pages again: the launch is a repeat, not new.
    let again = run_with(&t, &gh(), &p, 20);
    assert_eq!(again.ingest.launches, 0);
}

#[test]
fn views_and_requests_count_only_from_the_owner_for_this_project() {
    let ids = open_ids(STORE);
    let t = fixture_root();
    let view = |id: &str, project: &str, author: &str| {
        json!({"id": id, "project": project, "author": author, "created_at": "2026-10-23T08:30:00Z",
               "name": id, "include": [], "exclude": []})
    };
    let request = |id: &str, project: &str, author: &str| {
        json!({"id": id, "project": project, "author": author, "created_at": "2026-10-23T08:31:00Z",
               "kind": "triage", "items": [ids[0]]})
    };
    let p = Pages {
        views: vec![
            view("V_one", "home", OWNER),
            view("V_two", "home", OWNER),
            view("V_elsewhere", "kernel", OWNER),
            view("V_stranger", "home", "user_other"),
            json!({"id": "V_invalid", "project": "home", "author": OWNER}),
        ],
        requests: vec![
            request("R_one", "home", OWNER),
            request("R_elsewhere", "kernel", OWNER),
            request("R_stranger", "home", "user_other"),
            json!({"id": "R_invalid", "project": "home", "author": OWNER, "kind": "no-such-kind"}),
        ],
        ..Pages::default()
    };
    let r = run_with(&t, &gh(), &p, 10);
    assert_eq!((r.ingest.views, r.ingest.requests), (2, 1), "{:?}", r.ingest.skipped);
    let skipped: Vec<_> = r.ingest.skipped.iter().map(|s| s.id.clone().unwrap()).collect();
    for id in [
        "V_elsewhere",
        "V_stranger",
        "V_invalid",
        "R_elsewhere",
        "R_stranger",
        "R_invalid",
    ] {
        assert!(skipped.contains(&id.to_string()), "{id} in {skipped:?}");
    }
    assert_eq!(skipped.len(), 6);
}

#[test]
fn a_cancel_withdraws_a_requested_or_queued_launch_of_its_own_project_only() {
    let ids = open_ids(STORE);
    let t = fixture_root();
    let g = gh();
    run_with(&t, &g, &no_pages(), 0);
    // L_a takes the only slot; L_b waits for it.
    let p = with_launches(&[("L_a", &ids[0]), ("L_b", &ids[1]), ("L_c", &ids[2])]);
    run_with(&t, &g, &p, 10);
    let res = t
        .root
        .mutate(at(11), |tx| launch::reserve(tx, "L_a", "dispatcher-1", &|_| Ok(1)))
        .unwrap();
    assert!(matches!(res, Reservation::Reserved { .. }));
    let res = t
        .root
        .mutate(at(12), |tx| launch::reserve(tx, "L_b", "dispatcher-1", &|_| Ok(1)))
        .unwrap();
    assert!(matches!(res, Reservation::Queued { .. }), "{res:?}");
    let state = |id: &str| t.root.read(|l| launch::load(l.conn(), id)).unwrap().unwrap().state;
    assert_eq!(
        (state("L_a"), state("L_b"), state("L_c")),
        (State::Reserved, State::Queued, State::Requested)
    );
    // A launch of another project, and a decision that is not a cancel but names a launch.
    t.root
        .mutate(at(13), |tx| {
            launch::request(tx, &launch_doc("L_k", "kernel", &[&ids[3]], "lanes"), Via::Ingest).map(|_| ())
        })
        .unwrap();
    let release = decision(
        "D_release",
        OWNER,
        Action::ReleaseReservation,
        Target {
            launch: Some("L_c".into()),
            token: Some("tok_1".into()),
            ..Default::default()
        },
        None,
    );
    let p = pages(&[cancel("D_b", "L_b"), cancel("D_k", "L_k"), release]);
    let r = run_with(&t, &g, &p, 20);
    assert_eq!(r.ingest.withdrawn, vec!["L_b".to_string()]);
    assert_eq!(state("L_b"), State::Withdrawn, "a queued launch can be cancelled");
    assert_eq!(
        state("L_k"),
        State::Requested,
        "another project's launch is not this project's to cancel"
    );
    assert_eq!(state("L_c"), State::Requested, "only a cancel_launch cancels");
    assert_eq!(state("L_a"), State::Reserved);
}

fn link(id: &str, number: u64, state: Option<&str>) -> Value {
    let mut v = json!({"id": id, "number": number, "repository": {"nameWithOwner": STORE}});
    if let Some(s) = state {
        v["state"] = json!(s);
    }
    v
}

#[test]
fn links_are_resolved_from_the_ledger_first_and_from_the_read_otherwise() {
    let ids = open_ids(STORE);
    let base = doc(&run_with(&fixture_root(), &gh(), &no_pages(), 0));
    let alias = |i: usize| issue(&base, &ids[i])["alias"].as_str().unwrap().to_string();
    // ids[3] is open in the ledger, ids[4] closed there; the reads that link to them are stale
    // and say the opposite.
    let list = vec![
        (ids[4].clone(), Sc::new().closed()),
        (
            ids[0].clone(),
            Sc::new().raw("blockedBy", json!({"nodes": [link(&ids[3], 1, Some("CLOSED"))]})),
        ),
        (
            ids[1].clone(),
            Sc::new().raw("blockedBy", json!({"nodes": [link(&ids[4], 2, Some("OPEN"))]})),
        ),
        (
            ids[2].clone(),
            Sc::new()
                .raw("parent", link(&ids[5], 3, Some("CLOSED")))
                .raw("subIssues", json!({"nodes": [link(&ids[6], 4, Some("OPEN"))]}))
                .raw(
                    "blocking",
                    json!({"nodes": [link("I_somewhere_else", 9100, Some("OPEN"))]}),
                )
                .raw("createdAt", json!("2026-01-02T03:04:05Z")),
        ),
        // A linked item with no state at all is open.
        (
            ids[7].clone(),
            Sc::new().raw("blockedBy", json!({"nodes": [link("I_no_state", 9200, None)]})),
        ),
        // Last touched in the future: idle for no days, not for a negative number.
        (ids[8].clone(), Sc::new().idle(-3)),
    ];
    let t = fixture_root();
    let d = doc(&run_with(&t, &gh_scenarios(list), &no_pages(), 0));
    let x = |i: usize| issue(&d, &ids[i]).clone();
    assert_eq!(x(0)["status"], "blocked", "the ledger says it is open");
    assert_eq!(x(0)["links"]["blocked_by"], json!([alias(3)]));
    assert_eq!(x(1)["status"], "ready", "the ledger says it is closed");
    assert_eq!(x(1)["links"]["blocked_by"], json!([alias(4)]));
    assert_eq!(x(2)["links"]["parent"], json!(alias(5)));
    assert_eq!(x(2)["links"]["children"], json!([alias(6)]));
    assert_eq!(
        x(2)["links"]["blocks"],
        json!(["yuvalrakavy/Store#9100"]),
        "unknown to the ledger: the read's alias"
    );
    assert_eq!(x(2)["created_at"], "2026-01-02T03:04:05.000Z");
    assert_eq!(x(7)["status"], "blocked");
    assert_eq!(x(7)["links"]["blocked_by"], json!(["yuvalrakavy/Store#9200"]));
    assert_eq!(x(8)["idle_days"], 0);
    // Nothing else moved.
    assert_eq!(x(3)["links"]["blocked_by"], json!([]));
    assert_eq!(x(3)["links"]["parent"], Value::Null);
}

#[test]
fn a_repo_that_has_never_been_read_does_not_hide_the_others() {
    let t = fixture_root();
    let down = gh_with(|req, v| {
        if req.name == "repo" && req.variables["name"] == "rhai" {
            return Err(Error::GitHub("rhai is down".into()));
        }
        v
    });
    let d = doc(&run_with(&t, &down, &no_pages(), 0));
    let repos: Vec<&str> = d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .map(|i| i["alias"].as_str().unwrap().split('#').next().unwrap())
        .collect();
    assert!(repos.contains(&STORE) && repos.contains(&AIRZONED), "{repos:?}");
    assert!(!repos.contains(&RHAI));
    let rhai = d["meta"]["repos"]
        .as_array()
        .unwrap()
        .iter()
        .find(|r| r["repo"] == RHAI)
        .unwrap()
        .clone();
    assert_eq!(rhai["stale"], true);
    assert!(rhai["error"].as_str().unwrap().contains("rhai is down"));
    assert_eq!(d["meta"]["repos"].as_array().unwrap().len(), 3);
}

#[test]
fn a_member_with_no_observation_is_left_out_not_an_error() {
    let ids = open_ids(STORE);
    let before = doc(&run_with(&fixture_root(), &gh(), &no_pages(), 0))["issues"]
        .as_array()
        .unwrap()
        .len();
    // GitHub finds the item in a search but cannot resolve it by id: it is a member nobody has
    // ever seen.
    let unseen = ids[0].clone();
    let gh = gh_with(move |req, v| {
        let mut v = v?;
        if req.name == "nodes"
            && let Some(nodes) = v["data"]["nodes"].as_array_mut()
        {
            for n in nodes.iter_mut() {
                if n["id"] == unseen.as_str() {
                    *n = Value::Null;
                }
            }
        }
        Ok(v)
    });
    let t = fixture_root();
    let d = doc(&run_with(&t, &gh, &no_pages(), 0));
    assert_eq!(d["issues"].as_array().unwrap().len(), before - 1);
}

#[test]
fn the_default_view_is_carried_into_the_meta_document() {
    let t = fixture_root();
    let p = t.path().join("projects/home.toml");
    let text = std::fs::read_to_string(&p)
        .unwrap()
        .replace("history_days = 30\n", "history_days = 30\ndefault_view = \"triage\"\n");
    std::fs::write(&p, text).unwrap();
    let d = doc(&run_with(&t, &gh(), &no_pages(), 0));
    assert_eq!(d["meta"]["default_view"], "triage");
}

#[test]
fn the_outbox_is_checked_against_its_schemas_part_by_part() {
    let t = fixture_root();
    let r = run_with(
        &t,
        &gh(),
        &pages(&[set_priority("D_a", &open_ids(STORE)[0], Priority::P1)]),
        0,
    );
    let r2 = run_with(&t, &gh(), &no_pages(), 1);
    let config = ConfigSet::load(t.path()).unwrap();
    let mut good = t
        .root
        .read(|l| derive::derive(l.conn(), &config, "home", r2.sequence, at(2)))
        .unwrap();
    assert!(r.sequence < r2.sequence);
    assert!(!good.acks.is_empty() && !good.issues.is_empty());
    assert!(derive::validate(&good).is_ok());
    // Launch status needs a launch.
    t.root
        .mutate(at(3), |tx| {
            issue_dashboard_core::launch::request(
                tx,
                &launch_doc("L_v", "home", &[&open_ids(STORE)[1]], "lanes"),
                Via::Ingest,
            )
            .map(|_| ())
        })
        .unwrap();
    good = t
        .root
        .read(|l| derive::derive(l.conn(), &config, "home", r2.sequence, at(4)))
        .unwrap();
    assert!(!good.launch_status.is_empty());
    let break_it = |f: &dyn Fn(&mut derive::Outbox)| {
        let mut o = good.clone();
        f(&mut o);
        assert!(derive::validate(&o).is_err());
    };
    break_it(&|o| o.meta = json!({"schema": 1}));
    break_it(&|o| o.issues[0] = json!({"id": "x"}));
    break_it(&|o| o.acks[0] = json!({"id": "x"}));
    break_it(&|o| o.launch_status[0] = json!({"id": "x"}));
}

#[test]
fn the_ingest_report_counts_what_was_new_and_what_was_refused() {
    let ids = open_ids(STORE);
    let t = fixture_root();
    let mut stranger = set_priority("D_stranger", &ids[1], Priority::P1);
    stranger.author = "user_other".into();
    let mut p = pages(&[set_priority("D_owner", &ids[0], Priority::P1), stranger]);
    p.decisions.push(json!({"id": "D_garbage"}));
    p.launches = with_launches(&[("L_one", &ids[2])]).launches;
    let r = run_with(&t, &gh(), &p, 10);
    assert_eq!(
        (r.ingest.decided, r.ingest.refused, r.ingest.launches),
        (2, 1, 1),
        "{:?}",
        r.ingest
    );
    assert_eq!(r.ingest.skipped.len(), 1);
    assert_eq!(r.ingest.skipped[0].collection, "decisions");
    assert_eq!(r.ingest.skipped[0].id.as_deref(), Some("D_garbage"));
    // The same pages again: nothing is new.
    let again = run_with(&t, &gh(), &p, 20);
    assert_eq!(
        (again.ingest.decided, again.ingest.refused, again.ingest.launches),
        (0, 0, 0),
        "{:?}",
        again.ingest
    );
    assert_eq!(again.ingest.skipped.len(), 1);
}

#[test]
fn a_view_or_request_ingested_once_cannot_be_replaced_by_another_document() {
    let ids = open_ids(STORE);
    let t = fixture_root();
    let view = |name: &str| {
        json!({"id": "V_one", "project": "home", "author": OWNER, "created_at": "2026-10-23T08:30:00Z",
               "name": name, "include": [], "exclude": []})
    };
    let request = |items: &[&str]| {
        json!({"id": "R_one", "project": "home", "author": OWNER, "created_at": "2026-10-23T08:31:00Z",
               "kind": "triage", "items": items})
    };
    let first = Pages {
        views: vec![view("mine")],
        requests: vec![request(&[&ids[0]])],
        ..Pages::default()
    };
    let r = run_with(&t, &gh(), &first, 10);
    assert_eq!(
        (r.ingest.views, r.ingest.requests, r.ingest.skipped.len()),
        (1, 1, 0),
        "{:?}",
        r.ingest
    );
    // The same documents: repeats, not new and not skipped.
    let r = run_with(&t, &gh(), &first, 20);
    assert_eq!(
        (r.ingest.views, r.ingest.requests, r.ingest.skipped.len()),
        (0, 0, 0),
        "{:?}",
        r.ingest
    );
    // Different documents under the same ids are refused and reported.
    let changed = Pages {
        views: vec![view("renamed")],
        requests: vec![request(&[&ids[0], &ids[1]])],
        ..Pages::default()
    };
    let r = run_with(&t, &gh(), &changed, 30);
    assert_eq!((r.ingest.views, r.ingest.requests), (0, 0));
    assert_eq!(r.ingest.skipped.len(), 2, "{:?}", r.ingest.skipped);
    assert!(
        r.ingest.skipped.iter().all(|s| s.reason.contains("different document")),
        "{:?}",
        r.ingest.skipped
    );
}

#[test]
fn the_pages_file_accepts_no_collection_it_does_not_know_and_needs_none_it_has() {
    let ok: Pages = serde_json::from_value(json!({})).unwrap();
    assert_eq!(ok, Pages::default());
    let some: Pages = serde_json::from_value(json!({"views": [], "launches": []})).unwrap();
    assert_eq!(some, Pages::default());
    assert!(serde_json::from_value::<Pages>(json!({"decisions": [], "comments": []})).is_err());
}

#[test]
fn a_merged_or_closed_pull_request_is_merged_or_closed_not_waiting_for_review() {
    for (state, status) in [("MERGED", "merged"), ("CLOSED", "closed")] {
        let t = fixture_root();
        let gh = gh_with(move |req, v| {
            let mut v = v?;
            if req.name == "nodes"
                && let Some(nodes) = v["data"]["nodes"].as_array_mut()
            {
                for n in nodes.iter_mut().filter(|n| n["isDraft"].is_boolean()) {
                    // Every other field would make it waiting on you (outside, needs review).
                    n["isDraft"] = json!(false);
                    n["mergeable"] = json!("MERGEABLE");
                    n["reviewDecision"] = Value::Null;
                    n["state"] = json!(state);
                    n["closedAt"] = json!(at(-60).to_rfc3339());
                    n["authorAssociation"] = json!("NONE");
                    n["author"] = json!({"login": "stranger"});
                }
            }
            Ok(v)
        });
        let d = doc(&run_with(&t, &gh, &no_pages(), 0));
        let prs: Vec<&Value> = d["issues"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|i| i["kind"] == "pull_request")
            .collect();
        assert!(prs.len() >= 3, "{}", prs.len());
        for pr in &prs {
            assert_eq!(pr["status"], status, "{state}: {pr}");
        }
        summary_matches_the_issues(&d);
    }
}

fn dismiss(id: &str, author: &str, item: &str, closed_at: &str) -> Decision {
    decision(
        id,
        author,
        Action::DismissClosed,
        Target {
            item: Some(item.into()),
            ..Default::default()
        },
        Some(DecisionValue {
            closed_at: Some(closed_at.into()),
            ..Default::default()
        }),
    )
}

fn full_with(t: &TempRoot, gh: &Replay, p: &Pages, minutes: i64) -> Value {
    doc(&cycle::cycle_full(&t.root, gh, None, "home", p, None, &move || at(minutes), PAGE).unwrap())
}

#[test]
fn a_dismissed_close_sets_dismissed_at_and_undo_clears_it() {
    let ids = open_ids(STORE);
    let t = fixture_root();
    let g = gh_scenarios(vec![(ids[0].clone(), Sc::new().closed_minutes_ago(60, "CLOSED"))]);
    let d = doc(&run_with(&t, &g, &no_pages(), 0));
    let closed_at = issue(&d, &ids[0])["closed_at"].as_str().unwrap().to_string();
    assert_eq!(issue(&d, &ids[0])["dismissed_at"], Value::Null);
    let d = doc(&run_with(
        &t,
        &g,
        &pages(&[dismiss("D_dis", OWNER, &ids[0], &closed_at)]),
        10,
    ));
    assert_eq!(ack(&d, "D_dis")["result"], "applied", "{}", ack(&d, "D_dis"));
    assert_eq!(issue(&d, &ids[0])["dismissed_at"], "2026-10-23T08:00:00.000Z");
    // Still in the outbox, so Trends counts it.
    assert_eq!(issue(&d, &ids[0])["state"], "closed");
    let d = doc(&run_with(&t, &g, &pages(&[undo("D_undis", "D_dis")]), 20));
    assert_eq!(ack(&d, "D_dis")["result"], "cancelled");
    assert_eq!(issue(&d, &ids[0])["dismissed_at"], Value::Null);
}

#[test]
fn a_dismiss_of_an_open_item_another_close_or_by_a_stranger_is_refused() {
    let ids = open_ids(STORE);
    let t = fixture_root();
    let g = gh_scenarios(vec![(ids[0].clone(), Sc::new().closed_minutes_ago(60, "CLOSED"))]);
    let d = doc(&run_with(&t, &g, &no_pages(), 0));
    let closed_at = issue(&d, &ids[0])["closed_at"].as_str().unwrap().to_string();
    let d = doc(&run_with(
        &t,
        &g,
        &pages(&[
            dismiss("D_open", OWNER, &ids[1], &closed_at),
            dismiss("D_other", OWNER, &ids[0], "2026-01-01T00:00:00Z"),
            dismiss("D_stranger", "user_stranger", &ids[0], &closed_at),
        ]),
        10,
    ));
    let refused = |id: &str, part: &str| {
        let a = ack(&d, id);
        assert_eq!(a["result"], "refused", "{id}: {a}");
        assert!(a["reason"].as_str().unwrap().contains(part), "{id}: {a}");
    };
    refused("D_open", "only a closed item can be dismissed");
    refused("D_other", "the close the page showed is gone");
    refused("D_stranger", "is not the project's owner_id");
    assert_eq!(issue(&d, &ids[0])["dismissed_at"], Value::Null);
    assert_eq!(issue(&d, &ids[1])["dismissed_at"], Value::Null);
}

#[test]
fn a_dismissal_lapses_when_the_item_is_reopened_and_does_not_hide_a_later_close() {
    let ids = open_ids(STORE);
    let t = fixture_root();
    let closed = gh_scenarios(vec![(ids[0].clone(), Sc::new().closed_minutes_ago(60, "CLOSED"))]);
    let d = doc(&run_with(&t, &closed, &no_pages(), 0));
    let closed_at = issue(&d, &ids[0])["closed_at"].as_str().unwrap().to_string();
    let d = doc(&run_with(
        &t,
        &closed,
        &pages(&[dismiss("D_dis", OWNER, &ids[0], &closed_at)]),
        10,
    ));
    assert!(issue(&d, &ids[0])["dismissed_at"].is_string());
    // Reopened (GitHub keeps the old closedAt on a reopened issue); the daily full read sees it.
    const DAY: i64 = 24 * 60;
    let reopened = gh_scenarios(vec![(
        ids[0].clone(),
        Sc::new()
            .closed_minutes_ago(60, "OPEN")
            .raw("updatedAt", json!(at(DAY + 15).to_rfc3339())),
    )]);
    let d = full_with(&t, &reopened, &no_pages(), DAY + 20);
    assert_eq!(issue(&d, &ids[0])["state"], "open");
    assert_eq!(issue(&d, &ids[0])["dismissed_at"], Value::Null);
    // Closed again, later: a new close the owner has not dismissed.
    let again = gh_scenarios(vec![(
        ids[0].clone(),
        Sc::new()
            .closed_minutes_ago(-(2 * DAY + 25), "CLOSED")
            .raw("updatedAt", json!(at(2 * DAY + 25).to_rfc3339())),
    )]);
    let d = full_with(&t, &again, &no_pages(), 2 * DAY + 30);
    assert_eq!(issue(&d, &ids[0])["state"], "closed");
    assert_ne!(issue(&d, &ids[0])["closed_at"], closed_at.as_str());
    assert_eq!(issue(&d, &ids[0])["dismissed_at"], Value::Null);
    // Closed again by the very close that was dismissed: the dismissal holds again.
    let closed = gh_scenarios(vec![(
        ids[0].clone(),
        Sc::new()
            .closed_minutes_ago(60, "CLOSED")
            .raw("updatedAt", json!(at(3 * DAY + 35).to_rfc3339())),
    )]);
    let d = full_with(&t, &closed, &no_pages(), 3 * DAY + 40);
    assert!(issue(&d, &ids[0])["dismissed_at"].is_string(), "{}", issue(&d, &ids[0]));
}

#[test]
fn a_closed_issue_carries_its_close_reason_and_the_tracked_prs_that_close_it() {
    let ids = open_ids(STORE);
    let t = fixture_root();
    let g = gh_scenarios(vec![(
        ids[0].clone(),
        Sc::new()
            .closed_minutes_ago(60, "CLOSED")
            .raw("stateReason", json!("NOT_PLANNED")),
    )]);
    let d = doc(&run_with(&t, &g, &no_pages(), 0));
    assert_eq!(issue(&d, &ids[0])["state_reason"], "not_planned");
    for i in d["issues"].as_array().unwrap() {
        if i["kind"] == "issue" {
            assert!(i["closed_by"].is_array(), "{i}");
        } else {
            assert!(i.get("closed_by").is_none(), "{i}");
            assert_eq!(i["state_reason"], Value::Null, "{i}");
        }
    }
    // Every alias in closed_by is a tracked pull request of the outbox.
    let prs: Vec<&str> = d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["kind"] == "pull_request")
        .map(|i| i["alias"].as_str().unwrap())
        .collect();
    let named: Vec<&str> = d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|i| i["closed_by"].as_array().into_iter().flatten())
        .map(|a| a.as_str().unwrap())
        .collect();
    assert!(named.iter().all(|a| prs.contains(a)), "{named:?} vs {prs:?}");
}

#[test]
fn the_recently_closed_window_is_carried_into_the_meta_document() {
    let t = fixture_root();
    let d = doc(&run_with(&t, &gh(), &no_pages(), 0));
    assert_eq!(d["meta"]["recently_closed_days"], 7);
    let p = t.path().join("projects/home.toml");
    let text = std::fs::read_to_string(&p)
        .unwrap()
        .replace("history_days = 30\n", "history_days = 30\nrecently_closed_days = 3\n");
    std::fs::write(&p, text).unwrap();
    let d = doc(&run_with(&t, &gh(), &no_pages(), 1));
    assert_eq!(d["meta"]["recently_closed_days"], 3);
    // Longer than the history window: the window, since nothing closed before it is kept.
    let text = std::fs::read_to_string(&p).unwrap().replace(
        "history_days = 30
recently_closed_days = 3
",
        "history_days = 2
recently_closed_days = 7
",
    );
    std::fs::write(&p, text).unwrap();
    let d = doc(&run_with(&t, &gh(), &no_pages(), 2));
    assert_eq!(d["meta"]["recently_closed_days"], 2);
}
