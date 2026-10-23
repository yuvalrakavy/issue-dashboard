//! Outcome records (7.7) and the `config` event (10.3).

mod common;

use std::fs;

use common::{ISSUE_A, launch_doc, t, temp_root};
use issue_dashboard_core::Error;
use issue_dashboard_core::config::ConfigSet;
use issue_dashboard_core::event::Kind;
use issue_dashboard_core::launch::{self, Via};
use issue_dashboard_core::ledger;
use issue_dashboard_core::outcome::{self, GateRuns, Outcome, RecordedBy};

fn outcome(landed: bool) -> Outcome {
    Outcome {
        launch: "L1".into(),
        landed,
        commit: Some("95c2531bb".into()),
        pull_request: None,
        wall_clock_minutes: Some(74),
        gate_runs: Some(vec![GateRuns {
            level: "quick".into(),
            runs: 2,
        }]),
        findings_fixed: Some(3),
        context_tokens: Some(180_000),
        recorded_by: RecordedBy::Session,
        missing: vec![],
    }
}

#[test]
fn an_outcome_for_an_unknown_launch_is_refused() {
    let r = temp_root();
    let err = r
        .root
        .mutate(t(0), |tx| outcome::record(tx, &outcome(true)))
        .unwrap_err();
    assert!(matches!(err, Error::Refused(_)), "{err}");
}

#[test]
fn an_outcome_is_recorded_once_and_a_different_second_one_is_refused() {
    let r = temp_root();
    r.root
        .mutate(t(0), |tx| {
            launch::request(tx, &launch_doc("L1", "home", &[ISSUE_A], "lanes"), Via::Ingest)
        })
        .unwrap();
    assert!(
        r.root
            .mutate(t(1), |tx| outcome::record(tx, &outcome(true)))
            .unwrap()
            .is_new()
    );
    assert!(
        !r.root
            .mutate(t(2), |tx| outcome::record(tx, &outcome(true)))
            .unwrap()
            .is_new()
    );
    let err = r
        .root
        .mutate(t(3), |tx| outcome::record(tx, &outcome(false)))
        .unwrap_err();
    assert!(matches!(err, Error::Refused(_)), "{err}");
    let events = r
        .root
        .read(|l| ledger::events_by_kind(l.conn(), Kind::Outcome))
        .unwrap();
    assert_eq!(events.len(), 1);
    common::assert_valid("event-outcome", &serde_json::to_value(&events[0]).unwrap());
}

fn write_config(dir: &std::path::Path, capacity: u32) {
    fs::create_dir_all(dir.join("projects")).unwrap();
    fs::write(
        dir.join("repos.toml"),
        format!(
            "[repo.\"example-org/server\"]\nlaunch = \"lanes\"\n\n[launch.lanes]\ncapacity = {capacity}\nworkspace = \"w\"\nrelease = \"r\"\nland = \"l\"\ngate.quick = {{ run = \"t\", minutes = 1 }}\n"
        ),
    )
    .unwrap();
    fs::write(
        dir.join("projects/home.toml"),
        "schema = 1\nname = \"Home\"\nowner_id = \"user_owner\"\n[[member]]\nrepo = \"example-org/server\"\n",
    )
    .unwrap();
}

#[test]
fn the_config_event_is_recorded_on_change_and_a_to_b_to_a_is_recorded() {
    let r = temp_root();
    let rec = |cap: u32| {
        write_config(r.path(), cap);
        let config = ConfigSet::load(r.path()).unwrap();
        r.root.mutate(t(0), |tx| config.record(tx)).unwrap().is_new()
    };
    assert!(rec(2));
    assert!(!rec(2), "an unchanged config was recorded again");
    assert!(rec(3));
    assert!(rec(2), "a config returning to an earlier content was not recorded");
    let events = r.root.read(|l| ledger::events_by_kind(l.conn(), Kind::Config)).unwrap();
    assert_eq!(events.len(), 3);
    for e in &events {
        common::assert_valid("event-config", &serde_json::to_value(e).unwrap());
    }
}

#[test]
fn config_load_reads_projects_and_finds_recipes() {
    let r = temp_root();
    write_config(r.path(), 2);
    let config = ConfigSet::load(r.path()).unwrap();
    assert_eq!(config.projects["home"].owner_id, "user_owner");
    assert_eq!(config.recipe("lanes").unwrap().capacity, 2);
    assert!(matches!(config.recipe("nope"), Err(Error::Refused(_))));
}

#[test]
fn a_repo_file_in_a_checkout_is_read_and_a_conflicting_recipe_is_refused() {
    let r = temp_root();
    write_config(r.path(), 2);
    let checkout = r.path().join("checkout");
    fs::create_dir_all(&checkout).unwrap();
    let registry = fs::read_to_string(r.path().join("repos.toml")).unwrap();
    fs::write(
        r.path().join("repos.toml"),
        registry.replace(
            "launch = \"lanes\"",
            &format!("launch = \"lanes\"\nlocal_path = \"{}\"", checkout.display()),
        ),
    )
    .unwrap();
    fs::write(
        checkout.join(".issue-dashboard.toml"),
        "areas = [\"kernel\"]\n[launch.lanes]\ncapacity = 5\nworkspace = \"w\"\nrelease = \"r\"\nland = \"l\"\ngate.quick = { run = \"t\", minutes = 1 }\n",
    )
    .unwrap();
    let config = ConfigSet::load(r.path()).unwrap();
    assert_eq!(
        config.repo_files["example-org/server"].areas.as_deref(),
        Some(&["kernel".to_string()][..])
    );
    assert!(
        matches!(config.recipe("lanes"), Err(Error::Refused(_))),
        "two capacities for one pool"
    );
}

#[test]
fn an_unknown_key_in_a_project_config_is_refused_naming_the_file() {
    let r = temp_root();
    write_config(r.path(), 2);
    fs::write(
        r.path().join("projects/home.toml"),
        "schema = 1\nname = \"H\"\nowner_id = \"u\"\nownr = \"typo\"\n[[member]]\nrepo = \"a/b\"\n",
    )
    .unwrap();
    match ConfigSet::load(r.path()) {
        Err(Error::Config { file, reason }) => {
            assert!(file.ends_with("projects/home.toml"));
            assert!(reason.contains("ownr"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
}
