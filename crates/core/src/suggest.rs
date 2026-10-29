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

use std::collections::{BTreeMap, BTreeSet, HashSet};

use rusqlite::Connection;
use serde::Deserialize;
use serde_json::{Value, json};

use crate::canonical::sha256_hex;
use crate::config::ConfigSet;
use crate::decision::{self, Status};
use crate::docs::{Action, LaunchKind};
use crate::event::Kind;
use crate::github::{GitHub, Request};
use crate::launch::Launch;
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
                    Ok(doc) => match doc["data"]["repository"].get("object") {
                        Some(Value::Null) => Ok(false),
                        Some(o) => Ok(o["__typename"] == "Commit"),
                        None => Err(match doc["errors"][0]["message"].as_str() {
                            Some(m) => format!("GitHub answered: {m}"),
                            None => "GitHub's answer holds no repository".to_string(),
                        }),
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
    answers(
        &mut c,
        config,
        &project,
        &backend,
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
        } else if r.event().data == data {
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
#[allow(clippy::too_many_arguments)]
fn answers(
    c: &mut Check<'_>,
    config: &ConfigSet,
    project: &str,
    backend: &str,
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
        let changed = if !o.open {
            Some("the issue closed since prepare".to_string())
        } else {
            match triage::live_input(config, cat, &asked, &o, &q.decision)? {
                Ok((_, h)) if h == q.input_hash => None,
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
        let areas = triage::areas_of(&triage::translation_of(config, &o.repo));
        let allowed = cat.allowed(&q.decision, &areas)?;
        let (answer, raw) = match triage::check_probabilities(f.kind, &allowed, &a.probabilities) {
            Ok(x) => x,
            Err(m) => {
                c.fail(format!("{what}: {m}"));
                continue;
            }
        };
        let mut body = json!({
            "item": q.item,
            "alias": o.source.alias,
            "decision": q.decision,
            "answer": answer,
            "probabilities": triage::calibrate(&raw, q.map_id.as_deref())?,
            "raw": raw,
            "map_id": q.map_id,
            "question": q.id,
            "backend": backend,
            "evaluated": false,
            "reason": a.reason,
        });
        // A closure answered true names its subject (1.4).
        let yes = answer == json!(true);
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

/// The decisions a `suggestions` document shows.
const CARDS: [&str; 5] = ["likely_fixed", "duplicate", "obsolete", "batch", "left_out"];

/// The document id of the suggestion of `decision` about `subject` in run `run`.
fn doc_id(run: &str, decision: &str, subject: &str) -> String {
    format!("sg-{}", &sha256_hex(&format!("{run}|{decision}|{subject}"))[..24])
}

fn action_label(decision: &str) -> Option<&'static str> {
    match decision {
        "likely_fixed" => Some("Check and close…"),
        "duplicate" => Some("Close as duplicate…"),
        "obsolete" => Some("Close as not planned…"),
        "batch" => Some("Launch batch…"),
        _ => None,
    }
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

/// The `suggestions` documents of `project` (5.8).
pub fn documents(conn: &Connection, project: &str) -> Result<Vec<Value>> {
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

    let mut out = Vec::new();
    for ((subject, decision), e) in latest {
        // Phase 1's cards only (5.8): the page shows the other decisions from triage phase 3.
        // A closure answered false is no card, and hides an older true one of the same kind.
        if !CARDS.contains(&decision.as_str()) || e.data["suggestion"]["answer"] == json!(false) {
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
        let mut s = e.data["suggestion"].clone();
        if decision == "batch" {
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
        let run = e.distinct["run_id"].as_str().unwrap_or_default();
        let confidence = s["probabilities"]["true"].as_f64().unwrap_or(1.0);
        let Value::Object(mut doc) = s else { continue };
        doc.insert("id".into(), json!(doc_id(run, &decision, &subject)));
        doc.insert("project".into(), json!(project));
        doc.insert("run_id".into(), json!(run));
        doc.insert("kind".into(), json!(decision));
        doc.insert("made_at".into(), json!(format_time(parse_time(&e.time)?)));
        doc.insert("confidence".into(), json!(confidence));
        if let Some(label) = action_label(&decision) {
            doc.insert("action_label".into(), json!(label));
        }
        out.push(Value::Object(doc));
    }
    out.sort_by(|a, b| (a["made_at"].as_str(), a["id"].as_str()).cmp(&(b["made_at"].as_str(), b["id"].as_str())));
    Ok(out)
}
