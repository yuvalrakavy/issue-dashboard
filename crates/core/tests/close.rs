//! Closing an issue from the page (10.0's exception, 10.10 *Closing from the page*): the
//! decision → intent → apply path against the replay GitHub and a fake `gh` writer, the
//! already-closed skip, a crash between intent and apply, the refusals, and Undo as a reopen.

mod common;

use std::collections::HashMap;
use std::sync::Mutex;

use common::fixture::*;
use common::*;
use issue_dashboard_core::backup::Failpoint;
use issue_dashboard_core::cycle::{self, CycleReport};
use issue_dashboard_core::docs::{Action, CloseReason, CloseValue, Decision, DecisionValue, Target};
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::ingest::Pages;
use issue_dashboard_core::root::{Options, format_time};
use issue_dashboard_core::write::{CommentNow, IssueNow, IssueWriter};
use issue_dashboard_core::{Error, Result, ledger};
use serde_json::{Value, json};

/// A fake `gh` for the write path: one in-memory issue per (repo, number), every call logged.
struct FakeGh {
    issues: Mutex<HashMap<(String, u64), IssueNow>>,
    calls: Mutex<Vec<String>>,
    /// When set, `close` fails with this message and changes nothing.
    fail_close: Mutex<Option<String>>,
    /// The time GitHub stamps on what it does.
    now: Mutex<String>,
    /// How many reads after a close still show the issue as it was before it, as a lagging
    /// GitHub read would.
    stale_reads: Mutex<u32>,
    before_close: Mutex<HashMap<(String, u64), IssueNow>>,
    /// The pauses the write step asked for between reads.
    waits: Mutex<Vec<u64>>,
}

impl FakeGh {
    fn new() -> FakeGh {
        FakeGh {
            issues: Mutex::new(HashMap::new()),
            calls: Mutex::new(Vec::new()),
            fail_close: Mutex::new(None),
            now: Mutex::new(format_time(at(0))),
            stale_reads: Mutex::new(0),
            before_close: Mutex::new(HashMap::new()),
            waits: Mutex::new(Vec::new()),
        }
    }

    fn open(&self, alias: &str) {
        let (repo, n) = split(alias);
        self.issues.lock().unwrap().insert(
            (repo, n),
            IssueNow {
                state: "OPEN".into(),
                closed_at: None,
                comments: vec![],
            },
        );
    }

    fn get(&self, alias: &str) -> IssueNow {
        let (repo, n) = split(alias);
        self.issues.lock().unwrap()[&(repo, n)].clone()
    }

    fn set(&self, alias: &str, f: impl FnOnce(&mut IssueNow)) {
        let (repo, n) = split(alias);
        f(self.issues.lock().unwrap().get_mut(&(repo, n)).unwrap());
    }

    fn at(&self, minutes: i64) {
        *self.now.lock().unwrap() = format_time(at(minutes));
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn writes(&self) -> Vec<String> {
        self.calls().into_iter().filter(|c| !c.starts_with("view")).collect()
    }
}

fn split(alias: &str) -> (String, u64) {
    let (repo, n) = alias.split_once('#').unwrap();
    (repo.to_string(), n.parse().unwrap())
}

impl IssueWriter for FakeGh {
    fn issue_now(&self, repo: &str, number: u64) -> Result<IssueNow> {
        self.calls.lock().unwrap().push(format!("view {repo}#{number}"));
        let key = (repo.to_string(), number);
        let mut stale = self.stale_reads.lock().unwrap();
        if *stale > 0
            && let Some(before) = self.before_close.lock().unwrap().get(&key)
        {
            *stale -= 1;
            return Ok(before.clone());
        }
        self.issues
            .lock()
            .unwrap()
            .get(&key)
            .cloned()
            .ok_or_else(|| Error::GitHub(format!("no issue {repo}#{number}")))
    }

    fn wait(&self, d: std::time::Duration) {
        self.waits.lock().unwrap().push(d.as_millis() as u64);
    }

