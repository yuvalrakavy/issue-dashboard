//! Triage phase 4/5, launch (triage spec 5): the daily run's `triage-request`, the dispatcher's
//! author check, the `recommend_closures` alias, the `eval` kind, the routing defaults, and the
//! 2.2 selection the daily run names its items by. Every assertion is against a literal.

mod common;

use std::fs;

use common::fixture::*;
use common::triage::*;
use common::{OWNER, TempRoot, launch_doc};
use issue_dashboard_core::config::{ConfigSet, READ_ONLY_RECIPE, default_routing};
use issue_dashboard_core::daily::{self, TriageRequested};
use issue_dashboard_core::docs::{LaunchKind, LaunchRequest, TriageScope};
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::ingest::{self, Pages};
use issue_dashboard_core::launch::{self, State, Via};
use issue_dashboard_core::root::format_time;
use issue_dashboard_core::triage::{self, Stage};
use issue_dashboard_core::{Result, brief, cycle, ledger};
use serde_json::{Value, json};

fn config(t: &TempRoot) -> ConfigSet {
    ConfigSet::load(t.path()).unwrap()
}

fn request(t: &TempRoot, minutes: i64) -> Result<TriageRequested> {
    let c = config(t);
    t.root.mutate(at(minutes), |tx| daily::request(tx, &c, "home"))
}

fn requested_id(r: &TriageRequested) -> String {
    match r {
        TriageRequested::Requested { launch, .. } => launch.clone(),
        other => panic!("expected a request, got {other:?}"),
    }
}

fn load(t: &TempRoot, id: &str) -> launch::Launch {
    t.root.read(|l| launch::load(l.conn(), id)).unwrap().unwrap()
}

/// Appends `text` to the fixture's home project config.
fn configure(t: &TempRoot, text: &str) {
    let p = t.path().join("projects/home.toml");
    let mut s = fs::read_to_string(&p).unwrap();
    s.push_str(text);
    fs::write(&p, s).unwrap();
}

/// Sets a top-level key of the fixture's home project config (before its first table).
fn configure_top(t: &TempRoot, line: &str) {
    let p = t.path().join("projects/home.toml");
    let s = fs::read_to_string(&p)
        .unwrap()
        .replacen("[[member]]", &format!("{line}\n\n[[member]]"), 1);
    fs::write(&p, s).unwrap();
}

fn select(t: &TempRoot, scope: TriageScope, minutes: i64) -> Vec<triage::Candidate> {
    let c = config(t);
    let now = format_time(at(minutes));
    t.root
        .read(|l| triage::select(l.conn(), &c, "home", scope, &now))
        .unwrap()
}

// --- 6.1 "daily launch" ---------------------------------------------------------------------

#[test]
fn the_daily_run_records_a_schedule_triage_launch_at_the_default_routing() {
    let tr = tracked();
    let r = request(&tr.t, 5).unwrap();
    let id = requested_id(&r);
    let l = load(&tr.t, &id);
    assert_eq!(l.state, State::Requested);
    assert_eq!(l.request.author, "schedule");
    assert_eq!(l.request.kind, LaunchKind::Triage);
    assert_eq!(l.request.scope, Some(TriageScope::Full));
    assert_eq!(l.request.recipe, "read-only");
    assert_eq!(
        (l.request.routing.model.as_str(), l.request.routing.effort.as_str()),
        ("claude-opus-5-5", "medium")
    );
    assert_eq!(
        l.request.routing.reason.as_deref(),
        Some("the tracker's default routing (triage spec 5.3)")
    );
    // The items are 2.2's selection, in its order, capped at the default 40.
    let picked: Vec<String> = select(&tr.t, TriageScope::Full, 5)
        .into_iter()
        .map(|c| c.item)
        .collect();
    assert!(picked.len() > 40, "the fixture has more than 40 due: {}", picked.len());
    assert_eq!(l.request.items, picked[..40].to_vec());
    assert_eq!(l.request.items.len(), 40);
    // The requested transition says who asked and the scope.
    let e =
        tr.t.root
            .read(|c| ledger::latest(c.conn(), &id, Kind::Launch))
            .unwrap()
            .unwrap();
    assert_eq!(
        (e.data["via"].clone(), e.data["scope"].clone()),
        (json!("schedule"), json!("full"))
    );
    assert!(id.starts_with("L_20"), "{id}");
    let v = serde_json::to_value(&r).unwrap();
    assert_eq!(
        (v["result"].clone(), v["repeat"].clone()),
        (json!("requested"), json!(false))
    );
}

