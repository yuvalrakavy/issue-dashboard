//! Suggestions from a triage launch or a read-only recommender (5.8, triage spec 2.1): what
//! `record-suggestions` records, and the `suggestions` documents derive builds from them.
//!
//! A file is checked whole before anything is recorded: every entry must name an issue of the
//! launch by its alias, open at its latest observation, with the facts its kind needs; every
//! answer must answer a question `prepare` froze for the launch, under the launch's backend.
//! One bad entry refuses the file with every reason listed. An answer whose item changed since
//! prepare is dropped and reported, and the rest recorded. Each entry is one `suggested` event,
//! its run id the launch id, so the same file recorded again is a repeat.
//!
//! A document shows the latest suggestion of each kind for each issue until the issue closes,
//! or until the owner dismisses it. A dismissal holds for that issue and kind across later
//! runs, until the issue's observation changes after it (5.8: "unless its facts change").

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet, HashSet};

use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::canonical::sha256_hex;
use crate::config::ConfigSet;
use crate::decision::{self, Status};
use crate::docs::{Action, LaunchKind};
use crate::eval;
use crate::event::Kind;
use crate::github::{GitHub, Request};
use crate::launch::Launch;
use crate::layers;
use crate::ledger::{self, Dedupe, Tx};
use crate::root::{format_time, parse_time};
use crate::sync;
use crate::triage::{self, AnswerKind, Catalogue, Subject};
use crate::{Error, Result};

#[derive(Debug, Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Input {
    #[serde(default)]
    pub suggestions: Vec<Entry>,
    #[serde(default)]
    pub left_out: Vec<LeftOut>,
    /// The backend that answered: the launch's routing as model@effort (triage spec 2.1).
    /// Required with answers or estimates.
    #[serde(default)]
    pub backend: Option<String>,
    /// Answers to the launch's prepared questions.
    #[serde(default)]
    pub answers: Vec<Answer>,
    #[serde(default)]
    pub estimates: Vec<Estimate>,
}

/// An answer to one prepared question (triage spec 1.4).
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Answer {
    pub question: String,
    pub probabilities: BTreeMap<String, f64>,
    #[serde(default)]
    pub reason: String,
    /// The fixing commit, for likely_fixed answered true.
    #[serde(default)]
    pub commit: Option<String>,
    /// The other open issue, owner/repo#N, for duplicate answered true.
    #[serde(default)]
    pub related: Option<String>,
    /// The answer repeats a request in the reporter's own text (triage spec 4.1: marked).
    #[serde(default)]
    pub from_reporter: bool,
    /// The drafted reply to the reporter, for support_question answered true: the owner's close
    /// sheet opens with it (triage spec 1.4's support_question row).
    #[serde(default)]
    pub reply: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
pub enum Tier {
    T0,
    T1,
    T2,
    T3,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, serde::Serialize)]
pub enum Effort {
    S,
    M,
    L,
}

/// The unevaluated estimates of one item (triage spec 1.1): recorded, never a question.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Estimate {
    pub item: String,
    pub sites: Vec<String>,
    /// The commit the sites were read at.
    pub read_at: String,
    pub tier: Tier,
    pub blast: String,
    pub effort: Effort,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EntryKind {
    LikelyFixed,
    Duplicate,
    Obsolete,
    Batch,
}

impl EntryKind {
    fn as_str(self) -> &'static str {
        match self {
            EntryKind::LikelyFixed => "likely_fixed",
            EntryKind::Duplicate => "duplicate",
            EntryKind::Obsolete => "obsolete",
            EntryKind::Batch => "batch",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub kind: EntryKind,
    /// owner/repo#N; every kind but `batch`.
    #[serde(default)]
    pub item: Option<String>,
    /// A batch's members, owner/repo#N each.
    #[serde(default)]
    pub members: Vec<String>,
    #[serde(default)]
    pub surface: Option<String>,
    /// The other issue of a duplicate, owner/repo#N.
    #[serde(default)]
    pub related: Option<String>,
    /// The fixing commit of `likely_fixed`.
    #[serde(default)]
    pub commit: Option<String>,
    pub confidence: f64,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Why {
    NeedsDesign,
    Blocked,
    KeepOpen,
}

impl Why {
    fn as_str(self) -> &'static str {
        match self {
            Why::NeedsDesign => "needs_design",
            Why::Blocked => "blocked",
            Why::KeepOpen => "keep_open",
        }
    }
}

#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct LeftOut {
    pub item: String,
    pub why: Why,
    #[serde(default)]
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Report {
    pub launch: String,
    pub recorded: usize,
    pub repeats: usize,
    /// Answers not recorded because their item changed since prepare (triage spec 2.1).
    pub dropped: Vec<Dropped>,
}

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Dropped {
    pub question: String,
    pub item: String,
    pub decision: String,
    pub why: String,
}

/// One event to record: its subject (the item, or a batch's first member), its kind, and the
/// suggestion body.
struct Pending {
    subject: String,
    decision: String,
    body: Value,
}

/// Collects every reason a file is refused.
struct Check<'a> {
    conn: &'a Connection,
    launch: &'a Launch,
    errors: Vec<String>,
}

impl Check<'_> {
    fn fail(&mut self, e: impl Into<String>) {
        self.errors.push(e.into());
    }

    /// The node id of `alias` among the launch's items, open at its latest observation.
    fn open_item(&mut self, alias: &str) -> Result<Option<(String, String)>> {
        let l = self.launch;
        let Some(pos) = l.request.aliases.iter().position(|a| a.eq_ignore_ascii_case(alias)) else {
            self.fail(format!("{alias} is not in launch {}", l.id));
            return Ok(None);
        };
        let Some(id) = l.request.items.get(pos).cloned() else {
            self.fail(format!("launch {} names {alias} without its item", l.id));
            return Ok(None);
        };
        match ledger::latest(self.conn, &id, Kind::Observed)? {
            None => {
                self.fail(format!("{alias} has never been read"));
                Ok(None)
            }
            Some(e) if e.data["content"]["state"] != "open" => {
                self.fail(format!("{alias} is already closed"));
                Ok(None)
            }
            Some(e) => {
                let current = e.data["alias"].as_str().unwrap_or(alias).to_string();
                Ok(Some((id, current)))
            }
        }
    }

    /// Whether `sha`, named by `what` about item `alias`, is a commit of the item's repo as the
    /// lookup found it; refuses the file otherwise, naming why.
    fn commit(&mut self, commits: &Commits, what: &str, alias: &str, sha: &str) -> bool {
        if !is_commit(sha) {
            self.fail(format!("{what}: {sha} is not a commit hash"));
            return false;
        }
        let repo = repo_of(alias).unwrap_or_default();
        match commits.0.get(&(repo.clone(), sha.to_string())) {
            Some(Ok(true)) => return true,
            Some(Ok(false)) => self.fail(format!("{what}: {sha} is no commit of {repo}")),
            Some(Err(e)) => self.fail(format!(
                "{what}: commit {sha} of {repo} could not be checked ({e}); a closure's commit is recorded only once found"
            )),
            None => self.fail(format!("{what}: commit {sha} of {repo} was not looked up")),
        }
        false
    }

    fn common(&mut self, what: &str, confidence: f64, reason: &str) {
        if !(0.0..=1.0).contains(&confidence) {
            self.fail(format!("{what}: confidence {confidence} is not between 0 and 1"));
        }
        if reason.trim().is_empty() {
            self.fail(format!("{what} needs a reason"));
        }
    }
}

