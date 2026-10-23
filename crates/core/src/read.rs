//! The GraphQL reads of chapter 2 and 4.4, and their translation into what an `observed`
//! event records. Every read is a query: phase 1 writes nothing to GitHub (10.0).
//!
//! A response that carries an error fails the read, except a `NOT_FOUND` where a missing item
//! is an answer (a node gone, a number that is neither an issue nor a pull request).

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};
use serde_json::{Map, Value, json};

use crate::canonical::sha256_hex;
use crate::github::{GitHub, Request};
use crate::observe::ItemKind;
use crate::root::parse_time;
use crate::{Error, Result};

/// GitHub's search returns at most this many results for one query.
pub const SEARCH_CAP: u64 = 1000;

/// An issue body is kept up to this many characters (the page's Details; a doc is 256 KiB).
pub const BODY_CAP: usize = 8000;

/// The default page size; tests use a smaller one so the recordings paginate.
pub const DEFAULT_PAGE: u32 = 100;

const LINKED: &str = "fragment Linked on Issue { id number state repository { nameWithOwner } }";

const ISSUE_FIELDS: &str = "fragment IssueFields on Issue {
  id number title state stateReason author { login } authorAssociation
  assignees(first: 20) { nodes { login } }
  labels(first: 50) { totalCount nodes { name } }
  milestone { title dueOn }
  createdAt updatedAt closedAt body
  comments(last: 50) { totalCount nodes { author { login } authorAssociation createdAt } }
  reactionGroups { content reactors { totalCount } }
  parent { ...Linked }
  subIssues(first: 50) { nodes { ...Linked } }
  blockedBy(first: 50) { nodes { ...Linked } }
  blocking(first: 50) { nodes { ...Linked } }
  timelineItems(itemTypes: [LABELED_EVENT, UNLABELED_EVENT], last: 50) { nodes { ... on LabeledEvent { id } ... on UnlabeledEvent { id } } }
  repository { nameWithOwner }
}";

const PR_FIELDS: &str = "fragment PrFields on PullRequest {
  id number title state isDraft reviewDecision mergeable author { login } authorAssociation
  assignees(first: 20) { nodes { login } }
  labels(first: 50) { totalCount nodes { name } }
  milestone { title dueOn }
  createdAt updatedAt closedAt body
  comments(last: 50) { totalCount nodes { author { login } authorAssociation createdAt } }
  reactionGroups { content reactors { totalCount } }
  commits(last: 1) { nodes { commit { oid statusCheckRollup { state } } } }
  latestOpinionatedReviews(first: 50) { nodes { state commit { oid } } }
  closingIssuesReferences(first: 20) { nodes { ...Linked } }
  timelineItems(itemTypes: [LABELED_EVENT, UNLABELED_EVENT], last: 50) { nodes { ... on LabeledEvent { id } ... on UnlabeledEvent { id } } }
  repository { nameWithOwner }
}";

/// The query with the fragments it spreads: GitHub refuses a document that defines a fragment
/// it never uses. Both field fragments spread `Linked`.
fn with_fragments(query: &str) -> &'static str {
    let mut q = query.to_string();
    for (spread, fragment) in [("...IssueFields", ISSUE_FIELDS), ("...PrFields", PR_FIELDS)] {
        if query.contains(spread) {
            q = format!("{q}\n{fragment}");
        }
    }
    Box::leak(format!("{q}\n{LINKED}").into_boxed_str())
}

const REPO: &str = "query($owner: String!, $name: String!) {
  repository(owner: $owner, name: $name) {
    id nameWithOwner visibility viewerPermission defaultBranchRef { name }
    labels(first: 100) { totalCount nodes { name } }
  }
}";

const SEARCH: &str = "query($q: String!, $first: Int!, $after: String) {
  search(query: $q, type: ISSUE, first: $first, after: $after) {
    issueCount
    pageInfo { hasNextPage endCursor }
    nodes {
      __typename
      ... on Issue { id number closedAt repository { nameWithOwner } }
      ... on PullRequest { id number closedAt repository { nameWithOwner } }
    }
  }
}";