#[test]
fn the_daily_run_is_refused_while_a_triage_launch_is_live_and_requests_again_once_it_ends() {
    let tr = tracked();
    let first = request(&tr.t, 5).unwrap();
    let id = requested_id(&first);
    // Its own request, still waiting for a dispatcher: pending, nothing new recorded.
    let pending = serde_json::to_value(request(&tr.t, 6).unwrap()).unwrap();
    assert_eq!(
        (
            pending["result"].clone(),
            pending["launch"].clone(),
            pending["project"].clone()
        ),
        (json!("pending"), json!(id), json!("home"))
    );
    assert_eq!(pending["aliases"], serde_json::to_value(&first).unwrap()["aliases"]);
    assert_eq!(pending["routing"]["model"], "claude-opus-5-5");
    assert_eq!(tr.t.root.read(|l| launch::all(l.conn())).unwrap().len(), 1);
    let token =
        tr.t.root
            .mutate(at(7), |tx| match launch::reserve(tx, &id, D1, &cap)? {
                launch::Reservation::Reserved { token, .. } => Ok(token),
                other => panic!("expected a reservation, got {other:?}"),
            })
            .unwrap();
    assert_eq!(
        refused(request(&tr.t, 8)),
        format!("triage launch {id} of project home is live (reserved); the daily run requests nothing until it ends")
    );
    tr.t.root
        .mutate(at(9), |tx| {
            launch::started(tx, &token, "local_triage")?;
            launch::ended(tx, &token, launch::EndState::Stopped)
        })
        .unwrap();
    let again = requested_id(&request(&tr.t, 10).unwrap());
    assert_ne!(again, id);
}

#[test]
fn an_owner_recommend_closures_launch_also_blocks_the_daily_run() {
    let tr = tracked();
    let doc = launch_of("R1", LaunchKind::RecommendClosures, &tr.items[..1]);
    tr.t.root
        .mutate(at(2), |tx| launch::request(tx, &doc, Via::Ingest))
        .unwrap();
    assert!(refused(request(&tr.t, 3)).starts_with("triage launch R1 of project home is live (requested)"));
}

#[test]
fn a_live_fix_launch_does_not_block_the_daily_run() {
    let tr = tracked();
    let mut doc = launch_doc("F1", "home", &[&tr.items[0].0], "lanes");
    doc.aliases = vec![tr.items[0].1.clone()];
    tr.t.root
        .mutate(at(2), |tx| launch::request(tx, &doc, Via::Ingest))
        .unwrap();
    requested_id(&request(&tr.t, 3).unwrap());
}

#[test]
fn with_nothing_eligible_the_daily_run_records_nothing() {
    let tr = tracked();
    // Every member read-only: nothing this project may label.
    let p = tr.t.path().join("projects/home.toml");
    let s = fs::read_to_string(&p)
        .unwrap()
        .replace(
            "repo = \"yuvalrakavy/Store\"",
            "repo = \"yuvalrakavy/Store\"\nread_only = true",
        )
        .replace(
            "repo = \"yuvalrakavy/airzoned\"",
            "repo = \"yuvalrakavy/airzoned\"\nread_only = true",
        );
    fs::write(&p, s).unwrap();
    let r = request(&tr.t, 5).unwrap();
    assert_eq!(
        serde_json::to_value(&r).unwrap(),
        json!({"result": "nothing_to_triage", "project": "home"})
    );
    let all = tr.t.root.read(|l| launch::all(l.conn())).unwrap();
    assert!(all.is_empty(), "{all:?}");
}

#[test]
fn the_daily_run_names_at_most_max_triage_items_the_first_by_2_2() {
    let tr = tracked();
    configure_top(&tr.t, "max_triage_items = 2");
    let id = requested_id(&request(&tr.t, 5).unwrap());
    let picked: Vec<String> = select(&tr.t, TriageScope::Full, 5)
        .into_iter()
        .map(|c| c.item)
        .collect();
    assert_eq!(load(&tr.t, &id).request.items, picked[..2].to_vec());
}