    fn close(&self, repo: &str, number: u64, reason: CloseReason, duplicate_of: Option<u64>, comment: Option<&str>) -> Result<()> {
        self.calls.lock().unwrap().push(format!(
            "close {repo}#{number} {reason:?} dup={duplicate_of:?} comment={comment:?}"
        ));
        if let Some(why) = self.fail_close.lock().unwrap().clone() {
            return Err(Error::GitHub(why));
        }
        let now = self.now.lock().unwrap().clone();
        let mut issues = self.issues.lock().unwrap();
        let i = issues.get_mut(&(repo.to_string(), number)).unwrap();
        self.before_close.lock().unwrap().insert((repo.to_string(), number), i.clone());
        if let Some(c) = comment {
            i.comments.push(CommentNow {
                author: "yuvalrakavy".into(),
                at: now.clone(),
                body: c.to_string(),
            });
        }
        i.state = "CLOSED".into();
        i.closed_at = Some(now);
        Ok(())
    }

    fn reopen(&self, repo: &str, number: u64) -> Result<()> {
        self.calls.lock().unwrap().push(format!("reopen {repo}#{number}"));
        let mut issues = self.issues.lock().unwrap();
        let i = issues.get_mut(&(repo.to_string(), number)).unwrap();
        i.state = "OPEN".into();
        i.closed_at = None;
        Ok(())
    }
}

fn pages(decisions: &[Decision]) -> Pages {
    Pages {
        decisions: decisions.iter().map(|d| serde_json::to_value(d).unwrap()).collect(),
        ..Pages::default()
    }
}

fn run(t: &TempRoot, w: &FakeGh, p: &Pages, minutes: i64) -> Result<CycleReport> {
    cycle::cycle_with_writes(&t.root, &gh(), Some(w), "home", p, &move || at(minutes), PAGE)
}

fn doc(r: &CycleReport) -> Value {
    serde_json::from_str(&std::fs::read_to_string(&r.outbox).unwrap()).unwrap()
}

fn ack<'a>(d: &'a Value, id: &str) -> &'a Value {
    d["acks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["decision"] == id)
        .unwrap_or_else(|| panic!("no ack for {id}: {}", d["acks"]))
}

fn issue<'a>(d: &'a Value, id: &str) -> &'a Value {
    d["issues"].as_array().unwrap().iter().find(|i| i["id"] == id).unwrap()
}

/// An open issue of `repo` the home project tracks, after one cycle: (id, alias).
fn tracked(t: &TempRoot, w: &FakeGh, repo: &str, kind: &str) -> (String, String) {
    let r = run(t, w, &Pages::default(), 0).unwrap();
    let d = doc(&r);
    let i = d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| {
            i["alias"].as_str().unwrap().starts_with(&format!("{repo}#")) && i["state"] == "open" && i["kind"] == kind
        })
        .unwrap_or_else(|| panic!("no open {kind} in {repo}"));
    let alias = i["alias"].as_str().unwrap().to_string();
    w.open(&alias);
    (i["id"].as_str().unwrap().to_string(), alias)
}

fn close(id: &str, item: &str, alias: &str, reason: CloseReason, duplicate_of: Option<u64>, comment: &str) -> Decision {
    decision(
        id,
        OWNER,
        Action::Close,
        Target {
            item: Some(item.into()),
            alias: Some(alias.into()),
            ..Default::default()
        },
        Some(DecisionValue {
            close: Some(CloseValue {
                reason,
                duplicate_of,
                comment: comment.into(),
            }),
            ..Default::default()
        }),
    )
}

fn write_events(t: &TempRoot, decision: &str) -> Vec<(Kind, Value)> {
    t.root
        .read(|l| {
            let mut out = Vec::new();
            for k in [Kind::WriteIntended, Kind::WriteApplied, Kind::WriteFailed] {
                for e in ledger::events_of(l.conn(), decision, k)? {
                    out.push((e.seq, k, e.data));
                }
            }
            out.sort_by_key(|(s, _, _)| *s);
            Ok(out.into_iter().map(|(_, k, d)| (k, d)).collect())
        })
        .unwrap()
}