const NUMBER: &str = "query($owner: String!, $name: String!, $number: Int!) {
  repository(owner: $owner, name: $name) {
    issueOrPullRequest(number: $number) {
      __typename
      ... on Issue { id number closedAt }
      ... on PullRequest { id number closedAt }
    }
  }
}";

fn issues_since_query() -> &'static str {
    static Q: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    Q.get_or_init(|| {
        with_fragments(
            "query($owner: String!, $name: String!, $since: DateTime!, $first: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    issues(first: $first, after: $after, filterBy: {since: $since}, orderBy: {field: UPDATED_AT, direction: ASC}) {
      pageInfo { hasNextPage endCursor }
      nodes { ...IssueFields }
    }
  }
}",
        )
    })
}

fn prs_since_query() -> &'static str {
    static Q: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    Q.get_or_init(|| {
        with_fragments(
            "query($owner: String!, $name: String!, $first: Int!, $after: String) {
  repository(owner: $owner, name: $name) {
    pullRequests(first: $first, after: $after, orderBy: {field: UPDATED_AT, direction: DESC}) {
      pageInfo { hasNextPage endCursor }
      nodes { ...PrFields }
    }
  }
}",
        )
    })
}

fn nodes_query() -> &'static str {
    static Q: std::sync::OnceLock<&'static str> = std::sync::OnceLock::new();
    Q.get_or_init(|| {
        with_fragments(
            "query($ids: [ID!]!) {
  nodes(ids: $ids) {
    __typename
    ... on Issue { ...IssueFields }
    ... on PullRequest { ...PrFields }
  }
}",
        )
    })
}

/// A repo as read: the input to forced read-only (1.4) and label sync.
#[derive(Debug, Clone, PartialEq)]
pub struct RepoInfo {
    pub node_id: String,
    pub name_with_owner: String,
    pub visibility: String,
    pub viewer_permission: String,
    pub labels: Vec<String>,
    pub default_branch: Option<String>,
}

impl RepoInfo {
    /// The content of the repo's `observed` event.
    pub fn content(&self) -> Value {
        let mut c = json!({
            "visibility": self.visibility.to_lowercase(),
            "viewer_permission": self.viewer_permission,
            "labels": self.labels,
        });
        if let Some(b) = &self.default_branch {
            c["default_branch"] = json!(b);
        }
        c
    }
}

/// An item a membership read returned: ids only (2.2).
#[derive(Debug, Clone, PartialEq)]
pub struct Ref {
    pub node_id: String,
    pub kind: ItemKind,
    pub number: u64,
    pub closed_at: Option<DateTime<Utc>>,
}

/// An issue or pull request as read in full.
#[derive(Debug, Clone, PartialEq)]
pub struct Item {
    pub node_id: String,
    pub kind: ItemKind,
    /// owner/name of the repo GitHub says it is in now (a transfer changes it).
    pub repo: String,
    pub number: u64,
    pub alias: String,
    pub updated_at: DateTime<Utc>,
    /// What the `observed` event records (`schemas/event-observed.schema.json`).
    pub content: Value,
}

/// Reads GitHub through a [`GitHub`], `page` items per request.
pub struct Reader<'a> {
    gh: &'a dyn GitHub,
    page: u32,
}

fn split(repo: &str) -> Result<(&str, &str)> {
    repo.split_once('/')
        .ok_or_else(|| Error::GitHub(format!("{repo} is not owner/name")))
}

fn str_at<'v>(v: &'v Value, path: &[&str]) -> Option<&'v str> {
    path.iter().try_fold(v, |v, k| v.get(k))?.as_str()
}

fn nodes<'v>(v: &'v Value, field: &str) -> impl Iterator<Item = &'v Value> {
    v.get(field)
        .and_then(|c| c.get("nodes"))
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter(|n| !n.is_null())
}