fn is_commit(s: &str) -> bool {
    (7..=40).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// The repo of an `owner/repo#n` alias, as the alias spells it: the lookup and the check that
/// reads it both take it from the same alias.
fn repo_of(alias: &str) -> Option<String> {
    alias.rsplit_once('#').map(|(r, _)| r.to_string())
}

/// What a read-only lookup found for each (repo, commit) a file names (triage spec 1.4: a
/// closure's commit must exist): `Ok(true)` a commit of that repo, `Ok(false)` none, `Err` the
/// lookup failed. Built before the ledger transaction, by [`check_commits`].
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Commits(BTreeMap<(String, String), std::result::Result<bool, String>>);

const COMMIT_QUERY: &str = "query($owner: String!, $name: String!, $rev: String!) { \
     repository(owner: $owner, name: $name) { object(expression: $rev) { __typename ... on Commit { oid } } } }";

/// The (repo, commit) pairs `input` names a commit for, spelled as a commit hash: an answer's
/// by the repo of the item its question was prepared about, a phase-1 entry's by its item's.
pub fn commits_named(conn: &Connection, input: &Input) -> Result<BTreeSet<(String, String)>> {
    let mut wanted = BTreeSet::new();
    let questions = if input.answers.iter().any(|a| a.commit.is_some()) {
        triage::prepared_questions(conn)?
    } else {
        BTreeMap::new()
    };
    for a in &input.answers {
        if let (Some(sha), Some((q, _))) = (a.commit.as_deref(), questions.get(&a.question))
            && is_commit(sha)
            && let Some(repo) = repo_of(&q.alias)
        {
            wanted.insert((repo, sha.to_string()));
        }
    }
    for e in &input.suggestions {
        if let (EntryKind::LikelyFixed, Some(sha), Some(item)) = (e.kind, e.commit.as_deref(), e.item.as_deref())
            && is_commit(sha)
            && let Some(repo) = repo_of(item)
        {
            wanted.insert((repo, sha.to_string()));
        }
    }
    Ok(wanted)
}

/// Looks each of `wanted` up once, read-only. A failed lookup is recorded as one, never as
/// found.
pub fn check_commits(gh: &dyn GitHub, wanted: &BTreeSet<(String, String)>) -> Commits {
    let mut found = BTreeMap::new();
    for (repo, sha) in wanted {
        let status = match repo.split_once('/') {
            None => Err(format!("{repo} is not owner/name")),
            Some((owner, name)) => {
                let req = Request {
                    name: "commit",
                    query: COMMIT_QUERY,
                    variables: json!({"owner": owner, "name": name, "rev": sha}),
                };
                match gh.graphql(&req) {
                    Err(e) => Err(e.to_string()),
                    // Errors win over a null object: a partial answer is a failed lookup.
                    Ok(doc) => match (
                        doc["errors"][0]["message"].as_str(),
                        doc["data"]["repository"].get("object"),
                    ) {
                        (Some(m), _) => Err(format!("GitHub answered: {m}")),
                        (None, Some(Value::Null)) => Ok(false),
                        // The expression resolves any revision; a ref named like the hash
                        // resolves to another commit, so the oid must be the one named.
                        (None, Some(o)) => Ok(o["__typename"] == "Commit"
                            && o["oid"].as_str().is_some_and(|oid| oid.starts_with(sha.as_str()))),
                        (None, None) => Err("GitHub's answer holds no repository".to_string()),
                    },
                }
            }
        };
        found.insert((repo.clone(), sha.clone()), status);
    }
    Commits(found)
}

fn probabilities(confidence: f64) -> Value {
    json!({"true": confidence, "false": 1.0 - confidence})
}

/// Records the suggestions of `input` for recommender launch `launch_id`, or refuses the whole
/// file with every reason. `commits` is what [`check_commits`] found of [`commits_named`]
/// before the transaction; a closure's commit it does not show as found is refused.
pub fn record(
    tx: &mut Tx<'_>,
    config: &ConfigSet,
    launch_id: &str,
    input: &Input,
    commits: &Commits,
) -> Result<Report> {
    let (l, _) = triage::answering_launch(tx.conn(), launch_id)?;
    let backend = triage::backend(&l.request.routing);
    let project = l.request.project.clone();
    let stronger_below = config
        .project(&project)?
        .stronger_look_below
        .unwrap_or(crate::config::STRONGER_LOOK_BELOW);
    let mut c = Check {
        conn: tx.conn(),
        launch: &l,
        errors: Vec::new(),
    };
    let mut pending: Vec<Pending> = Vec::new();
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut batched: HashSet<String> = HashSet::new();
    let mut report = Report {
        launch: launch_id.to_string(),
        recorded: 0,
        repeats: 0,
        dropped: Vec::new(),
    };

    // The backend is the launch's routing, never the file's claim (triage spec 2.1).
    match input.backend.as_deref() {
        None if !input.answers.is_empty() || !input.estimates.is_empty() => {
            c.fail("a file with answers or estimates names its backend")
        }
        Some(b) if b != backend => c.fail(format!(
            "the file's backend {b} is not launch {launch_id}'s routing {backend}; only the routed backend answers"
        )),
        _ => {}
    }
    // A stronger look answers the questions it was prepared and nothing else (2.5).
    if l.request.kind == LaunchKind::StrongerLook
        && !(input.suggestions.is_empty() && input.left_out.is_empty() && input.estimates.is_empty())
    {
        c.fail(format!(
            "launch {launch_id} is a stronger look; it answers its prepared questions only, with no suggestions, left_out or estimates"
        ));
    }
    answers(
        &mut c,
        config,
        &project,
        &backend,
        stronger_below,
        &input.answers,
        &mut seen,
        &mut pending,
        &mut report.dropped,
        commits,
    )?;
    for e in &input.estimates {
        let what = format!("{} (estimates)", e.item);
        c.common(&what, 1.0, &e.reason);
        if !is_commit(&e.read_at) {
            c.fail(format!("{what}: {} is not a commit hash", e.read_at));
        }
        if e.blast.trim().is_empty() {
            c.fail(format!("{what}: blast names the gate level a fix needs"));
        }
        if let Some((id, current)) = c.open_item(&e.item)? {
            if !seen.insert((id.clone(), "estimates".into())) {
                c.fail(format!("{} has two estimates entries", e.item));
            }
            pending.push(Pending {
                subject: id.clone(),
                decision: "estimates".into(),
                body: json!({
                    "item": id,
                    "alias": current,
                    "decision": "estimates",
                    "estimate": {
                        "sites": e.sites,
                        "read_at": e.read_at,
                        "tier": e.tier,
                        "blast": e.blast,
                        "effort": e.effort,
                    },
                    "backend": backend,
                    "evaluated": false,
                    "reason": e.reason,
                }),
            });
        }
    }

    for e in &input.suggestions {
        let kind = e.kind.as_str();
        if e.kind != EntryKind::Batch && l.request.kind == LaunchKind::Triage {
            c.fail(format!(
                "a {kind} entry: a triage launch answers its prepared {kind} questions instead"
            ));
            continue;
        }
        if e.kind == EntryKind::Batch {
            let what = format!("the batch of {}", e.members.join(", "));
            c.common(&what, e.confidence, &e.reason);
            if e.members.len() < 2 {
                c.fail(format!("{what}: a batch needs at least two members"));
            }
            if e.surface.as_deref().is_none_or(|s| s.trim().is_empty()) {
                c.fail(format!("{what}: a batch needs its surface, the code its members share"));
            }
            let mut members = Vec::new();
            for m in &e.members {
                if let Some(found) = c.open_item(m)? {
                    if !batched.insert(found.0.clone()) {
                        c.fail(format!("{m} is in two batches"));
                    }
                    members.push(found);
                }
            }
            // By the aliases the batch names (the launch's): an item keeps the name it was last
            // read under, so after a repo rename its members' observed names may differ.
            let repo = |a: &str| a.rsplit_once('#').map(|(r, _)| r.to_ascii_lowercase());
            if e.members.windows(2).any(|w| repo(&w[0]) != repo(&w[1])) {
                c.fail(format!("{what}: a batch's members are in one repo"));
            }
            if members.len() == e.members.len() && members.len() >= 2 {
                // The subject (what a dismissal is keyed by) is the same whatever order the
                // members are listed in; the card still leads with the first.
                let subject = members.iter().map(|m| m.0.clone()).min().expect("two or more members");
                pending.push(Pending {
                    subject,
                    decision: kind.into(),
                    body: json!({
                        "item": members[0].0,
                        "alias": members[0].1,
                        "decision": kind,
                        "answer": true,
                        "probabilities": probabilities(e.confidence),
                        "backend": backend,
                        "evaluated": false,
                        "reason": e.reason,
                        "members": members.iter().map(|m| &m.0).collect::<Vec<_>>(),
                        "member_aliases": members.iter().map(|m| &m.1).collect::<Vec<_>>(),
                        "surface": e.surface,
                    }),
                });
            }
            continue;
        }
        let Some(alias) = e.item.as_deref() else {
            c.fail(format!("a {kind} entry needs its item"));
            continue;
        };
        let what = format!("{alias} ({kind})");
        c.common(&what, e.confidence, &e.reason);
        let found = c.open_item(alias)?;
        let mut body = json!({
            "decision": kind,
            "answer": true,
            "probabilities": probabilities(e.confidence),
            "backend": backend,
            "evaluated": false,
            "reason": e.reason,
        });
        match e.kind {
            EntryKind::LikelyFixed => match e.commit.as_deref() {
                None => c.fail(format!("{alias}: likely_fixed needs the fixing commit")),
                Some(sha) => {
                    if c.commit(commits, alias, alias, sha) {
                        body["commit"] = json!(sha);
                    }
                }
            },
            EntryKind::Duplicate => match e.related.as_deref() {
                None => c.fail(format!("{alias}: duplicate needs the other issue (related)")),
                Some(other) => match sync::tracked(tx.conn(), config, &project, other) {
                    Err(Error::Refused(m)) => c.fail(m),
                    Err(err) => return Err(err),
                    Ok(other_id) => {
                        if found.as_ref().is_some_and(|f| f.0 == other_id) {
                            c.fail(format!("{alias} cannot be a duplicate of itself"));
                        }
                        body["related"] = json!(other_id);
                        body["related_alias"] = json!(alias_of(tx.conn(), &other_id)?.unwrap_or(other.to_string()));
                    }
                },
            },
            EntryKind::Obsolete | EntryKind::Batch => {}
        }
        if let Some((id, current)) = found {
            if !seen.insert((id.clone(), kind.into())) {
                c.fail(format!("{alias} has two {kind} entries"));
            }
            body["item"] = json!(id);
            body["alias"] = json!(current);
            pending.push(Pending {
                subject: id,
                decision: kind.into(),
                body,
            });
        }
    }
    for o in &input.left_out {
        let what = format!("{} (left_out)", o.item);
        c.common(&what, 1.0, &o.reason);
        if let Some((id, current)) = c.open_item(&o.item)? {
            if !seen.insert((id.clone(), "left_out".into())) {
                c.fail(format!("{} is left out twice", o.item));
            }
            pending.push(Pending {
                subject: id.clone(),
                decision: "left_out".into(),
                body: json!({
                    "item": id,
                    "alias": current,
                    "decision": "left_out",
                    "answer": o.why.as_str(),
                    "probabilities": {o.why.as_str(): 1.0},
                    "backend": backend,
                    "evaluated": false,
                    "reason": o.reason,
                    "why": o.why.as_str(),
                }),
            });
        }
    }
    if c.errors.is_empty() && pending.is_empty() && report.dropped.is_empty() {
        c.fail("the file has no answers, suggestions or estimates and nothing left out");
    }
    if !c.errors.is_empty() {
        return Err(Error::refused(format!(
            "record-suggestions refused for launch {launch_id}; nothing was recorded:\n- {}",
            c.errors.join("\n- ")
        )));
    }

    for p in pending {
        // An answer is one per question: a launch re-asks a question when its item changes.
        let mut distinct = json!({"run_id": launch_id, "decision": p.decision});
        if let Some(q) = p.body.get("question") {
            distinct["question"] = q.clone();
        }
        let data = json!({"project": project, "run_id": launch_id, "suggestion": p.body});
        let r = tx.record_valid(Kind::Suggested, &p.subject, distinct, data.clone(), Dedupe::Any)?;
        if r.is_new() {
            report.recorded += 1;
        } else if without_context(&r.event().data) == without_context(&data) {
            report.repeats += 1;
        } else {
            return Err(Error::refused(format!(
                "launch {launch_id} already recorded a different {} suggestion for {} (seq {}); a run records each once",
                p.decision,
                data["suggestion"]["alias"].as_str().unwrap_or(&p.subject),
                r.event().seq
            )));
        }
    }
    Ok(report)
}

/// Checks each answer against the question `prepare` froze for this launch (triage spec 2.1):
/// an unknown question, a second answer to one question or to one (item, decision), bad
/// probabilities, a missing reason or a missing closure subject refuse the file; an answer
/// whose item closed or whose input changed since prepare is dropped. The calibration map is
/// the one the question froze (2.4).
/// A recorded suggestion's data without what the tracker computes when it records it (the
/// item's `current` value and the second opinion against it): a retry of the same answer file
/// is the same suggestion although the item's value moved since (2.1).
fn without_context(data: &Value) -> Value {
    let mut d = data.clone();
    if let Some(s) = d.get_mut("suggestion").and_then(Value::as_object_mut) {
        s.remove("current");
        s.remove("second_opinion");
    }
    d
}

/// Whether an applied page decision has dismissed suggestion `id` (an accept or edit is in
/// [`layers::Layers::acted`]).
fn acted_on(conn: &Connection, project: &str, id: &str) -> Result<bool> {
    Ok(crate::decision::effective(conn, project)?
        .iter()
        .any(|d| d.action == crate::docs::Action::DismissSuggestion && d.target.suggestion.as_deref() == Some(id)))
}

#[allow(clippy::too_many_arguments)]
fn answers(
    c: &mut Check<'_>,
    config: &ConfigSet,
    project: &str,
    backend: &str,
    stronger_below: f64,
    answers: &[Answer],
    seen: &mut HashSet<(String, String)>,
    pending: &mut Vec<Pending>,
    dropped: &mut Vec<Dropped>,
    commits: &Commits,
) -> Result<()> {
    if answers.is_empty() {
        return Ok(());
    }
    let cat = Catalogue::embedded();
    let conn = c.conn;
    let questions = triage::prepared_questions(conn)?;
    let asked = triage::asked(conn)?;
    let mut label_layers: Option<layers::Layers> = None;
    let mut answered: BTreeSet<&str> = BTreeSet::new();
    for a in answers {
        let q = match questions.get(&a.question) {
            Some((q, launch)) if *launch == c.launch.id => q,
            _ => {
                c.fail(format!(
                    "{}: launch {} prepared no such question",
                    a.question, c.launch.id
                ));
                continue;
            }
        };
        let what = format!("{} ({}, {})", q.alias, q.decision, q.id);
        if !answered.insert(&a.question) {
            c.fail(format!("{what} is answered twice"));
            continue;
        }
        if !seen.insert((q.item.clone(), q.decision.clone())) {
            c.fail(format!("{what}: {} has two {} answers", q.alias, q.decision));
        }
        if a.reason.trim().is_empty() {
            c.fail(format!("{what} needs a reason"));
        }
        let f = cat.file(&q.decision)?;
        let Some(o) = triage::observed(conn, config, project, &q.item)? else {
            c.fail(format!("{what}: {} has never been read", q.alias));
            continue;
        };
        // Dropped, not refused: the item moved under the session (2.1). Checked before the
        // answer itself, which is checked against what the question is now (its areas).
        let mut live = Value::Null;
        let changed = if !o.open {
            Some("the issue closed since prepare".to_string())
        } else {
            match triage::live_input(config, cat, &asked, &o, &q.decision)? {
                Ok((input, h)) if h == q.input_hash => {
                    live = input;
                    None
                }
                Ok(_) => Some("the issue changed since prepare".to_string()),
                Err(why) => Some(why),
            }
        };
        if let Some(why) = changed {
            dropped.push(Dropped {
                question: q.id.clone(),
                item: o.source.alias.clone(),
                decision: q.decision.clone(),
                why,
            });
            continue;
        }
        // The owner accepted, edited or dismissed this run's card: a new answer under the same
        // card would inherit that verdict (4.2), so it is dropped.
        if layers::LABEL_DECISIONS.contains(&q.decision.as_str()) {
            if label_layers.is_none() {
                label_layers = Some(layers::layers(conn, project)?);
            }
            let id = doc_id(&c.launch.id, &q.decision, &q.item);
            if label_layers.as_ref().expect("loaded above").acted.contains(&id) || acted_on(conn, project, &id)? {
                dropped.push(Dropped {
                    question: q.id.clone(),
                    item: o.source.alias.clone(),
                    decision: q.decision.clone(),
                    why: "the owner already acted on this run's card".to_string(),
                });
                continue;
            }
        }
        let areas = triage::areas_of(&triage::translation_of(config, &o.repo));
        let allowed = cat.allowed(&q.decision, &areas)?;
        let (answer, raw) = match triage::check_probabilities(f.kind, &allowed, &a.probabilities) {
            Ok(x) => x,
            Err(m) => {
                c.fail(format!("{what}: {m}"));
                continue;
            }
        };
        let calibrated = triage::calibrate(conn, f.kind, &raw, q.map_id.as_deref())?;
        let mut body = json!({
            "item": q.item,
            "alias": o.source.alias,
            "decision": q.decision,
            "answer": answer,
            "probabilities": calibrated,
            "raw": raw,
            "map_id": q.map_id,
            "question": q.id,
            "backend": backend,
            "evaluated": false,
            "reason": a.reason,
        });
        if a.from_reporter {
            body["from_reporter"] = json!(true);
        }
        // An answer the backend was unsure of needs a stronger look (2.5): marked unless it is
        // the stronger look's own.
        let top = calibrated.values().copied().fold(0.0_f64, f64::max);
        if c.launch.request.kind != LaunchKind::StrongerLook
            && triage::STRONGER_LOOK_DECISIONS.contains(&q.decision.as_str())
            && top < stronger_below
        {
            body["needs_stronger_look"] = json!(true);
        }
        if q.decision == "priority" {
            body["rubric_level"] = live.get("rubric_level").cloned().unwrap_or(Value::Null);
        }
        // What the item's value was when the answer was recorded, and whether the answer is a
        // confident disagreement with it (triage spec 4.1): the card shows only while the value
        // is still this one (4.2's stale accept).
        if layers::LABEL_DECISIONS.contains(&q.decision.as_str()) {
            if label_layers.is_none() {
                label_layers = Some(layers::layers(conn, project)?);
            }
            let l = label_layers.as_ref().expect("loaded above");
            let current =
                layers::effective_of(conn, config, l, &q.item)?.map_or(Value::Null, |e| e.current(&q.decision));
            if let Some(opinion) = second_opinion(conn, &q.decision, &current, &calibrated, q.map_id.as_deref())? {
                body["second_opinion"] = opinion;
            }
            body["current"] = current;
        }
        // A closure answered true names its subject (1.4).
        let yes = answer == json!(true);
        // A support question answered true carries the reply drafted for the reporter.
        let reply = a.reply.as_deref().map(str::trim).filter(|r| !r.is_empty());
        match (q.decision == "support_question" && yes, reply) {
            (true, Some(r)) if r.chars().count() > REPLY_MAX => {
                c.fail(format!("{what}: a reply is at most {REPLY_MAX} characters"))
            }
            (true, Some(r)) => body["reply"] = json!(r),
            (true, None) => c.fail(format!(
                "{what}: a support question answered true needs its drafted reply"
            )),
            (false, Some(_)) => c.fail(format!("{what}: only a support question answered true takes a reply")),
            (false, None) => {}
        }
        match (f.subject, yes) {
            (Some(Subject::Commit), true) => match a.commit.as_deref() {
                None => c.fail(format!("{what}: likely_fixed needs the fixing commit")),
                Some(sha) => {
                    if c.commit(commits, &what, &q.alias, sha) {
                        body["commit"] = json!(sha);
                    }
                }
            },
            (Some(Subject::Related), true) => match a.related.as_deref() {
                None => c.fail(format!("{what}: duplicate needs the other issue (related)")),
                Some(other) => match sync::tracked(conn, config, project, other) {
                    Err(Error::Refused(m)) => c.fail(m),
                    Err(err) => return Err(err),
                    Ok(other_id) if other_id == q.item => {
                        c.fail(format!("{what}: {} cannot be a duplicate of itself", q.alias))
                    }
                    Ok(other_id) => {
                        if observed_open(conn, &other_id)? != Some(true) {
                            c.fail(format!("{what}: {other} is not an open issue"));
                        }
                        body["related"] = json!(other_id);
                        body["related_alias"] = json!(alias_of(conn, &other_id)?.unwrap_or(other.to_string()));
                    }
                },
            },
            _ => {}
        }
        if f.kind == AnswerKind::Areas && areas.is_empty() {
            c.fail(format!("{what}: the repo declares no areas"));
        }
        pending.push(Pending {
            subject: q.item.clone(),
            decision: q.decision.clone(),
            body,
        });
    }
    Ok(())
}

fn observed_open(conn: &Connection, id: &str) -> Result<Option<bool>> {
    Ok(ledger::latest(conn, id, Kind::Observed)?.map(|e| e.data["content"]["state"] == "open"))
}

fn alias_of(conn: &Connection, id: &str) -> Result<Option<String>> {
    Ok(ledger::latest(conn, id, Kind::Observed)?.and_then(|e| e.data["alias"].as_str().map(str::to_string)))
}

/// The closing decisions a `suggestions` document shows (5.8); the label decisions' cards are
/// triage spec 4.1's.
const CARDS: [&str; 6] = [
    "likely_fixed",
    "duplicate",
    "obsolete",
    "support_question",
    "batch",
    "left_out",
];

/// A labelled item's label is a second opinion's subject when its calibrated probability is at
/// most this (triage spec 4.1).
pub const SECOND_OPINION_AT: f64 = 0.2;

/// At most this many second opinions a run (4.1).
pub const SECOND_OPINIONS_PER_RUN: usize = 5;

/// The second opinion an answer gives about the item's `current` value, if it is one (4.1):
/// type and priority when the current value's calibrated probability is at most
/// [`SECOND_OPINION_AT`]; area when any current area's is (`flagged`). It names the current value
/// and the false-alarm rate the map's evaluation measured (null without a map).
fn second_opinion(
    conn: &Connection,
    decision: &str,
    current: &Value,
    calibrated: &BTreeMap<String, f64>,
    map_id: Option<&str>,
) -> Result<Option<Value>> {
    let p = |a: &str| calibrated.get(a).copied();
    let (p_current, flagged) = match (decision, current) {
        ("area", Value::Array(set)) => {
            let flagged: Vec<(String, f64)> = set
                .iter()
                .filter_map(|a| a.as_str())
                .filter_map(|a| p(a).filter(|x| *x <= SECOND_OPINION_AT).map(|x| (a.to_string(), x)))
                .collect();
            let Some(low) = flagged.iter().map(|(_, x)| *x).reduce(f64::min) else {
                return Ok(None);
            };
            (low, Some(flagged.into_iter().map(|(a, _)| a).collect::<Vec<_>>()))
        }
        (_, Value::String(c)) => match p(c) {
            Some(x) if x <= SECOND_OPINION_AT => (x, None),
            _ => return Ok(None),
        },
        _ => return Ok(None),
    };
    let false_alarm = match map_id {
        Some(m) => evaluation_of_map(conn, m)?.and_then(|d| d["false_alarm_rate"].as_f64()),
        None => None,
    };
    let mut o = json!({"current": current, "p_current": p_current, "false_alarm_rate": false_alarm});
    if let Some(f) = flagged {
        o["flagged"] = json!(f);
    }
    Ok(Some(o))
}

/// The `evaluated` data that wrote map `map_id`.
fn evaluation_of_map(conn: &Connection, map_id: &str) -> Result<Option<Value>> {
    Ok(ledger::events_by_kind(conn, Kind::Evaluated)?
        .into_iter()
        .find(|e| e.data["map_id"] == map_id)
        .map(|e| e.data))
}

/// The document id of the suggestion of `decision` about `subject` in run `run`.
pub(crate) fn doc_id(run: &str, decision: &str, subject: &str) -> String {
    format!("sg-{}", &sha256_hex(&format!("{run}|{decision}|{subject}"))[..24])
}

/// The id of a combined card (4.1) over its type and priority parts.
pub(crate) fn card_id(type_doc: &str, priority_doc: &str) -> String {
    format!("cd-{}", &sha256_hex(&format!("{type_doc}|{priority_doc}"))[..24])
}

fn action_label(decision: &str) -> Option<&'static str> {
    match decision {
        "likely_fixed" => Some("Check and close…"),
        "duplicate" => Some("Close as duplicate…"),
        "obsolete" | "support_question" => Some("Close as not planned…"),
        "batch" => Some("Launch batch…"),
        _ => None,
    }
}