fn kinds(t: &TempRoot, decision: &str) -> Vec<Kind> {
    write_events(t, decision).into_iter().map(|(k, _)| k).collect()
}

const NOT_PLANNED: &str = "Closed from the issue dashboard: no longer relevant.";

#[test]
fn a_close_decision_records_the_intent_closes_the_issue_and_records_the_result() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    let r = run(&t, &w, &pages(&[close("D_close", &x, &alias, CloseReason::NotPlanned, None, NOT_PLANNED)]), 10).unwrap();

    let (repo, n) = split(&alias);
    assert_eq!(
        w.calls(),
        vec![
            format!("view {repo}#{n}"),
            format!("close {repo}#{n} NotPlanned dup=None comment=Some({NOT_PLANNED:?})"),
            format!("view {repo}#{n}"),
        ]
    );
    assert_eq!(w.get(&alias).state, "CLOSED");
    let ev = write_events(&t, "D_close");
    assert_eq!(kinds(&t, "D_close"), vec![Kind::WriteIntended, Kind::WriteApplied]);
    assert_eq!(ev[0].1["op"], "close");
    assert_eq!(ev[0].1["repo"], repo);
    assert_eq!(ev[0].1["number"], n);
    assert_eq!(ev[0].1["comment"], NOT_PLANNED);
    assert_eq!(ev[1].1["result"], "closed");
    assert_eq!(ev[1].1["closed_at"], format_time(at(0)));

    let d = doc(&r);
    assert_eq!(ack(&d, "D_close")["result"], "applied");
    assert_eq!(issue(&d, &x)["dashboard_close"]["decision"], "D_close");
    assert_eq!(issue(&d, &x)["dashboard_close"]["reason"], "not_planned");

    // The next cycle does not write again.
    run(&t, &w, &Pages::default(), 20).unwrap();
    assert_eq!(w.writes().len(), 1, "{:?}", w.calls());
}

#[test]
fn a_duplicate_close_passes_the_other_issue_and_an_empty_comment_posts_none() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    run(&t, &w, &pages(&[close("D_dup", &x, &alias, CloseReason::Duplicate, Some(12), "")]), 10).unwrap();
    let (repo, n) = split(&alias);
    assert!(
        w.writes().contains(&format!("close {repo}#{n} Duplicate dup=Some(12) comment=None")),
        "{:?}",
        w.calls()
    );
}

#[test]
fn an_issue_already_closed_is_recorded_closed_and_gets_no_comment() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    w.set(&alias, |i| {
        i.state = "CLOSED".into();
        i.closed_at = Some(format_time(at(5)));
    });
    let r = run(&t, &w, &pages(&[close("D_close", &x, &alias, CloseReason::Completed, None, "done")]), 10).unwrap();
    assert!(w.writes().is_empty(), "nothing is written: {:?}", w.calls());
    assert!(w.get(&alias).comments.is_empty());
    let ev = write_events(&t, "D_close");
    assert_eq!(kinds(&t, "D_close"), vec![Kind::WriteIntended, Kind::WriteApplied]);
    assert_eq!(ev[1].1["result"], "already_closed");
    assert_eq!(ack(&doc(&r), "D_close")["result"], "applied");
}

