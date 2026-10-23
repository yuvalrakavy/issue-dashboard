# issue-dashboard — design spec

Status: **draft, chapter 1 (revision 2) for review.** Tier T2: a generic tool whose
config format is a contract that several repos and projects depend on.

## Intent

A **project** is a collection of GitHub repos (for example the Villa Rakavy home
automation system: Store, the MQTT bridges, airzoned, logmon, tracing-init). One
**tracker** installation serves any number of projects. For each project it shows one
private dashboard: the open issues of every member repo in one consolidated,
prioritised queue, with triage suggestions and a button that starts a fix or triage
session with a fitting model, effort and brief.

A repo may belong to several projects (tracing-init could be in two), and a bug can
matter differently to each. A repo can also belong to a project without being owned by
it (an upstream like Rhai).

**Settled decisions:**

| Decision | Reason |
|---|---|
| Generic code in this repo; the tracker's configuration and state in a private directory (for this installation, inside the `yuval-claude-settings` repo) | The code must name no repo; the configuration lists private repos and their layout |
| Rust for the deterministic core (snapshot, translation, write-back, config check, `explain`) | Matches the fleet's toolchain; the config becomes typed structs checked with `deny_unknown_fields` |
| The judgement parts (triage, briefs, dispatch) are Claude sessions, not code here | They are judgement, and a session already has `gh`, git and model routing |
| Each project's dashboard is a private claude.ai Artifact with a `db` | Works on a phone; the page never needs GitHub access |
| A launch button wakes the dispatcher through `comments.sendToClaude` | Probed 2026-10-22: a press woke the watching session within seconds |
| **GitHub stays fully usable on its own.** The tracker reads it, and writes back only through each repo's own label names | Anyone working only in GitHub, a person or a session, must see a true picture |
| The canonical axes are fixed by the tool | Project-wide aggregation (flow, aging, ordering across repos) needs every repo on the same axes |
| Priority is per project; the tracker is the record of each project's view | A bug that is minor in one project can be a blocker in another |

**Non-goals:** a multi-user tool; tracking anything but GitHub issues; replacing
GitHub as the record of the issues themselves.

**Requirement under design (2026-10-23):** some or all of a project's repos may be **open
source with the owner as maintainer**, so issues arrive from people outside the project.
Its design is a chapter of its own, not yet written; chapters 1-3 must not assume every
issue author is the owner.

## Chapters

1. Configuration, translation and priority — **approved 2026-10-23**
2. **The snapshot: what is read, what is derived, the JSON it writes** ← draft
3. **Upstream forks** ← draft
4. The dashboard page and its database
5. Ordering and the priority suggestion
6. Launching sessions: the dispatcher, routing, briefs, launch recipes
7. The CLI: label sync, config check, `explain`, plan/apply
8. Documentation and skills

---

## 1. Configuration, translation and priority

### 1.1 Three layers

| Layer | Where | Holds | Test for belonging here |
|---|---|---|---|
| **Repo registry** | `<root>/repos.toml` | one entry per repo, shared by every project that includes it: label translation, local checkout path, launch recipe, docs for briefs | true of the repo whichever project includes it |
| **Project config** | `<root>/projects/<project>.toml` | the project's name, its member repos and its view of each (filter, extra issues, read-only, write policy), time limits, routing, the dashboard link | a choice this project makes |
| **Repo file** (optional) | `.issue-dashboard.toml` at the root of a repo you own | the repo's own areas, launch recipe and docs | changes in the same commit as the repo's code |

`<root>` is the tracker directory, given by `--root` or `$ISSUE_DASHBOARD_ROOT`. It also
holds the tracker's state (1.6) and audit log (1.7), and is kept in git.

A repo file is optional because some member repos cannot carry one (an upstream you do
not own). When a key is set both in the repo file and in the registry, **the registry
wins**, and the config check reports the disagreement so it does not go unnoticed.

### 1.2 The canonical model