/// How a label answer reads on its button: "bug", "p1", "kernel, sdl".
fn answer_text(answer: &Value) -> String {
    match answer {
        Value::String(s) => s.clone(),
        Value::Array(a) => a.iter().filter_map(Value::as_str).collect::<Vec<_>>().join(", "),
        other => other.to_string(),
    }
}

/// A label card's confidence: the calibrated probability of its answer; for area the lowest of
/// the chosen set's.
fn label_confidence(s: &Value) -> f64 {
    let p = |a: &str| s["probabilities"][a].as_f64();
    match &s["answer"] {
        Value::String(a) => p(a),
        Value::Array(set) => set.iter().filter_map(|a| a.as_str().and_then(p)).reduce(f64::min),
        _ => None,
    }
    .unwrap_or(0.0)
}

/// The latest observation's seq and whether it shows the item open.
fn observed(conn: &Connection, id: &str) -> Result<Option<(i64, bool, Option<String>)>> {
    Ok(ledger::latest(conn, id, Kind::Observed)?.map(|e| {
        (
            e.seq,
            e.data["content"]["state"] == "open",
            e.data["alias"].as_str().map(str::to_string),
        )
    }))
}

/// A suggestion's badge (triage spec 4.1, 3.5) and whether apply all may take it (4.3), from the
/// evaluations in the ledger.
pub(crate) struct Badges<'a> {
    conn: &'a Connection,
    project: &'a str,
    /// The evaluation that wrote each map.
    by_map: BTreeMap<String, Value>,
    /// Each key's latest evaluation.
    latest: BTreeMap<String, Value>,
    drift: RefCell<BTreeMap<String, bool>>,
}