#[test]
fn a_configured_triage_routing_replaces_the_default() {
    let tr = tracked();
    configure(
        &tr.t,
        "\n[routing.triage]\nmodel = \"claude-sonnet-5-5\"\neffort = \"high\"\n",
    );
    let r = request(&tr.t, 5).unwrap();
    let l = load(&tr.t, &requested_id(&r));
    assert_eq!(
        (l.request.routing.model.as_str(), l.request.routing.effort.as_str()),
        ("claude-sonnet-5-5", "high")
    );
}

#[test]
fn the_routing_defaults_are_opus_medium_for_triage_and_fable_high_for_a_stronger_look() {
    let t = default_routing("triage").unwrap();
    assert_eq!((t.model.as_str(), t.effort.as_str()), ("claude-opus-5-5", "medium"));
    let s = default_routing("stronger_look").unwrap();
    assert_eq!((s.model.as_str(), s.effort.as_str()), ("claude-fable-5-1", "high"));
    assert!(default_routing("eval").is_none());
    assert!(default_routing("fix").is_none());
    let tr = tracked();
    let m = refused(config(&tr.t).routing("home", "eval"));
    assert_eq!(
        m,
        "project home has no routing for eval launches; add one under [routing.eval]"
    );
}

// --- the dispatcher's author check (5.2, 4.6) ---------------------------------------------

fn doc_by(author: &str, kind: LaunchKind) -> LaunchRequest {
    let mut d = launch_doc("L9", "home", &["I_1"], READ_ONLY_RECIPE);
    d.author = author.to_string();
    d.kind = kind;
    d
}

#[test]
fn the_dispatcher_takes_the_owner_s_launches_and_the_schedule_s_triage_only() {
    let tr = tracked();
    let c = config(&tr.t);
    launch::check_author(&c, &doc_by(OWNER, LaunchKind::Fix)).unwrap();
    launch::check_author(&c, &doc_by(OWNER, LaunchKind::Triage)).unwrap();
    launch::check_author(&c, &doc_by("schedule", LaunchKind::Triage)).unwrap();
    assert_eq!(
        refused(launch::check_author(&c, &doc_by("schedule", LaunchKind::Fix))),
        "launch L9: author schedule requests triage launches only, not fix"
    );
    assert_eq!(
        refused(launch::check_author(
            &c,
            &doc_by("schedule", LaunchKind::RecommendClosures)
        )),
        "launch L9: author schedule requests triage launches only, not recommend_closures"
    );
    assert_eq!(
        refused(launch::check_author(
            &c,
            &doc_by("user_someone_else", LaunchKind::Triage)
        )),
        "launch L9: author user_someone_else is not project home's owner_id"
    );
}

#[test]
fn the_schedule_s_request_is_reserved_like_the_owner_s() {
    let tr = tracked();
    let id = requested_id(&request(&tr.t, 5).unwrap());
    let r =
        tr.t.root
            .mutate(at(6), |tx| launch::reserve(tx, &id, D1, &cap))
            .unwrap();
    assert!(matches!(r, launch::Reservation::Reserved { .. }), "{r:?}");
    assert_eq!(load(&tr.t, &id).state, State::Reserved);
}

// --- 6.1 "alias" --------------------------------------------------------------------------

#[test]
fn a_phase_1_recommend_closures_document_parses_as_triage_scope_closures() {
    let phase1: Value = json!({
        "id": "L_20261023_080000_ab12", "project": "home", "author": "user_owner",
        "created_at": "2026-10-23T08:20:00Z", "kind": "recommend_closures",
        "items": ["I_1"], "aliases": ["example-org/server#403"], "recipe": "read-only",
        "routing": {"model": "claude-opus-5-5", "effort": "medium"},
    });
    let d: LaunchRequest = serde_json::from_value(phase1.clone()).unwrap();
    assert_eq!(d.scope, None);
    assert_eq!(d.triage_scope(), Some(TriageScope::Closures));
    // It serialises back unchanged: no scope appears.
    assert_eq!(serde_json::to_value(&d).unwrap(), phase1);
    let mut t = d.clone();
    t.kind = LaunchKind::Triage;
    assert_eq!(t.triage_scope(), Some(TriageScope::Full));
    t.scope = Some(TriageScope::Closures);
    assert_eq!(t.triage_scope(), Some(TriageScope::Closures));
    let mut f = d;
    f.kind = LaunchKind::Fix;
    assert_eq!(f.triage_scope(), None);
}