| Axis | Values | On one issue | Kind |
|---|---|---|---|
| `type` | `bug`, `todo`, `decision`, `docs` | exactly one, else *untriaged* | true of the issue anywhere |
| `area` | the repo's own area names | at least one, if the repo declares any | true of the issue anywhere |
| `modifier` | `needs_decision`, `in_progress`, `self_contained`, `partial`, `umbrella` | any number | true of the issue anywhere |
| `priority` | `p0`, `p1`, `p2`, `p3` | exactly one, else *untriaged* (unless `umbrella`) | **per project**, see 1.5 |

`umbrella` marks a tracker or umbrella issue (Store #71, #81): shown, never prioritised,
ordered or aged.

### 1.3 Translation

A registry entry translates the repo's labels to the canonical model:

```toml
# <root>/repos.toml

# The default names, used by any repo that does not override a value.
[defaults.translate]
type.bug      = ["bug"]
type.todo     = ["todo"]
type.decision = ["design-question"]
type.docs     = ["docs"]
priority.p0   = ["p0"]
priority.p1   = ["p1"]
priority.p2   = ["p2"]
priority.p3   = ["p3"]
modifier.needs_decision = ["needs-decision"]
modifier.in_progress    = ["InProgress"]
modifier.self_contained = ["self-contained"]
modifier.partial        = ["partial"]
modifier.umbrella       = ["umbrella"]

[repo."yuvalrakavy/Store"]
local_path = "~/Documents/Projects/Store"
launch     = "store-dev-track"
# areas, docs: from Store's own .issue-dashboard.toml

[repo."yuvalrakavy/airzoned"]
local_path     = "~/Documents/Projects/Home/airzoned"
translate.type.todo = ["todo", "enhancement"]

[repo."rhaiscript/rhai"]                   # upstream: no repo file possible
translate.type.todo  = ["enhancement", "new feature"]
translate.type.bug   = ["bug", "regression"]
translate.priority.p1 = ["priority"]
translate.area.vm    = ["vm"]              # canonical area name = its repo labels
```

Rules:

1. **A repo's translation merges over the defaults value by value.** Setting
   `translate.type.todo` replaces the default list for that one value only.
2. **Several labels may mean one value**, in either direction of reading. **When writing,
   the tracker uses the first label in the list**, so the list order states the preferred
   name.
3. **A label means at most one value in a repo**, across all axes including areas. The
   config check refuses a label listed twice, naming both places. (This is a real
   collision: the `ways-of-working` scheme uses `docs` both as a type and as an example
   area. In a repo that wants a docs area, the area takes another name.)
4. **Labels match case-insensitively**, as GitHub's do (probed, see the claims table).
5. **Areas map canonical area names to repo labels** (`translate.area.<name>`), in the same
   list shape as every other value; a repo file's `areas` list means each name maps to the
   label of the same name.
6. **A label that translates to nothing is reported, never dropped.** The issue is still
   shown, and the label appears in the repo's "untranslated labels" list.

### 1.4 A project's view of a repo

```toml
# <root>/projects/villa-rakavy.toml
schema = 1
name   = "Villa Rakavy home automation"
dashboard = "https://claude.ai/artifact/…"

[[member]]
repo = "yuvalrakavy/Store"

[[member]]
repo = "yuvalrakavy/mqtt_ac"
writes = "labels"                    # public repo: no agent comments

[[member]]
repo      = "rhaiscript/rhai"
filter    = "author:yuvalrakavy"     # GitHub issue-search syntax
also      = [912, 1047]              # plus these issues, by number
read_only = true
```

| Key | Default | Meaning |
|---|---|---|
| `repo` | required | must have a registry entry |
| `filter` | none (all issues) | GitHub issue-search qualifiers, passed to `gh` unchanged, so a filter can be tested in GitHub's search box. It may not contain `repo:`, `is:open`, `is:closed`, `is:pr` or `state:`, which the tracker sets itself |
| `also` | `[]` | issue numbers included whatever the filter says; an issue is included if it matches the filter **or** is listed here |
| `read_only` | `false` | the tracker writes nothing to this repo |
| `writes` | `labels-and-comments` | `labels` or `labels-and-comments`; ignored when read-only |

**Read-only is also forced** when the tracker's GitHub account lacks write access to the
repo (read from GitHub, not declared). The dashboard shows which of the two made a repo
read-only. Read-only stops the tracker writing; it does not stop you launching a fix
session on the repo's issues.

**Public repos:** the config check warns when a public repo (read from GitHub) allows
comments, and `allow_public_comments = true` on the member silences it.

A filter names a GitHub username, never "me", so a project means the same thing whichever
account runs the tracker.

### 1.5 Priority

An issue's priority in a project comes from the first of these that is set:

| Order | Source | Written to GitHub? |
|---|---|---|
| 1 | **Local override**: this project's priority for one issue, with a reason | never |
| 2 | **Project priority**: set in this project's dashboard | yes, unless the repo is read-only |
| 3 | **The GitHub label**, read through the repo's translation | (it is GitHub's own) |