#[test]
fn a_crash_between_intent_and_apply_is_reconciled_on_the_next_cycle() {
    let t = fixture_root_with(Options {
        failpoint: Some(Failpoint::AfterWriteIntent),
        ..Options::default()
    });
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    let p = pages(&[close("D_close", &x, &alias, CloseReason::NotPlanned, None, NOT_PLANNED)]);
    let crashed = run(&t, &w, &p, 10);
    assert!(matches!(crashed, Err(Error::Failpoint(_))), "{crashed:?}");
    assert_eq!(kinds(&t, "D_close"), vec![Kind::WriteIntended]);
    assert!(w.writes().is_empty());

    let healed = Root::new(t.path(), Options::default());
    let r = cycle::cycle_with_writes(&healed, &gh(), Some(&w), "home", &p, &|| at(20), PAGE).unwrap();
    assert_eq!(w.writes().len(), 1, "closed once: {:?}", w.calls());
    assert_eq!(w.get(&alias).comments.len(), 1);
    let ev = write_events(&t, "D_close");
    assert_eq!(kinds(&t, "D_close"), vec![Kind::WriteIntended, Kind::WriteApplied]);
    assert_eq!(ev[1].1["result"], "closed");
    assert_eq!(ev[1].1["reconciled"], true);
    assert_eq!(ack(&doc(&r), "D_close")["result"], "applied");
}

#[test]
fn a_crash_after_github_closed_is_reconciled_without_closing_again() {
    let t = fixture_root_with(Options {
        failpoint: Some(Failpoint::AfterWriteCall),
        ..Options::default()
    });
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    w.at(10);
    let p = pages(&[close("D_close", &x, &alias, CloseReason::NotPlanned, None, NOT_PLANNED)]);
    assert!(run(&t, &w, &p, 10).is_err());
    assert_eq!(w.writes().len(), 1);

    let healed = Root::new(t.path(), Options::default());
    cycle::cycle_with_writes(&healed, &gh(), Some(&w), "home", &p, &|| at(20), PAGE).unwrap();
    assert_eq!(w.writes().len(), 1, "no second close: {:?}", w.calls());
    assert_eq!(w.get(&alias).comments.len(), 1, "no second comment");
    let ev = write_events(&t, "D_close");
    assert_eq!(ev[1].1["result"], "closed");
    assert_eq!(ev[1].1["reconciled"], true);
}

#[test]
fn a_crash_after_the_comment_but_before_the_close_closes_without_a_second_comment() {
    let t = fixture_root_with(Options {
        failpoint: Some(Failpoint::AfterWriteIntent),
        ..Options::default()
    });
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    let p = pages(&[close("D_close", &x, &alias, CloseReason::NotPlanned, None, NOT_PLANNED)]);
    assert!(run(&t, &w, &p, 10).is_err());
    // gh posted the comment and died before the close.
    w.set(&alias, |i| {
        i.comments.push(CommentNow {
            author: "yuvalrakavy".into(),
            at: format_time(at(11)),
            body: NOT_PLANNED.into(),
        })
    });
    let healed = Root::new(t.path(), Options::default());
    cycle::cycle_with_writes(&healed, &gh(), Some(&w), "home", &p, &|| at(20), PAGE).unwrap();
    let (repo, n) = split(&alias);
    assert_eq!(w.writes(), vec![format!("close {repo}#{n} NotPlanned dup=None comment=None")]);
    assert_eq!(w.get(&alias).comments.len(), 1);
}