impl<'a> Badges<'a> {
    pub(crate) fn load(conn: &'a Connection, project: &'a str) -> Result<Badges<'a>> {
        let mut by_map = BTreeMap::new();
        let mut latest = BTreeMap::new();
        for e in ledger::events_by_kind(conn, Kind::Evaluated)? {
            if e.data["project"] != project {
                continue;
            }
            if let Some(m) = e.data["map_id"].as_str() {
                by_map.insert(m.to_string(), e.data.clone());
            }
            latest.insert(e.subject.clone(), e.data);
        }
        Ok(Badges {
            conn,
            project,
            by_map,
            latest,
            drift: RefCell::new(BTreeMap::new()),
        })
    }

    /// The badge of suggestion `body`, and whether its key passes Bulk and is not drifting.
    /// The badge is the evaluation of the suggestion's own map (2.4): uncalibrated without one,
    /// unevaluated when that evaluation failed its bars, drifting when its key drifts (3.5).
    /// Bulk needs the suggestion's own map and the key's latest evaluation both to pass it.
    pub(crate) fn of(&self, body: &Value) -> Result<(&'static str, bool)> {
        let Some(data) = body["map_id"].as_str().and_then(|m| self.by_map.get(m)) else {
            return Ok(("uncalibrated", false));
        };
        if data["bars"]["evaluated"] != true {
            return Ok(("unevaluated", false));
        }
        let backend = body["backend"].as_str().unwrap_or_default();
        let decision = body["decision"].as_str().unwrap_or_default();
        let hash = data["question_hash"].as_str().unwrap_or_default();
        let key = eval::key_subject(self.project, backend, decision, hash);
        let cached = self.drift.borrow().get(&key).copied();
        let drifting = match cached {
            Some(d) => d,
            None => {
                let d = eval::drift(self.conn, self.project, backend, decision, hash)?.drifting;
                self.drift.borrow_mut().insert(key.clone(), d);
                d
            }
        };
        if drifting {
            return Ok(("drifting", false));
        }
        let bulk = data["bars"]["bulk"] == true && self.latest.get(&key).is_some_and(|l| l["bars"]["bulk"] == true);
        Ok(("evaluated", bulk))
    }
}