**Write-back.** Setting a project priority removes the repo's old priority label from the
issue and adds the new one, using the first translated name (1.3 rule 2). If that label
does not exist in the repo it is created. No other label is touched.

**A repo shared by several projects.** Each project keeps its own priority. The label
written to GitHub is the **highest project priority any project gives the issue**
(overrides excluded), so the label means "the most urgent any project needs this" and
does not flip as projects re-prioritise. A dashboard whose own priority differs shows
"GitHub shows p1, set by another project".

**The tracker writes on a change, never re-asserts.** A priority label is written only when
a project priority changes (set, changed, or cleared so that the shared maximum moves).
A later run never rewrites the label just because it differs from the tracker's view, so
an edit someone makes on GitHub stands until a project changes its priority again.
Clearing every project priority writes nothing: the label is left as it is.

**Edits made on GitHub.** The next snapshot reads every priority label change.

- **In a repo that belongs to one project**, a GitHub-side change is adopted as that
  project's priority. When both sides changed between two snapshots, the more recent change
  wins, by the timestamps of the labelled/unlabelled events in the issue's timeline, and the
  dashboard notes which change replaced which.
- **In a shared repo**, a GitHub-side change is adopted by no project automatically, since
  it cannot say which project it speaks for. Each dashboard shows "GitHub changed to p2" and
  you accept or ignore it per project.

**An override does not go quietly out of date.** When the priority beneath an override
changes, the dashboard shows both ("override p0; GitHub now p1") until you keep or drop
the override.

**A blocker raises what it blocks automatically.** An issue that blocks another inherits
that issue's priority for ordering (chapter 4), so "a low-priority Rhai bug blocks my Store
work" needs no override when the link exists.

### 1.6 Where each project's view is recorded

The tracker's state lives in `<root>/state/<project>/` and is committed to git: project
priorities, overrides with their reasons, accepted and rejected suggestions, and the
override log the triage agent learns from. The dashboard's database is the working copy a
person edits; each tracker run reads its changes into the state files first, then writes
the state back to the database. Those writes are pinned to the version the run read, so an
edit you make during a run is not overwritten: the pinned write fails and the next run
picks the edit up. Keeping the state in files is what lets one project's run
see another project's priorities for the shared-repo rule in 1.5.

### 1.7 Writing to GitHub safely

Every write to GitHub goes through **plan, then apply**:

1. A run first computes every write it would make: issue, old labels, new labels, comment
   text, and the reason.
2. **A dry run is forced** on a repo's first run after it joins a project, and after its
   translation changes. The plan is shown on the dashboard and applied only once you
   approve it.
3. **A cap per run:** a plan with more writes than `max_writes_per_run` (default 10, set in
   the project config) waits for your approval.
4. **Every applied write is logged** in `<root>/audit/` (one line per write, with the run
   id), and `issue-dashboard undo <run id>` reverses a run. Undo restores a label only where
the issue still carries what that run wrote; where someone has changed it since, undo
skips that issue and reports it, rather than overwriting a later edit.

Before applying each label write, the tracker re-reads the issue's labels and skips the
write if the target state is already there, so two projects writing the same fact do not
fight.

### 1.8 Issue identity and links