#[test]
fn a_triage_launch_of_scope_closures_asks_what_the_alias_asks() {
    let tr = tracked();
    let mut doc = launch_of("T1", LaunchKind::Triage, &tr.items[..1]);
    doc.scope = Some(TriageScope::Closures);
    tr.t.root
        .mutate(at(2), |tx| {
            launch::request(tx, &doc, Via::Reserve)?;
            launch::reserve(tx, "T1", D1, &cap)
        })
        .unwrap();
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    // An owner-written issue: the three closure decisions, as the alias asks (triage_seam).
    assert_eq!(
        asked_about(&p, &tr.items[0].1),
        ["duplicate", "likely_fixed", "obsolete"]
    );
    let m = refused(prepare(&tr.t, "T1", BACKEND, Stage::Priority, 3));
    assert!(
        m.contains("launch T1 asks closures only; it has no priority stage"),
        "{m}"
    );
}

// --- the kinds (5.1, 4.6) -----------------------------------------------------------------

#[test]
fn triage_eval_and_their_kin_lock_no_items_and_the_read_only_kinds_are_those_three() {
    for (kind, holds, read_only) in [
        (LaunchKind::Fix, true, false),
        (LaunchKind::Batch, true, false),
        (LaunchKind::Triage, false, true),
        (LaunchKind::RecommendClosures, false, true),
        (LaunchKind::Eval, false, true),
        (LaunchKind::StrongerLook, false, false),
    ] {
        assert_eq!((kind.holds_items(), kind.read_only()), (holds, read_only), "{kind:?}");
    }
}

#[test]
fn a_scope_on_any_kind_but_triage_is_refused() {
    let tr = tracked();
    let mut doc = launch_of("R2", LaunchKind::RecommendClosures, &tr.items[..1]);
    doc.scope = Some(TriageScope::Closures);
    assert_eq!(
        refused(tr.t.root.mutate(at(2), |tx| launch::request(tx, &doc, Via::Ingest))),
        "launch R2: a recommend_closures launch takes no scope; only triage does"
    );
}

#[test]
fn a_read_only_kind_outside_the_read_only_pool_is_refused() {
    let tr = tracked();
    for (kind, name) in [(LaunchKind::Triage, "triage"), (LaunchKind::Eval, "eval")] {
        let mut doc = launch_of("X1", kind, &tr.items[..1]);
        doc.recipe = "lanes".to_string();
        assert_eq!(
            refused(tr.t.root.mutate(at(2), |tx| launch::request(tx, &doc, Via::Ingest))),
            format!("launch X1: a {name} launch runs in the read-only pool, not lanes")
        );
    }
}

#[test]
fn an_eval_names_no_items_and_a_triage_must() {
    let tr = tracked();
    let eval = launch_of("E1", LaunchKind::Eval, &[]);
    tr.t.root
        .mutate(at(2), |tx| launch::request(tx, &eval, Via::Ingest))
        .unwrap();
    assert_eq!(load(&tr.t, "E1").state, State::Requested);
    let triage = launch_of("T2", LaunchKind::Triage, &[]);
    assert_eq!(
        refused(tr.t.root.mutate(at(2), |tx| launch::request(tx, &triage, Via::Ingest))),
        "launch T2 names no items"
    );
}

#[test]
fn two_triage_launches_over_the_same_item_both_reserve_one_at_a_time() {
    let tr = tracked();
    for id in ["T1", "T2"] {
        let doc = launch_of(id, LaunchKind::Triage, &tr.items[..1]);
        tr.t.root
            .mutate(at(2), |tx| launch::request(tx, &doc, Via::Ingest))
            .unwrap();
    }
    let r1 =
        tr.t.root
            .mutate(at(3), |tx| launch::reserve(tx, "T1", D1, &cap))
            .unwrap();
    assert!(matches!(r1, launch::Reservation::Reserved { .. }), "{r1:?}");
    // The read-only pool's one slot is taken: queued, not refused for a held item.
    let r2 = serde_json::to_value(
        tr.t.root
            .mutate(at(3), |tx| launch::reserve(tx, "T2", D1, &cap))
            .unwrap(),
    )
    .unwrap();
    assert_eq!(r2["result"], "queued", "{r2}");
}

// --- 2.2 selection ------------------------------------------------------------------------