#[test]
fn a_close_on_a_read_only_repo_is_refused_without_an_intent() {
    let t = fixture_root();
    let w = FakeGh::new();
    let r = run(&t, &w, &Pages::default(), 0).unwrap();
    let i = doc(&r)["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["alias"].as_str().unwrap().starts_with(&format!("{RHAI}#")) && i["kind"] == "issue")
        .cloned()
        .expect("a rhai issue");
    let (x, alias) = (i["id"].as_str().unwrap(), i["alias"].as_str().unwrap());
    w.open(alias);
    let r = run(&t, &w, &pages(&[close("D_rhai", x, alias, CloseReason::Completed, None, "done")]), 10).unwrap();
    assert!(w.calls().is_empty(), "GitHub is not touched: {:?}", w.calls());
    let ev = write_events(&t, "D_rhai");
    assert_eq!(kinds(&t, "D_rhai"), vec![Kind::WriteFailed]);
    assert_eq!(ev[0].1["stage"], "precondition");
    assert!(ev[0].1["why"].as_str().unwrap().contains("read-only"), "{}", ev[0].1);
    let a = ack(&doc(&r), "D_rhai").clone();
    assert_eq!(a["result"], "refused");
    assert!(a["reason"].as_str().unwrap().contains("read-only"));
}

#[test]
fn a_close_of_a_pull_request_is_refused() {
    let t = fixture_root();
    let w = FakeGh::new();
    let r = run(&t, &w, &Pages::default(), 0).unwrap();
    let pr = doc(&r)["issues"]
        .as_array()
        .unwrap()
        .iter()
        .find(|i| i["kind"] == "pull_request")
        .cloned()
        .expect("a pull request");
    let (x, alias) = (pr["id"].as_str().unwrap(), pr["alias"].as_str().unwrap());
    w.open(alias);
    run(&t, &w, &pages(&[close("D_pr", x, alias, CloseReason::Completed, None, "")]), 10).unwrap();
    assert!(w.calls().is_empty());
    let ev = write_events(&t, "D_pr");
    assert_eq!(kinds(&t, "D_pr"), vec![Kind::WriteFailed]);
    assert!(ev[0].1["why"].as_str().unwrap().contains("not an issue"), "{}", ev[0].1);
}

#[test]
fn the_repo_and_number_come_from_the_observation_not_the_page() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    w.open("yuvalrakavy/airzoned#1");
    run(&t, &w, &pages(&[close("D_close", &x, "yuvalrakavy/airzoned#1", CloseReason::Completed, None, "")]), 10).unwrap();
    let (repo, n) = split(&alias);
    assert!(w.writes()[0].starts_with(&format!("close {repo}#{n} ")), "{:?}", w.calls());
    assert_eq!(w.get("yuvalrakavy/airzoned#1").state, "OPEN");
}

#[test]
fn a_close_by_anyone_but_the_owner_is_refused_and_never_written() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    let mut d = close("D_intruder", &x, &alias, CloseReason::Completed, None, "bye");
    d.author = "user_someone_else".into();
    let r = run(&t, &w, &pages(&[d]), 10).unwrap();
    assert!(w.calls().is_empty());
    assert!(kinds(&t, "D_intruder").is_empty());
    assert_eq!(ack(&doc(&r), "D_intruder")["result"], "refused");
}

#[test]
fn a_failed_close_is_recorded_refused_and_not_retried() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    *w.fail_close.lock().unwrap() = Some("HTTP 403: Resource not accessible by integration".into());
    let p = pages(&[close("D_close", &x, &alias, CloseReason::Completed, None, "")]);
    let r = run(&t, &w, &p, 10).unwrap();
    assert_eq!(kinds(&t, "D_close"), vec![Kind::WriteIntended, Kind::WriteFailed]);
    let a = ack(&doc(&r), "D_close").clone();
    assert_eq!(a["result"], "refused");
    assert!(a["reason"].as_str().unwrap().contains("403"), "{a}");
    *w.fail_close.lock().unwrap() = None;
    run(&t, &w, &p, 20).unwrap();
    assert_eq!(w.writes().len(), 1, "not retried: {:?}", w.calls());
}

#[test]
fn an_undo_before_the_close_was_applied_writes_nothing() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    let r = run(
        &t,
        &w,
        &pages(&[close("D_close", &x, &alias, CloseReason::Completed, None, ""), undo("D_undo", "D_close")]),
        10,
    )
    .unwrap();
    assert!(w.calls().is_empty(), "{:?}", w.calls());
    let d = doc(&r);
    assert_eq!(ack(&d, "D_close")["result"], "cancelled");
    assert_eq!(ack(&d, "D_undo")["result"], "applied");
}