/// A node id as GitHub returned it, checked against the document-id pattern (5.3): a legacy
/// base64 id would otherwise reach a database document id.
fn node_id(v: &Value) -> Result<String> {
    let id = v
        .get("id")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::GitHub(format!("a node without an id: {v}")))?;
    if id.is_empty() || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-') {
        return Err(Error::GitHub(format!(
            "node id {id} is not in the current format (was X-Github-Next-Global-ID sent?)"
        )));
    }
    Ok(id.to_string())
}

fn time(v: &Value, field: &str) -> Result<Option<DateTime<Utc>>> {
    match v.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) => Ok(Some(parse_time(s)?)),
        Some(other) => Err(Error::GitHub(format!("{field} is not a time: {other}"))),
    }
}

/// Fails on any error in the document except a `NOT_FOUND` whose path `allowed` accepts.
fn check(name: &str, resp: &Value, allowed: impl Fn(&[Value]) -> bool) -> Result<()> {
    if let Some(errors) = resp.get("errors").and_then(Value::as_array) {
        let fatal: Vec<String> = errors
            .iter()
            .filter(|e| {
                let not_found = e.get("type").and_then(Value::as_str) == Some("NOT_FOUND");
                let path = e
                    .get("path")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                !(not_found && allowed(path))
            })
            .map(|e| {
                e.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("an error without a message")
                    .to_string()
            })
            .collect();
        if !fatal.is_empty() {
            return Err(Error::GitHub(format!("{name}: {}", fatal.join("; "))));
        }
    }
    match resp.get("data") {
        Some(Value::Object(_)) => Ok(()),
        _ => Err(Error::GitHub(format!("{name}: the response has no data"))),
    }
}

fn kind_of(v: &Value) -> Result<ItemKind> {
    match v.get("__typename").and_then(Value::as_str) {
        Some("Issue") => Ok(ItemKind::Issue),
        Some("PullRequest") => Ok(ItemKind::PullRequest),
        other => Err(Error::GitHub(format!("unexpected node type {other:?}"))),
    }
}

fn linked(v: &Value, out: &mut Map<String, Value>) -> Result<String> {
    let id = node_id(v)?;
    if let (Some(repo), Some(n)) = (
        str_at(v, &["repository", "nameWithOwner"]),
        v.get("number").and_then(Value::as_u64),
    ) {
        out.insert(
            id.clone(),
            json!({
                "alias": format!("{repo}#{n}"),
                "state": v.get("state").and_then(Value::as_str).unwrap_or("OPEN").to_lowercase(),
            }),
        );
    }
    Ok(id)
}