/// The order second opinions are kept in under the cap (4.1): priority, type, area.
fn opinion_rank(decision: &str) -> usize {
    match decision {
        "priority" => 0,
        "type" => 1,
        _ => 2,
    }
}

/// The `suggestions` documents of `project` (5.8, triage spec 4.1).
///
/// - Closing cards as phase 1: a closure answered false is no card and hides an older true one.
/// - A label card (type, area, priority) shows while the item's value is still the one the
///   suggestion recorded (`current`): for an item untriaged on it, or as a second opinion, at
///   most [`SECOND_OPINIONS_PER_RUN`] of a run's. An agreeing answer is no card. A card an
///   applied accept or edit acted on never returns.
/// - Untriaged on both type and priority, the two parts share one combined card.
pub fn documents(conn: &Connection, config: &ConfigSet, project: &str) -> Result<Vec<Value>> {
    let events: Vec<_> = ledger::events_by_kind(conn, Kind::Suggested)?
        .into_iter()
        .filter(|e| e.data["project"] == project)
        .collect();
    // Every document id ever shown, to what it was about; and the latest of each (item, kind).
    let mut about: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut latest: BTreeMap<(String, String), &crate::event::Event> = BTreeMap::new();
    for e in &events {
        let decision = e.distinct["decision"].as_str().unwrap_or_default().to_string();
        let run = e.distinct["run_id"].as_str().unwrap_or_default();
        about.insert(
            doc_id(run, &decision, &e.subject),
            (e.subject.clone(), decision.clone()),
        );
        latest.insert((e.subject.clone(), decision), e);
    }
    // The latest applied dismissal of each (item, kind), by the seq it was decided at.
    let mut dismissed: BTreeMap<(String, String), i64> = BTreeMap::new();
    for (seq, d, status) in decision::resolved(conn, project)? {
        if d.action != Action::DismissSuggestion || status != Status::Applied {
            continue;
        }
        if let Some(key) = d.target.suggestion.as_ref().and_then(|s| about.get(s)) {
            let at = dismissed.entry(key.clone()).or_insert(seq);
            *at = (*at).max(seq);
        }
    }
    let is_label = |d: &str| layers::LABEL_DECISIONS.contains(&d);
    let layers = layers::layers(conn, project)?;
    let badges = Badges::load(conn, project)?;

    // The cap is over the second opinions a run recorded (4.1: "at most 5 a run"), so acting on
    // one never brings a sixth forward.
    let mut kept_opinions: BTreeSet<String> = BTreeSet::new();
    {
        let mut by_run: BTreeMap<&str, Vec<(usize, f64, String)>> = BTreeMap::new();
        for ((subject, decision), e) in &latest {
            let o = &e.data["suggestion"]["second_opinion"];
            if !is_label(decision) || !o.is_object() {
                continue;
            }
            let run = e.distinct["run_id"].as_str().unwrap_or_default();
            by_run.entry(run).or_default().push((
                opinion_rank(decision),
                o["p_current"].as_f64().unwrap_or(0.0),
                doc_id(run, decision, subject),
            ));
        }
        for (_, mut v) in by_run {
            v.sort_by(|a, b| {
                (a.0, a.1)
                    .partial_cmp(&(b.0, b.1))
                    .unwrap_or(std::cmp::Ordering::Equal)
                    .then(a.2.cmp(&b.2))
            });
            kept_opinions.extend(v.into_iter().take(SECOND_OPINIONS_PER_RUN).map(|x| x.2));
        }
    }

    let mut out = Vec::new();
    for ((subject, decision), e) in latest {
        let label = is_label(&decision);
        if !label && (!CARDS.contains(&decision.as_str()) || e.data["suggestion"]["answer"] == json!(false)) {
            continue;
        }
        let Some((mut seen_at, open, _)) = observed(conn, &subject)? else {
            continue;
        };
        if decision == "batch" {
            // A dismissed batch returns when any of its members changes.
            for m in e.data["suggestion"]["members"].as_array().into_iter().flatten() {
                if let Some((seq, _, _)) = observed(conn, m.as_str().unwrap_or_default())? {
                    seen_at = seen_at.max(seq);
                }
            }
        }
        if dismissed
            .get(&(subject.clone(), decision.clone()))
            .is_some_and(|d| seen_at < *d)
        {
            continue;
        }
        let run = e.distinct["run_id"].as_str().unwrap_or_default();
        let id = doc_id(run, &decision, &subject);
        let mut s = e.data["suggestion"].clone();
        let mut second = false;
        if label {
            if !open || layers.acted.contains(&id) {
                continue;
            }
            // A suggestion recorded before cards existed holds no `current`: no card.
            let Some(recorded) = s.get("current").cloned() else {
                continue;
            };
            let now = layers::effective_of(conn, config, &layers, &subject)?.map(|v| v.current(&decision));
            if now.as_ref() != Some(&recorded) {
                continue;
            }
            second = s["second_opinion"].is_object();
            if !layers::is_unset(&recorded) && !(second && kept_opinions.contains(&id)) {
                continue;
            }
        } else if decision == "batch" {
            // A closed member leaves the batch; below two members there is no batch.
            let mut kept = Vec::new();
            let members = s["members"].as_array().cloned().unwrap_or_default();
            let aliases = s["member_aliases"].as_array().cloned().unwrap_or_default();
            for (m, a) in members.iter().zip(aliases) {
                let id = m.as_str().unwrap_or_default();
                if let Some((_, true, current)) = observed(conn, id)? {
                    kept.push((id.to_string(), current.map_or(a, Value::String)));
                }
            }
            if kept.len() < 2 {
                continue;
            }
            s["item"] = json!(kept[0].0);
            s["alias"] = kept[0].1.clone();
            s["members"] = json!(kept.iter().map(|k| &k.0).collect::<Vec<_>>());
            s["member_aliases"] = json!(kept.iter().map(|k| &k.1).collect::<Vec<_>>());
        } else if !open {
            continue;
        }
        let (badge, bulk) = badges.of(&s)?;
        let confidence = if label {
            label_confidence(&s)
        } else {
            s["probabilities"]["true"].as_f64().unwrap_or(1.0)
        };
        let button = if label {
            let verb = if second { "Change to" } else { "Set" };
            Some(format!("{verb} {}", answer_text(&s["answer"])))
        } else {
            action_label(&decision).map(str::to_string)
        };
        let Value::Object(mut doc) = s else { continue };
        doc.insert("id".into(), json!(id));
        doc.insert("project".into(), json!(project));
        doc.insert("run_id".into(), json!(run));
        doc.insert("kind".into(), json!(decision));
        doc.insert("made_at".into(), json!(format_time(parse_time(&e.time)?)));
        doc.insert("confidence".into(), json!(confidence));
        doc.insert("badge".into(), json!(badge));
        // Apply all takes only an untriaged item's label card on a Bulk key (4.3).
        doc.insert("bulk_ok".into(), json!(label && !second && bulk));
        doc.insert("card".into(), json!(id));
        if let Some(label) = button {
            doc.insert("action_label".into(), json!(label));
        }
        out.push(Value::Object(doc));
    }
    // One combined card for an item untriaged on type and priority (4.1).
    let mut parts: BTreeMap<String, [Option<usize>; 2]> = BTreeMap::new();
    for (i, d) in out.iter().enumerate() {
        if d.get("second_opinion").is_some_and(Value::is_object) {
            continue;
        }
        let slot = match d["kind"].as_str() {
            Some("type") => 0,
            Some("priority") => 1,
            _ => continue,
        };
        parts
            .entry(d["item"].as_str().unwrap_or_default().to_string())
            .or_default()[slot] = Some(i);
    }
    for [t, p] in parts.into_values() {
        let (Some(t), Some(p)) = (t, p) else { continue };
        let card = card_id(
            out[t]["id"].as_str().unwrap_or_default(),
            out[p]["id"].as_str().unwrap_or_default(),
        );
        let label = format!(
            "Set {} · {}",
            answer_text(&out[t]["answer"]),
            answer_text(&out[p]["answer"])
        );
        for i in [t, p] {
            out[i]["card"] = json!(card);
            out[i]["action_label"] = json!(label);
        }
    }
    out.sort_by(|a, b| (a["made_at"].as_str(), a["id"].as_str()).cmp(&(b["made_at"].as_str(), b["id"].as_str())));
    Ok(out)
}