#[test]
fn untriaged_items_come_before_labelled_ones() {
    let tr = tracked();
    let c = select(&tr.t, TriageScope::Full, 5);
    let labelled: Vec<&str> = tr.items.iter().map(|(i, _)| i.as_str()).collect();
    let first_labelled = c.iter().position(|x| labelled.contains(&x.item.as_str())).unwrap();
    assert!(first_labelled > 0, "an untriaged item comes first");
    assert!(c[..first_labelled].iter().all(|x| x.untriaged), "{c:?}");
    assert!(c[first_labelled..].iter().all(|x| !x.untriaged), "{c:?}");
    assert_eq!(c.iter().filter(|x| labelled.contains(&x.item.as_str())).count(), 3);
}

/// Answers every question of a full triage of `items` (both stages) as launch `id`.
fn answered(tr: &Tracked, id: &str, items: &[(String, String)], minutes: i64) {
    let doc = launch_of(id, LaunchKind::Triage, items);
    tr.t.root
        .mutate(at(minutes), |tx| {
            launch::request(tx, &doc, Via::Reserve)?;
            launch::reserve(tx, id, D1, &cap)
        })
        .unwrap();
    let p = prepare(&tr.t, id, BACKEND, Stage::First, minutes).unwrap();
    record(&tr.t, id, answer_all(&p), minutes).unwrap();
    let p = prepare(&tr.t, id, BACKEND, Stage::Priority, minutes).unwrap();
    record(&tr.t, id, answer_all(&p), minutes).unwrap();
}

#[test]
fn an_answered_item_leaves_the_selection_until_the_owner_asks_again() {
    let tr = tracked();
    let item = tr.items[0].0.clone();
    answered(&tr, "T1", &tr.items[..1], 2);
    let ids = |m| -> Vec<String> {
        select(&tr.t, TriageScope::Full, m)
            .into_iter()
            .map(|c| c.item)
            .collect()
    };
    assert!(!ids(4).contains(&item), "answered under the current hash: not due");
    // The owner's Triage… on the item, after its answers.
    let c = config(&tr.t);
    let pages = Pages {
        requests: vec![json!({
            "id": "R_20261029_080000_aaaaaaaa", "project": "home", "author": OWNER,
            "created_at": "2026-10-29T08:00:00Z", "kind": "triage", "items": [item],
        })],
        ..Pages::default()
    };
    tr.t.root
        .mutate(at(5), |tx| ingest::ingest(tx, &c, "home", OWNER, &pages))
        .unwrap();
    let after = ids(6);
    assert!(after.contains(&item), "the owner asked: due again");
    // And never-triaged items still come before it: it was triaged once.
    let pos = after.iter().position(|x| x == &item).unwrap();
    let cands = select(&tr.t, TriageScope::Full, 6);
    assert!(cands[pos].last_triaged.is_some());
    assert!(
        cands[..pos].iter().all(|x| x.untriaged || x.last_triaged.is_none()),
        "{cands:?}"
    );
}

#[test]
fn an_owner_request_before_the_last_answer_does_not_make_the_item_due() {
    let tr = tracked();
    let item = tr.items[0].0.clone();
    let c = config(&tr.t);
    let pages = Pages {
        requests: vec![json!({
            "id": "R_20261029_070000_bbbbbbbb", "project": "home", "author": OWNER,
            "created_at": "2026-10-29T07:00:00Z", "kind": "triage", "items": [item],
        })],
        ..Pages::default()
    };
    tr.t.root
        .mutate(at(1), |tx| ingest::ingest(tx, &c, "home", OWNER, &pages))
        .unwrap();
    answered(&tr, "T1", &tr.items[..1], 2);
    let ids: Vec<String> = select(&tr.t, TriageScope::Full, 4)
        .into_iter()
        .map(|c| c.item)
        .collect();
    assert!(!ids.contains(&item));
}

// --- gate round 1: what a live launch is, and the selection's filters ------------------------

#[test]
fn an_owner_triage_request_no_dispatcher_took_for_a_day_stops_blocking_the_daily_run() {
    let tr = tracked();
    // The owner's request, written at 2026-10-23T08:00:00Z (launch_doc), never reserved: its
    // send was refused, and the not_sent record is not ingested.
    let doc = launch_of("R1", LaunchKind::Triage, &tr.items[..1]);
    tr.t.root
        .mutate(at(2), |tx| launch::request(tx, &doc, Via::Ingest))
        .unwrap();
    // at(1282) is 2026-10-24T07:59:46Z, less than a day after: still live.
    assert_eq!(
        refused(request(&tr.t, 1282)),
        "triage launch R1 of project home is live (requested); the daily run requests nothing until it ends"
    );
    // at(1283) is 2026-10-24T08:00:46Z: unheard for a day, it no longer counts.
    let id = requested_id(&request(&tr.t, 1283).unwrap());
    assert_ne!(id, "R1");
    assert_eq!(load(&tr.t, "R1").state, State::Requested);
}