/// Translates one issue or pull request node into an [`Item`].
pub fn item(v: &Value) -> Result<Item> {
    let kind = kind_of(v)?;
    let id = node_id(v)?;
    let repo = str_at(v, &["repository", "nameWithOwner"])
        .ok_or_else(|| Error::GitHub(format!("{id}: no repository")))?
        .to_string();
    let number = v
        .get("number")
        .and_then(Value::as_u64)
        .ok_or_else(|| Error::GitHub(format!("{id}: no number")))?;
    let updated_at = time(v, "updatedAt")?.ok_or_else(|| Error::GitHub(format!("{id}: no updatedAt")))?;
    if v.get("labels")
        .and_then(|l| l.get("totalCount"))
        .and_then(Value::as_u64)
        .unwrap_or(0)
        > 50
    {
        return Err(Error::GitHub(format!(
            "{repo}#{number} has more than 50 labels; not read"
        )));
    }

    let mut linked_items = Map::new();
    let ids = |field: &str, linked_items: &mut Map<String, Value>| -> Result<Vec<String>> {
        nodes(v, field).map(|n| linked(n, linked_items)).collect()
    };
    let links = match kind {
        ItemKind::Issue => json!({
            "parent": match v.get("parent") {
                Some(p) if !p.is_null() => Value::String(linked(p, &mut linked_items)?),
                _ => Value::Null,
            },
            "sub_issues": ids("subIssues", &mut linked_items)?,
            "blocked_by": ids("blockedBy", &mut linked_items)?,
            "blocking": ids("blocking", &mut linked_items)?,
        }),
        _ => json!({"parent": null, "sub_issues": [], "blocked_by": [], "blocking": []}),
    };
    let lower = |field: &str| v.get(field).and_then(Value::as_str).map(str::to_lowercase);
    let mut reactions = Map::new();
    for g in v.get("reactionGroups").and_then(Value::as_array).into_iter().flatten() {
        let n = g
            .get("reactors")
            .and_then(|r| r.get("totalCount"))
            .and_then(Value::as_u64)
            .unwrap_or(0);
        if n > 0
            && let Some(c) = g.get("content").and_then(Value::as_str)
        {
            reactions.insert(c.to_string(), json!(n));
        }
    }
    let body = v.get("body").and_then(Value::as_str).unwrap_or("");
    // Kept for Details (5.x), cut at BODY_CAP characters; the hash is of the whole body.
    let trimmed = body.trim();
    let kept: String = trimmed.chars().take(BODY_CAP).collect();
    let body_truncated = kept.len() < trimmed.len();
    let mut content = json!({
        "state": lower("state").unwrap_or_else(|| "open".into()),
        "title": v.get("title").and_then(Value::as_str).unwrap_or(""),
        "labels": nodes(v, "labels").filter_map(|l| l.get("name").and_then(Value::as_str)).collect::<Vec<_>>(),
        "assignees": nodes(v, "assignees").filter_map(|a| a.get("login").and_then(Value::as_str)).collect::<Vec<_>>(),
        "author": str_at(v, &["author", "login"]).unwrap_or("ghost"),
        "author_association": v.get("authorAssociation").and_then(Value::as_str).unwrap_or("NONE"),
        "milestone": match v.get("milestone") {
            Some(m) if !m.is_null() => json!({"title": m.get("title").and_then(Value::as_str).unwrap_or(""), "due_on": m.get("dueOn")}),
            _ => Value::Null,
        },
        "created_at": v.get("createdAt"),
        "closed_at": v.get("closedAt"),
        "comment_count": v.get("comments").and_then(|c| c.get("totalCount")).and_then(Value::as_u64).unwrap_or(0),
        "comments": nodes(v, "comments").map(|c| json!({
            "author": str_at(c, &["author", "login"]).unwrap_or("ghost"),
            "association": c.get("authorAssociation").and_then(Value::as_str).unwrap_or("NONE"),
            "at": c.get("createdAt"),
        })).collect::<Vec<_>>(),
        "reactions": reactions,
        "body_hash": if body.is_empty() { Value::Null } else { json!(sha256_hex(body)) },
        "body": if kept.is_empty() { Value::Null } else { json!(kept) },
        "body_truncated": body_truncated,
        "links": links,
        "timeline_ids": nodes(v, "timelineItems").filter_map(|t| t.get("id").and_then(Value::as_str)).collect::<Vec<_>>(),
    });
    if kind == ItemKind::Issue {
        content["state_reason"] = lower("stateReason").map_or(Value::Null, Value::String);
    }
    if kind == ItemKind::PullRequest {
        let head = nodes(v, "commits").next().and_then(|c| c.get("commit"));
        let closes = nodes(v, "closingIssuesReferences")
            .map(|n| linked(n, &mut linked_items))
            .collect::<Result<Vec<_>>>()?;
        content["pull_request"] = json!({
            "draft": v.get("isDraft").and_then(Value::as_bool).unwrap_or(false),
            "review_decision": v.get("reviewDecision"),
            "mergeable": v.get("mergeable").and_then(Value::as_str).unwrap_or("UNKNOWN"),
            "head": head.and_then(|c| c.get("oid")),
            "checks": head.and_then(|c| c.get("statusCheckRollup")).and_then(|s| s.get("state")),
            "closes": closes,
        });
        // A `null` (the field is there but GitHub did not answer it) is no list: keep the aggregate.
        if v.get("latestOpinionatedReviews").is_some_and(|r| !r.is_null()) {
            content["pull_request"]["changes_requested_on"] = json!(
                nodes(v, "latestOpinionatedReviews")
                    .filter(|r| r.get("state").and_then(Value::as_str) == Some("CHANGES_REQUESTED"))
                    .filter_map(|r| str_at(r, &["commit", "oid"]))
                    .collect::<BTreeSet<_>>()
            );
        }
    }
    content["linked"] = Value::Object(linked_items);
    Ok(Item {
        node_id: id,
        kind,
        alias: format!("{repo}#{number}"),
        repo,
        number,
        updated_at,
        content,
    })
}