/// The longest drafted reply a support question may carry: well inside a GitHub comment (65536)
/// and the page's close sheet, and small beside a published generation.
pub const REPLY_MAX: usize = 10_000;

/// The latest instructs_agent answer about `item` when it is true (spec 4.2, triage spec 1.4):
/// the warning its Details and briefs carry. Its `reason`, `backend` and calibrated
/// `probability` of true.
pub(crate) fn instructs_agent(conn: &Connection, item: &str) -> Result<Option<Value>> {
    let latest = ledger::events_of(conn, item, Kind::Suggested)?
        .into_iter()
        .rfind(|e| e.distinct["decision"] == "instructs_agent");
    Ok(latest.and_then(|e| {
        let s = &e.data["suggestion"];
        (s["answer"] == json!(true)).then(|| {
            json!({
                "reason": s["reason"].as_str().unwrap_or_default(),
                "backend": s["backend"],
                "probability": s["probabilities"]["true"],
            })
        })
    }))
}

/// The answers that need a stronger look (2.5), by item: of each (item, decision) in
/// [`triage::STRONGER_LOOK_DECISIONS`], `project`'s latest answer when it is marked, about the item
/// as it is now (an answer about older facts is due for an ordinary run, which re-asks it), the
/// item open, and the owner has not acted on it: no accept or edit of that answer, and no applied
/// dismissal of the (item, decision) since the item was last observed (the card's own rule).
/// Each entry is {decision, answer, confidence (the top calibrated probability), backend,
/// suggestion}. The issue document lists these, and a stronger look prepares exactly these.
pub(crate) fn stronger_look_open(conn: &Connection, project: &str) -> Result<BTreeMap<String, Vec<Value>>> {
    let questions = triage::prepared_questions(conn)?;
    let mut about: BTreeMap<String, (String, String)> = BTreeMap::new();
    let mut latest: BTreeMap<(String, String), crate::event::Event> = BTreeMap::new();
    for e in ledger::events_by_kind(conn, Kind::Suggested)? {
        let decision = e.distinct["decision"].as_str().unwrap_or_default().to_string();
        if e.data["project"] != project || !triage::STRONGER_LOOK_DECISIONS.contains(&decision.as_str()) {
            continue;
        }
        let run = e.distinct["run_id"].as_str().unwrap_or_default();
        about.insert(
            doc_id(run, &decision, &e.subject),
            (e.subject.clone(), decision.clone()),
        );
        latest.insert((e.subject.clone(), decision), e);
    }
    let mut dismissed: BTreeMap<(String, String), i64> = BTreeMap::new();
    for (seq, d, status) in decision::resolved(conn, project)? {
        if d.action != Action::DismissSuggestion || status != Status::Applied {
            continue;
        }
        if let Some(key) = d.target.suggestion.as_ref().and_then(|s| about.get(s)) {
            let at = dismissed.entry(key.clone()).or_insert(seq);
            *at = (*at).max(seq);
        }
    }
    let acted = layers::layers(conn, project)?.acted;
    let mut out: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    for ((item, decision), e) in &latest {
        let s = &e.data["suggestion"];
        if !triage::needs_stronger_look(s) {
            continue;
        }
        let Some((seq, true, _)) = observed(conn, item)? else {
            continue;
        };
        let now = s["question"]
            .as_str()
            .and_then(|q| questions.get(q))
            .is_some_and(|(q, _)| q.observed >= seq);
        let id = doc_id(e.distinct["run_id"].as_str().unwrap_or_default(), decision, item);
        if !now
            || acted.contains(&id)
            || dismissed
                .get(&(item.clone(), decision.clone()))
                .is_some_and(|d| seq < *d)
        {
            continue;
        }
        out.entry(item.clone()).or_default().push(json!({
            "decision": decision,
            "answer": s["answer"],
            "confidence": s["probabilities"]
                .as_object()
                .map_or(0.0, |p| p.values().filter_map(Value::as_f64).fold(0.0_f64, f64::max)),
            "backend": s["backend"],
            "suggestion": id,
        }));
    }
    Ok(out)
}