#[test]
fn a_schedule_request_does_not_age_the_next_daily_run_wakes_for_it_again() {
    let tr = tracked();
    let id = requested_id(&request(&tr.t, 5).unwrap());
    let v = serde_json::to_value(request(&tr.t, 5 + 3 * 24 * 60).unwrap()).unwrap();
    assert_eq!(
        (v["result"].clone(), v["launch"].clone()),
        (json!("pending"), json!(id))
    );
    assert_eq!(tr.t.root.read(|l| launch::all(l.conn())).unwrap().len(), 1);
}

#[test]
fn a_triage_launch_of_another_project_does_not_block_this_one() {
    let tr = tracked();
    let mut doc = launch_doc("K1", "kernel", &[], READ_ONLY_RECIPE);
    doc.kind = LaunchKind::Triage;
    doc.items = vec![tr.items[0].0.clone()];
    doc.aliases = vec![tr.items[0].1.clone()];
    tr.t.root
        .mutate(at(2), |tx| launch::request(tx, &doc, Via::Ingest))
        .unwrap();
    assert_eq!(load(&tr.t, "K1").state, State::Requested);
    let id = requested_id(&request(&tr.t, 3).unwrap());
    assert_ne!(id, "K1");
}

#[test]
fn a_recommend_closures_request_records_no_scope_and_still_reads_as_closures() {
    let tr = tracked();
    let doc = launch_of("R1", LaunchKind::RecommendClosures, &tr.items[..1]);
    tr.t.root
        .mutate(at(2), |tx| launch::request(tx, &doc, Via::Ingest))
        .unwrap();
    let e =
        tr.t.root
            .read(|c| ledger::latest(c.conn(), "R1", Kind::Launch))
            .unwrap()
            .unwrap();
    assert_eq!(e.data.get("scope"), None, "{}", e.data);
    assert_eq!(load(&tr.t, "R1").request.triage_scope(), Some(TriageScope::Closures));
}

#[test]
fn an_owner_request_in_another_project_does_not_make_the_item_due_here() {
    let tr = tracked();
    let item = tr.items[0].0.clone();
    answered(&tr, "T1", &tr.items[..1], 2);
    let c = config(&tr.t);
    let pages = Pages {
        requests: vec![json!({
            "id": "R_20261029_080000_cccccccc", "project": "kernel", "author": OWNER,
            "created_at": "2026-10-29T08:00:00Z", "kind": "triage", "items": [item],
        })],
        ..Pages::default()
    };
    tr.t.root
        .mutate(at(5), |tx| ingest::ingest(tx, &c, "kernel", OWNER, &pages))
        .unwrap();
    let n =
        tr.t.root
            .read(|l| ledger::events_by_kind(l.conn(), Kind::Request))
            .unwrap()
            .len();
    assert_eq!(n, 1, "the kernel request was recorded");
    let ids: Vec<String> = select(&tr.t, TriageScope::Full, 6)
        .into_iter()
        .map(|c| c.item)
        .collect();
    assert!(!ids.contains(&item), "{ids:?}");
}

#[test]
fn the_closures_selection_asks_closures_only_and_the_full_one_labels_too() {
    let tr = tracked();
    let item = tr.items[0].0.clone();
    let mut doc = launch_of("T1", LaunchKind::Triage, &tr.items[..1]);
    doc.scope = Some(TriageScope::Closures);
    tr.t.root
        .mutate(at(2), |tx| {
            launch::request(tx, &doc, Via::Reserve)?;
            launch::reserve(tx, "T1", D1, &cap)
        })
        .unwrap();
    let p = prepare(&tr.t, "T1", BACKEND, Stage::First, 3).unwrap();
    record(&tr.t, "T1", answer_all(&p), 3).unwrap();
    let ids = |scope| -> Vec<String> { select(&tr.t, scope, 4).into_iter().map(|c| c.item).collect() };
    assert!(!ids(TriageScope::Closures).contains(&item), "its closures are answered");
    assert!(ids(TriageScope::Full).contains(&item), "its labels are not");
}