impl<'a> Reader<'a> {
    pub fn new(gh: &'a dyn GitHub, page: u32) -> Reader<'a> {
        Reader { gh, page: page.max(1) }
    }

    fn ask(&self, name: &'static str, query: &'static str, variables: Value) -> Result<Value> {
        self.gh.graphql(&Request { name, query, variables })
    }

    pub fn repo(&self, repo: &str) -> Result<RepoInfo> {
        let (owner, name) = split(repo)?;
        let resp = self.ask("repo", REPO, json!({"owner": owner, "name": name}))?;
        check("repo", &resp, |_| false)?;
        let r = &resp["data"]["repository"];
        if r.is_null() {
            return Err(Error::GitHub(format!("{repo}: no such repository")));
        }
        if r["labels"]["totalCount"].as_u64().unwrap_or(0) > 100 {
            return Err(Error::GitHub(format!("{repo} has more than 100 labels; not read")));
        }
        Ok(RepoInfo {
            node_id: node_id(r)?,
            name_with_owner: r["nameWithOwner"].as_str().unwrap_or(repo).to_string(),
            visibility: r["visibility"].as_str().unwrap_or("PRIVATE").to_string(),
            viewer_permission: r["viewerPermission"].as_str().unwrap_or("NONE").to_string(),
            labels: nodes(r, "labels")
                .filter_map(|l| l["name"].as_str().map(str::to_string))
                .collect(),
            default_branch: str_at(r, &["defaultBranchRef", "name"]).map(str::to_string),
        })
    }

    /// Every item a search returns, ids only, across all pages. A search matching more than
    /// GitHub returns ([`SEARCH_CAP`]) is a failed read, never a silently short set.
    pub fn search(&self, q: &str, repo: &str) -> Result<Vec<Ref>> {
        let mut out = Vec::new();
        let mut after = Value::Null;
        loop {
            let resp = self.ask("search", SEARCH, json!({"q": q, "first": self.page, "after": after}))?;
            check("search", &resp, |_| false)?;
            let s = &resp["data"]["search"];
            let count = s["issueCount"].as_u64().unwrap_or(0);
            if count > SEARCH_CAP {
                return Err(Error::GitHub(format!(
                    "the search {q:?} matches {count} items, more than GitHub returns ({SEARCH_CAP}); narrow the member's filter"
                )));
            }
            for n in s["nodes"].as_array().into_iter().flatten().filter(|n| !n.is_null()) {
                // Search is by repo name; an item transferred out is no longer the repo's.
                if !str_at(n, &["repository", "nameWithOwner"]).is_some_and(|r| r.eq_ignore_ascii_case(repo)) {
                    continue;
                }
                out.push(Ref {
                    node_id: node_id(n)?,
                    kind: kind_of(n)?,
                    number: n["number"].as_u64().unwrap_or(0),
                    closed_at: time(n, "closedAt")?,
                });
            }
            if s["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
                return Ok(out);
            }
            after = s["pageInfo"]["endCursor"].clone();
        }
    }

    /// The issue or pull request with this number, if there is one (an `also` entry, 2.2).
    pub fn by_number(&self, repo: &str, number: u64) -> Result<Option<Ref>> {
        let (owner, name) = split(repo)?;
        let resp = self.ask(
            "number",
            NUMBER,
            json!({"owner": owner, "name": name, "number": number}),
        )?;
        check("number", &resp, |p| p.len() == 2 && p[1] == "issueOrPullRequest")?;
        let n = &resp["data"]["repository"]["issueOrPullRequest"];
        if n.is_null() {
            return Ok(None);
        }
        Ok(Some(Ref {
            node_id: node_id(n)?,
            kind: kind_of(n)?,
            number,
            closed_at: time(n, "closedAt")?,
        }))
    }

    /// Every issue updated at or after `since`, read in full, without any filter (2.2).
    pub fn issues_since(&self, repo: &str, since: DateTime<Utc>) -> Result<Vec<Item>> {
        let (owner, name) = split(repo)?;
        let mut out = Vec::new();
        let mut after = Value::Null;
        loop {
            let resp = self.ask(
                "issues_since",
                issues_since_query(),
                json!({"owner": owner, "name": name, "since": crate::root::format_time(since), "first": self.page, "after": after}),
            )?;
            check("issues_since", &resp, |_| false)?;
            let c = &resp["data"]["repository"]["issues"];
            for n in c["nodes"].as_array().into_iter().flatten().filter(|n| !n.is_null()) {
                let mut n = n.clone();
                n["__typename"] = json!("Issue");
                let it = item(&n)?;
                // GitHub filters by `since` too; this keeps a recording valid for any watermark.
                if it.updated_at >= since {
                    out.push(it);
                }
            }
            if c["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
                return Ok(out);
            }
            after = c["pageInfo"]["endCursor"].clone();
        }
    }

    /// Every pull request updated at or after `since`: pull requests have no `since` filter,
    /// so they are read newest first until a page ends before `since`.
    pub fn prs_since(&self, repo: &str, since: DateTime<Utc>) -> Result<Vec<Item>> {
        let (owner, name) = split(repo)?;
        let mut out = Vec::new();
        let mut after = Value::Null;
        loop {
            let resp = self.ask(
                "prs_since",
                prs_since_query(),
                json!({"owner": owner, "name": name, "first": self.page, "after": after}),
            )?;
            check("prs_since", &resp, |_| false)?;
            let c = &resp["data"]["repository"]["pullRequests"];
            let mut older = false;
            for n in c["nodes"].as_array().into_iter().flatten().filter(|n| !n.is_null()) {
                let mut n = n.clone();
                n["__typename"] = json!("PullRequest");
                let it = item(&n)?;
                if it.updated_at >= since {
                    out.push(it);
                } else {
                    older = true;
                }
            }
            if older || c["pageInfo"]["hasNextPage"].as_bool() != Some(true) {
                return Ok(out);
            }
            after = c["pageInfo"]["endCursor"].clone();
        }
    }

    /// Reads items by node id, `page` per request. An id GitHub does not resolve maps to
    /// `None`: deleted, or no longer visible to the account.
    pub fn nodes(&self, ids: &[String]) -> Result<BTreeMap<String, Option<Item>>> {
        let mut out = BTreeMap::new();
        for chunk in ids.chunks(self.page as usize) {
            let resp = self.ask("nodes", nodes_query(), json!({"ids": chunk}))?;
            check("nodes", &resp, |p| p.len() == 2 && p[0] == "nodes")?;
            let got = resp["data"]["nodes"]
                .as_array()
                .ok_or_else(|| Error::GitHub("nodes: no list".into()))?;
            if got.len() != chunk.len() {
                return Err(Error::GitHub(format!(
                    "nodes: asked for {} ids, got {}",
                    chunk.len(),
                    got.len()
                )));
            }
            for (id, n) in chunk.iter().zip(got) {
                let it = if n.is_null() { None } else { Some(item(n)?) };
                if let Some(it) = &it
                    && &it.node_id != id
                {
                    return Err(Error::GitHub(format!("nodes: asked for {id}, got {}", it.node_id)));
                }
                out.insert(id.clone(), it);
            }
        }
        Ok(out)
    }
}