/// The `triage` of each item's issue document (4.5): its latest severity, reach and
/// detectability answers, each {answer, probabilities, backend, badge, suggestion, edited},
/// `edited` the owner's Details edit (null: none); its latest estimates; `stronger_look`, the
/// answers that need a stronger look ([`stronger_look_open`]); and `instructs_agent`, the
/// warning (spec 4.2).
pub fn triage_details(conn: &Connection, project: &str, layers: &layers::Layers) -> Result<BTreeMap<String, Value>> {
    let badges = Badges::load(conn, project)?;
    let mut latest: BTreeMap<(String, String), crate::event::Event> = BTreeMap::new();
    for e in ledger::events_by_kind(conn, Kind::Suggested)? {
        let decision = e.distinct["decision"].as_str().unwrap_or_default().to_string();
        if e.data["project"] == project
            && (layers::DETAIL_DECISIONS.contains(&decision.as_str())
                || decision == "estimates"
                || decision == "instructs_agent")
        {
            latest.insert((e.subject.clone(), decision), e);
        }
    }
    let mut out: BTreeMap<String, Value> = BTreeMap::new();
    for (item, list) in stronger_look_open(conn, project)? {
        out.entry(item).or_insert_with(|| json!({}))["stronger_look"] = json!(list);
    }
    for ((item, decision), e) in &latest {
        let s = &e.data["suggestion"];
        let run = e.distinct["run_id"].as_str().unwrap_or_default();
        if decision == "instructs_agent" {
            if let Some(w) = instructs_agent(conn, item)? {
                out.entry(item.clone()).or_insert_with(|| json!({}))["instructs_agent"] = w;
            }
            continue;
        }
        let entry = out.entry(item.clone()).or_insert_with(|| json!({}));
        if decision == "estimates" {
            let mut est = s["estimate"].clone();
            est["backend"] = s["backend"].clone();
            entry["estimates"] = est;
            continue;
        }
        let edited = layers.details.get(item).and_then(|d| d.get(decision)).cloned();
        entry[decision.as_str()] = json!({
            "answer": s["answer"],
            "probabilities": s["probabilities"],
            "backend": s["backend"],
            "badge": badges.of(s)?.0,
            "suggestion": doc_id(run, decision, item),
            "edited": edited,
        });
    }
    // A Details edit without a suggestion still holds.
    for (item, edits) in &layers.details {
        let entry = out.entry(item.clone()).or_insert_with(|| json!({}));
        for (decision, v) in edits {
            if entry.get(decision.as_str()).is_none() {
                entry[decision.as_str()] = json!({
                    "answer": null,
                    "probabilities": null,
                    "backend": null,
                    "badge": null,
                    "suggestion": null,
                    "edited": v,
                });
            }
        }
    }
    Ok(out)
}