#[test]
fn an_undo_after_the_close_reopens_the_issue() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    let c = close("D_close", &x, &alias, CloseReason::NotPlanned, None, NOT_PLANNED);
    run(&t, &w, &pages(&[c.clone()]), 10).unwrap();
    let r = run(&t, &w, &pages(&[c, undo("D_undo", "D_close")]), 20).unwrap();
    let (repo, n) = split(&alias);
    assert_eq!(w.writes().last().unwrap(), &format!("reopen {repo}#{n}"));
    assert_eq!(w.get(&alias).state, "OPEN");
    assert_eq!(
        kinds(&t, "D_close"),
        vec![Kind::WriteIntended, Kind::WriteApplied, Kind::WriteIntended, Kind::WriteApplied]
    );
    assert_eq!(write_events(&t, "D_close")[3].1["result"], "reopened");
    let d = doc(&r);
    assert_eq!(ack(&d, "D_undo")["result"], "applied");
    assert_eq!(ack(&d, "D_close")["result"], "cancelled");
    assert!(issue(&d, &x)["dashboard_close"].is_null());
}

#[test]
fn a_read_that_lags_the_close_is_read_again_so_undo_still_reopens() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    let c = close("D_close", &x, &alias, CloseReason::NotPlanned, None, NOT_PLANNED);
    *w.stale_reads.lock().unwrap() = 2;
    run(&t, &w, &pages(&[c.clone()]), 10).unwrap();
    assert_eq!(*w.waits.lock().unwrap(), vec![1000, 2000]);
    assert_eq!(write_events(&t, "D_close")[1].1["closed_at"], format_time(at(0)));
    // The reopen's own precondition read lags too.
    w.waits.lock().unwrap().clear();
    *w.stale_reads.lock().unwrap() = 1;
    let r = run(&t, &w, &pages(&[c, undo("D_undo", "D_close")]), 20).unwrap();
    assert_eq!(*w.waits.lock().unwrap(), vec![1000]);
    assert_eq!(w.get(&alias).state, "OPEN");
    assert_eq!(ack(&doc(&r), "D_undo")["result"], "applied");
}

#[test]
fn a_read_that_never_shows_the_close_leaves_its_time_unknown_after_the_last_pause() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    // Closed once before and reopened: GitHub keeps that earlier closedAt on the open issue.
    w.set(&alias, |i| i.closed_at = Some(format_time(at(-100))));
    let c = close("D_close", &x, &alias, CloseReason::Completed, None, "");
    *w.stale_reads.lock().unwrap() = 99;
    run(&t, &w, &pages(&[c]), 10).unwrap();
    assert_eq!(*w.waits.lock().unwrap(), vec![1000, 2000, 4000]);
    let applied = &write_events(&t, "D_close")[1].1;
    assert_eq!(applied["result"], "closed");
    assert!(applied.get("closed_at").is_none_or(Value::is_null), "{applied}");
}

#[test]
fn an_undo_is_refused_once_someone_commented_after_the_close() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    let c = close("D_close", &x, &alias, CloseReason::NotPlanned, None, NOT_PLANNED);
    run(&t, &w, &pages(&[c.clone()]), 10).unwrap();
    w.set(&alias, |i| {
        i.comments.push(CommentNow {
            author: "someone".into(),
            at: format_time(at(15)),
            body: "why?".into(),
        })
    });
    let r = run(&t, &w, &pages(&[c, undo("D_undo", "D_close")]), 20).unwrap();
    assert!(!w.writes().iter().any(|c| c.starts_with("reopen")), "{:?}", w.calls());
    let d = doc(&r);
    assert_eq!(ack(&d, "D_undo")["result"], "refused");
    assert!(ack(&d, "D_undo")["reason"].as_str().unwrap().contains("comment"));
    assert_eq!(ack(&d, "D_close")["result"], "applied", "the close stands");
}