#[test]
fn a_closed_issue_is_never_selected() {
    let tr = tracked();
    let r = cycle::cycle(&tr.t.root, &gh(), "home", &no_pages(), &move || at(1), PAGE).unwrap();
    let d: Value = serde_json::from_str(&fs::read_to_string(&r.outbox).unwrap()).unwrap();
    let closed: Vec<String> = d["issues"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|i| i["state"] != "open" && i["kind"] == "issue")
        .map(|i| i["id"].as_str().unwrap().to_string())
        .collect();
    assert!(!closed.is_empty(), "the fixture holds closed issues");
    let picked = select(&tr.t, TriageScope::Full, 2);
    assert!(!picked.is_empty());
    let wrong: Vec<&String> = picked.iter().map(|c| &c.item).filter(|i| closed.contains(i)).collect();
    assert!(wrong.is_empty(), "closed issues selected: {wrong:?}");
}

#[test]
fn a_repo_the_account_cannot_write_to_is_not_selected() {
    for (permission, store_selected) in [("READ", false), ("WRITE", true)] {
        let t = fixture_root();
        let gh = gh_with(move |req, v| {
            let mut v = v?;
            if req.name == "repo" && req.variables["name"] == "Store" {
                v["data"]["repository"]["viewerPermission"] = json!(permission);
            }
            Ok(v)
        });
        cycle::cycle(&t.root, &gh, "home", &no_pages(), &move || at(0), PAGE).unwrap();
        let picked = select(&t, TriageScope::Full, 1);
        let store = picked.iter().any(|c| c.alias.starts_with("yuvalrakavy/Store#"));
        let airzoned = picked.iter().any(|c| c.alias.starts_with("yuvalrakavy/airzoned#"));
        assert_eq!((store, airzoned), (store_selected, true), "{permission}");
    }
}

#[test]
fn a_closures_brief_says_closures_and_has_no_priority_stage() {
    let tr = tracked();
    let items = &tr.items[..2];
    for (id, scope) in [("T1", TriageScope::Closures), ("T2", TriageScope::Full)] {
        let mut doc = launch_of(id, LaunchKind::Triage, items);
        doc.scope = Some(scope);
        let token =
            tr.t.root
                .mutate(at(2), |tx| {
                    launch::request(tx, &doc, Via::Reserve)?;
                    match launch::reserve(tx, id, D1, &cap)? {
                        launch::Reservation::Reserved { token, .. } => {
                            launch::started(tx, &token, &format!("local_{id}"))?;
                            launch::ended(tx, &token, launch::EndState::Landed)?;
                            Ok(token)
                        }
                        other => panic!("expected a reservation, got {other:?}"),
                    }
                })
                .unwrap();
        let c = config(&tr.t);
        let b =
            tr.t.root
                .read(|l| brief::brief_with(l.conn(), &c, id, "issue-dashboard"))
                .unwrap();
        let closures = scope == TriageScope::Closures;
        let want = if closures {
            format!("Triage closures (2) {token}")
        } else {
            format!("Triage (2) {token}")
        };
        assert_eq!(b.title, want);
        assert_eq!(b.text.contains("--stage priority"), !closures, "{}", b.text);
    }
}

#[test]
fn the_page_hears_only_this_project_s_evaluated_backends() {
    let tr = tracked();
    let meta = |minutes: i64| -> Value {
        let r = cycle::cycle(&tr.t.root, &gh(), "home", &no_pages(), &move || at(minutes), PAGE).unwrap();
        let d: Value = serde_json::from_str(&fs::read_to_string(&r.outbox).unwrap()).unwrap();
        d["meta"]["evaluated_backends"].clone()
    };
    let evaluated = |project: &str, backend: &str, minutes: i64| {
        tr.t.root
            .mutate(at(minutes), |tx| {
                tx.record(
                    Kind::Evaluated,
                    &format!("{project}|{backend}"),
                    json!({"n": minutes}),
                    json!({"project": project, "backend": backend}),
                    ledger::Dedupe::Latest,
                )
            })
            .unwrap();
    };
    evaluated("kernel", "claude-haiku-4-5@low", 2);
    assert_eq!(meta(3), json!([]));
    evaluated("home", "claude-sonnet-5@high", 4);
    assert_eq!(meta(5), json!(["claude-sonnet-5@high"]));
}