Every issue is `owner/repo#N` in the tracker, the state files and the database. Links
between issues come from **GitHub's own sub-issue and blocked-by relationships first**,
and from `blocked by #N` / `blocked by owner/repo#N` in an issue's body or comments as a
fallback (probe, 2026-10-23: the native relationships are available on personal repos and
unused so far; see chapter 2). A link to a repo in no project is kept and shown as
external.

### 1.9 The config check and `explain`

`issue-dashboard check` refuses, naming the file and key:

- an unknown key anywhere (`deny_unknown_fields`);
- a `schema` the tool does not know;
- a label under two values in one repo (1.3 rule 3);
- a member with no registry entry, or two members naming the same repo;
- a `filter` containing a qualifier the tracker sets itself (1.4);
- a `launch` naming no recipe.

It reports, without refusing: a repo `gh` cannot read; a translated label missing from the
repo; labels on open issues that translate to nothing; a key set differently in the repo
file and the registry; a public repo allowed to receive comments without
`allow_public_comments`; a filter that matches no issue.

`issue-dashboard explain <owner/repo#N> --project <name>` prints how each value was
reached: which label translated to which type and area, whether the filter or `also`
included the issue, which priority source won (and what the others say), and why the repo
is or is not read-only.

---

## 2. The snapshot

The snapshot is the deterministic half of every tracker run: it reads GitHub and the local
checkouts, applies chapter 1's rules, and writes one JSON document per project. No
judgement happens here; the triage session (chapter 5) reads the snapshot and adds the
judgement on top.

### 2.1 What is read

**From GitHub, per member repo** (GraphQL, through `gh api graphql`):

| Read | Used for |
|---|---|
| repo: visibility, the account's permission (`viewerPermission`), the label list | forced read-only (1.4), the public-comment warning, label sync |
| issues matching the member's view: the tracker's own `repo:` and `is:issue` qualifiers + `filter`, plus each issue in `also` | the issue set |
| per issue: number, title, state and close reason, author, assignees, labels, created / updated / closed times, comment count, body | translation, status, aging, the structured filing block |
| per issue: the timeline's labelled / unlabelled events for priority labels | the "more recent change wins" rule (1.5) |
| per issue: `parent`, `subIssues`, `blockedBy`, `blocking` | the link graph (1.8), across repos |
| comments, only for issues whose body or comments may hold a text link or a filing block | the text-link fallback |

**Closed issues are read too**, back to a window set in the project config
(`history_days`, default 180), because the flow metrics need what closed, not only what
is open.

**From each local checkout** (when the registry gives a `local_path`): `git log` of the
default branch, for commits that mention an issue (`#N`, `owner/repo#N`, `fixes #N`), and
the dependency manifests for chapter 3.

### 2.2 Incremental by default

A run asks GitHub only for issues updated since the previous run (`updated:>` the stored
time, per repo, minus a five-minute overlap so a change landing during the last run is
not missed). That keeps a run cheap enough to repeat every few minutes, which keeps the
dashboard fresh and shrinks the window in which a GitHub edit and a tracker edit collide.

A **full read** replaces the incremental one: on a repo's first run, after its filter or
`also` changes (an incremental read cannot notice an issue that now falls out of the
filter), after the repo's translation changes, and once a day. The daily read also catches
what does not move an issue's update time: an issue transferred or deleted, and a label
renamed in the repo's settings.

A number in `also` that is a pull request, not an issue, or that does not exist, is
reported by the config check and skipped, never silently treated as an issue.

### 2.3 What is derived

| Derived value | Rule |
|---|---|
| canonical type, areas, modifiers | translation (1.3), with untranslated labels listed per repo |
| priority and its source | the three sources in order (1.5), plus every source's value for `explain` |
| status | `untriaged` (an axis missing), `needs-decision`, `in-progress`, `blocked` (an open blocker), else `ready`; `umbrella` issues carry `umbrella` instead |
| age and idle time | from created and updated times |
| time-limit breach | idle time against the project's limit for the issue's priority |
| link graph | native relationships first, text links second, with the source of each edge recorded |
| "waiting on you" | issues with `needs-decision`, plans awaiting approval (1.7), overrides gone out of date (1.5), GitHub-side changes awaiting a decision in a shared repo (1.5) |
| commits mentioning an open issue | from `git log`; the input to the triage session's "fixed but still open?" check |
| flow | per week: opened, closed (completed / not planned), net, per repo and for the project |