#[test]
fn an_undo_of_a_close_someone_else_made_is_refused() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    w.set(&alias, |i| {
        i.state = "CLOSED".into();
        i.closed_at = Some(format_time(at(5)));
    });
    let c = close("D_close", &x, &alias, CloseReason::Completed, None, "");
    run(&t, &w, &pages(&[c.clone()]), 10).unwrap();
    let r = run(&t, &w, &pages(&[c, undo("D_undo", "D_close")]), 20).unwrap();
    assert!(w.writes().is_empty(), "{:?}", w.calls());
    assert_eq!(ack(&doc(&r), "D_undo")["result"], "refused");
}

#[test]
fn an_undo_is_refused_when_the_issue_was_reopened_on_github_since() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    let c = close("D_close", &x, &alias, CloseReason::Completed, None, "");
    run(&t, &w, &pages(&[c.clone()]), 10).unwrap();
    // GitHub keeps the last closedAt on a reopened issue.
    w.set(&alias, |i| i.state = "OPEN".into());
    let r = run(&t, &w, &pages(&[c, undo("D_undo", "D_close")]), 20).unwrap();
    assert!(!w.writes().iter().any(|c| c.starts_with("reopen")), "{:?}", w.calls());
    let d = doc(&r);
    let a = ack(&d, "D_undo");
    assert_eq!(a["result"], "refused");
    assert!(a["reason"].as_str().unwrap().contains("reopened on GitHub"), "{a}");
}

#[test]
fn an_undo_is_refused_when_the_issue_was_reopened_and_closed_again() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    let c = close("D_close", &x, &alias, CloseReason::Completed, None, "");
    run(&t, &w, &pages(&[c.clone()]), 10).unwrap();
    w.set(&alias, |i| i.closed_at = Some(format_time(at(15))));
    let r = run(&t, &w, &pages(&[c, undo("D_undo", "D_close")]), 20).unwrap();
    assert!(!w.writes().iter().any(|c| c.starts_with("reopen")));
    assert_eq!(ack(&doc(&r), "D_undo")["result"], "refused");
}

#[test]
fn a_duplicate_close_without_the_other_issue_is_refused_by_the_schema() {
    let mut d = serde_json::to_value(close("D", "I_x", "a/b#1", CloseReason::Duplicate, None, "")).unwrap();
    assert!(!schema_errors("page-decision", &d).is_empty(), "a duplicate needs duplicate_of");
    d["value"]["close"]["duplicate_of"] = json!(3);
    assert!(schema_errors("page-decision", &d).is_empty(), "{:?}", schema_errors("page-decision", &d));
    d["value"] = json!({});
    assert!(!schema_errors("page-decision", &d).is_empty(), "a close needs value.close");
}

fn schema_errors(stem: &str, v: &Value) -> Vec<String> {
    issue_dashboard_core::schema::errors(stem, v)
}

#[test]
fn meta_names_each_repos_visibility() {
    let t = fixture_root();
    let w = FakeGh::new();
    let r = run(&t, &w, &Pages::default(), 0).unwrap();
    let d = doc(&r);
    let store = d["meta"]["repos"].as_array().unwrap().iter().find(|m| m["repo"] == STORE).unwrap().clone();
    assert!(matches!(store["visibility"].as_str(), Some("public" | "private")), "{store}");
}

/// Sets the node `id` closed at `closed_at`, updated at `updated`, with these comments
/// (author, at), wherever a response holds it.
fn closed_on_github(v: &mut Value, id: &str, closed_at: &str, updated: &str, comments: &[(&str, String)]) {
    match v {
        Value::Object(o) => {
            if o.get("id").and_then(Value::as_str) == Some(id) && o.contains_key("updatedAt") {
                o.insert("state".into(), json!("CLOSED"));
                o.insert("stateReason".into(), json!("NOT_PLANNED"));
                o.insert("closedAt".into(), json!(closed_at));
                o.insert("updatedAt".into(), json!(updated));
                o.insert(
                    "comments".into(),
                    json!({"totalCount": comments.len(), "nodes": comments.iter().map(|(a, at)| json!({
                        "author": {"login": a}, "authorAssociation": "NONE", "createdAt": at
                    })).collect::<Vec<_>>()}),
                );
            }
            for c in o.values_mut() {
                closed_on_github(c, id, closed_at, updated, comments);
            }
        }
        Value::Array(a) => a.iter_mut().for_each(|c| closed_on_github(c, id, closed_at, updated, comments)),
        _ => {}
    }
}