### 2.4 What is written

`<root>/state/<project>/snapshot.json`, replaced on each run, and a copy per day under
`<root>/state/<project>/history/YYYY-MM-DD.json` so metrics can be recomputed and compared
over time. The document is sorted (repos by name, issues by number) so two runs over the
same data produce identical bytes and a git diff shows only what changed.

```json
{
  "schema": 1,
  "project": "Villa Rakavy home automation",
  "taken_at": "2026-10-23T08:00:00Z",
  "repos": [
    {
      "repo": "yuvalrakavy/Store",
      "read_at": "2026-10-23T08:00:00Z",
      "read": "incremental",
      "error": null,
      "visibility": "private",
      "read_only": { "value": false, "why": null },
      "untranslated_labels": [],
      "issues": [
        {
          "id": "yuvalrakavy/Store#403",
          "title": "DeleteStore parks in remove_dir_all openat and the whole server stops answering",
          "state": "open",
          "type": "bug",
          "areas": ["kernel"],
          "modifiers": [],
          "priority": { "value": "p0", "source": "label", "label": "p0", "project": null, "override": null },
          "status": "ready",
          "created_at": "…", "updated_at": "…", "idle_days": 1,
          "limit_breached": false,
          "links": { "blocked_by": [], "blocks": [], "parent": null, "children": [] },
          "commits": []
        }
      ]
    }
  ],
  "waiting_on_you": [],
  "flow": []
}
```

### 2.5 When a read fails

A repo that cannot be read (network, rate limit, a renamed repo) records `error` and keeps
its **last good issues, marked with the time they were read**. The dashboard shows that
repo as stale rather than empty. A failed read never removes issues, because an empty repo
and an unreadable one must not look the same. The run still writes the other repos.

### 2.6 Testing seam

All GitHub access goes through one `GitHub` trait, with two implementations: the real one,
and a replay one that serves **recorded real responses** captured from Store, airzoned and
Rhai. Translation, filtering, status, priority resolution and the incremental merge are
tested against those recordings, so the tests see GitHub's real label spellings, missing
fields and case differences rather than what a hand-written fixture assumes.

---

## 3. Upstream forks

A member repo sometimes builds against a **fork** of an upstream project, because a fix it
needs is not released yet. This chapter makes the tracker notice when it can go back to a
released version, and the risk of staying on the fork in the meantime.

### 3.1 Finding forks

For each member repo with a local checkout, the tracker reads its dependency manifests and
lists every dependency served from a git repository instead of the package registry.
Cargo is the first reader (`Cargo.toml`, including `[patch.*]` sections, and
`Cargo.lock` for the resolved revision); others (npm, pip) can be added as further readers
without changing anything else.

Which git sources count as forks is configuration, not code: the registry lists the
owners whose repos are forks (`[forks] owners = ["yuvalrakavy"]`), and each fork may name
its upstream when the name differs (`[fork."yuvalrakavy/rumqtt"] upstream =
"bytebeamio/rumqtt"`). Otherwise the upstream is the repo the fork was created from, as
GitHub reports it.

### 3.2 Linking a fork to its upstream fix

In this order:

1. issue and pull-request links in the comment block directly above the dependency line
   (Store's do this today: the rhai patch names rhaiscript/rhai#1188 and #1189);
2. an entry in the registry (`[fork."yuvalrakavy/rhai"] fixes = ["rhaiscript/rhai#1189"]`);
3. otherwise, the commits on the fork's pinned revision that are not in the upstream's
   history, each checked for an upstream pull request containing it.

### 3.3 The states

| State | Condition | The dashboard shows |
|---|---|---|
| **Not offered upstream** | a fork commit is in no upstream pull request | "rumqttc: 1 local fix never sent upstream", with a launch button for a session that prepares the pull request |
| **Under review** | the fix pull request is open | the pull request and how long it has been open |
| **Merged, unreleased** | merged, and no release tag contains the merge commit | "merged on …, waiting for a release" |
| **Released** | a release tag contains the merge commit **and** that version is published on the package registry | **"Can go back to rhai 1.27.0: it contains rhai#1189"**, with a launch button |
| **Back on the release** | the manifest no longer uses the fork | removed from the list |

"Contains" is checked with GitHub's compare API between the merge commit and the release
tag. A release counts only once the registry has it, because a tag alone cannot be built
against.

### 3.4 The risk of staying on a fork

A fork is pinned to an old base, so every upstream release after that base carries fixes
the fork lacks. For each fork the dashboard also shows **how far behind its base is**
("based on 1.26.1; latest is 1.27.0, 1 release behind"), and flags a release that is
marked as a security fix (a GitHub security advisory against the package).

### 3.5 What a launch carries

The "go back" brief names the consumer repo, the manifest line and its comment, the version
change it implies (going back is usually also an upgrade, as with rhai 1.26.1 to 1.27.0),
the upstream release notes to read, and the tests the comment says must stay green. The
"send it upstream" brief names the fork commit, the upstream repo, and that repo's own
contribution rules, read from the upstream's `CONTRIBUTING` file.

The first real case: yuvalrakavy/Store#410, filed 2026-10-23 when this check was done by
hand.

## Claims checked

| Claim | Status | Evidence |
|---|---|---|
| Open issues only in Store (67) and airzoned (14) among the 11 active repos | confirmed | `gh issue list` per repo, 2026-10-23 |
| airzoned uses GitHub default labels: 3 of 14 open issues carry `bug`, 1 `enhancement` | confirmed | `gh issue list -R yuvalrakavy/airzoned` |
| Rhai's labels are `bug, duplicate, enhancement, help wanted, question, wontfix, new feature, priority, regression, docs, vm` | confirmed | `gh label list -R rhaiscript/rhai`, 2026-10-23 |
| `yuvalrakavy/rhai` (the fork) has issues disabled, so Rhai is tracked as `rhaiscript/rhai` | confirmed | `gh repo list` (`hasIssuesEnabled: false`) |
| No open Rhai issue is authored by `yuvalrakavy`, so the example filter matches nothing today and the `also` numbers are placeholders | confirmed | `gh issue list -R rhaiscript/rhai --author yuvalrakavy` returned none |
| GitHub label names are unique case-insensitively | confirmed | probe on this repo: `BUG` beside `bug`, and `probe-case` beside `Probe-Case`, both refused as "already exists" |
| Seven of the 11 active repos are public | confirmed | `gh repo list` |
| GitHub's native sub-issues and blocked-by links work on personal repos | confirmed | probe 2026-10-23: the REST `sub_issues` and `dependencies/blocked_by` endpoints and the GraphQL `parent`, `subIssues`, `blockedBy`, `blocking` fields all answer on yuvalrakavy/Store (all empty: unused so far) |
| Native links can cross repos | **unverified** | not probed: needs two test issues; probe before building 2.1 |
| Repo-level issue types are not available on personal repos | confirmed | GraphQL `issueTypes` returns `null` on yuvalrakavy/Store |
| 7 Store issues contain "blocked by" in their text, none of them open | confirmed | `gh search issues --repo yuvalrakavy/Store "blocked by"` |
| Store patches `rhai` and `rumqttc` to forks | confirmed | `Store/Cargo.toml:85` and `:96` |
| rhaiscript/rhai#1189 merged 2026-10-17 (merge commit `aaf90906`), contained in v1.27.0, which is crates.io's latest | confirmed | `gh pr list`, `gh api …/compare/aaf90906...v1.27.0` → `identical`, crates.io API |
| No pull request by `yuvalrakavy` on bytebeamio/rumqtt | confirmed | `gh pr list -R bytebeamio/rumqtt --author @me` returned none |
| Renaming a label in a repo's settings does not change its issues' update time | believed | not probed; the daily full read covers it either way |
| The forks' upstreams are bytebeamio/rumqtt and rhaiscript/rhai | confirmed | `gh repo view … --json parent` on both forks |
| `sendToClaude` wakes a watching session | confirmed | probe artifact, 2026-10-22 |