#[test]
fn undo_is_offered_while_the_observation_shows_the_dashboards_close_and_no_comment_since() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    let c = close("D_close", &x, &alias, CloseReason::NotPlanned, None, NOT_PLANNED);
    let sw = Switch::new();
    let closed_at = format_time(at(0));
    let cycle = |minutes: i64, p: &Pages| {
        cycle::cycle_with_writes(&t.root, &sw.gh(), Some(&w), "home", p, &move || at(minutes), PAGE).unwrap()
    };
    cycle(10, &pages(&[c.clone()]));
    let (id, at0) = (x.clone(), closed_at.clone());
    sw.set(move |_, v| {
        let mut v = v?;
        closed_on_github(&mut v, &id, &at0, &format_time(at(24 * 60)), &[("yuvalrakavy", format_time(at(0)))]);
        Ok(v)
    });
    // A day later, so the daily full read reads it by node id.
    let d = doc(&cycle(24 * 60 + 20, &pages(&[c.clone()])));
    let dc = &issue(&d, &x)["dashboard_close"];
    assert_eq!(issue(&d, &x)["state"], "closed", "the read shows the close");
    assert_eq!(dc["result"], "closed");
    assert_eq!(dc["undo_allowed"], true, "{dc}");

    let (id, at0) = (x.clone(), closed_at.clone());
    sw.set(move |_, v| {
        let mut v = v?;
        closed_on_github(&mut v, &id, &at0, &format_time(at(48 * 60)), &[("yuvalrakavy", format_time(at(0))), ("someone", format_time(at(15)))]);
        Ok(v)
    });
    let d = doc(&cycle(48 * 60 + 30, &pages(&[c])));
    assert_eq!(issue(&d, &x)["dashboard_close"]["undo_allowed"], false, "a comment since the close");
}

#[test]
fn a_crash_before_the_fresh_read_settles_an_earlier_close_as_someone_elses() {
    let t = fixture_root_with(Options {
        failpoint: Some(Failpoint::AfterWriteIntent),
        ..Options::default()
    });
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    let p = pages(&[close("D_close", &x, &alias, CloseReason::Completed, None, "")]);
    assert!(run(&t, &w, &p, 10).is_err());
    // Someone had closed it before the intent was recorded.
    w.set(&alias, |i| {
        i.state = "CLOSED".into();
        i.closed_at = Some(format_time(at(5)));
    });
    let healed = Root::new(t.path(), Options::default());
    cycle::cycle_with_writes(&healed, &gh(), Some(&w), "home", &p, &|| at(20), PAGE).unwrap();
    assert!(w.writes().is_empty());
    assert_eq!(write_events(&t, "D_close")[1].1["result"], "already_closed");
}

/// The cycle without a writer (the `snapshot`-style path) never applies a close.
#[test]
fn without_a_writer_a_close_stays_pending() {
    let t = fixture_root();
    let w = FakeGh::new();
    let (x, alias) = tracked(&t, &w, STORE, "issue");
    let p = pages(&[close("D_close", &x, &alias, CloseReason::Completed, None, "")]);
    let r = cycle::cycle(&t.root, &gh(), "home", &p, &|| at(10), PAGE).unwrap();
    assert!(kinds(&t, "D_close").is_empty());
    assert!(doc(&r)["acks"].as_array().unwrap().iter().all(|a| a["decision"] != "D_close"));
}

use issue_dashboard_core::Root;
