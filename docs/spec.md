# issue-dashboard — design spec

Status: **chapters 1-6 and §7.1 approved; the rest of chapter 7 a draft for review.** Tier T2: a generic tool whose
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

**Non-goals:** a multi-user tool (one maintainer operates it, though issues and pull
requests come from anyone); tracking anything but GitHub issues and pull requests;
replacing GitHub as the record of the issues themselves; merging pull requests.

Some or all of a project's repos may be open source with the owner as maintainer
(chapter 4), so no chapter may assume every author is the owner.

## Chapters

1. Configuration, translation and priority — approved 2026-10-23
2. The snapshot: what is read, what is derived, the JSON it writes — approved 2026-10-23
3. Upstream forks — approved 2026-10-23
4. Open source: outside authors and pull requests — approved 2026-10-23
5. The dashboard page and its database — approved 2026-10-23 (with the mockup)
6. Ordering: WSJF within the priority level, and the priority suggestion — approved 2026-10-23
7. **Launching sessions: decisions, the dispatcher, routing, briefs, launch recipes** ← draft (§7.1 approved)
8. The CLI: label sync, config check, `explain`, plan/apply
9. Documentation and skills

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
| `modifier` | `needs_decision`, `in_progress`, `self_contained`, `partial`, `umbrella`, `good_first_issue`, `help_wanted` (the last two added with chapter 4) | any number | true of the issue anywhere |
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

**Repo groups.** A group is a named list of repos, such as `bridges` or `pd-libraries`.
Groups that describe repos whatever the project (the bridges are bridges everywhere) live
in the registry; a project may add its own in its config. A repo may be in several groups.
A group name used by both the registry and a project is refused by the config check.

```toml
# <root>/repos.toml
[groups]
bridges = ["yuvalrakavy/mqtt_ac", "yuvalrakavy/mqtt_dali", "yuvalrakavy/mqtt_dmx",
           "yuvalrakavy/mqtt-hdl", "yuvalrakavy/mqtt_lutron", "yuvalrakavy/mqtt_mdns",
           "yuvalrakavy/mqtt_ynca"]
```

A member may name a group instead of a repo, which adds every repo in it, and may exclude
repos from it; the member's other keys then apply to each repo it adds:

```toml
[[member]]
group   = "bridges"
exclude = ["yuvalrakavy/mqtt_dali"]      # not tracked in this project at all
```

When a repo arrives through two members (a group and its own entry), its own entry wins.
**Excluding a repo here removes it from the project**: it is not read, counted or shown. To
keep tracking a repo but not see it, use a dashboard view (5.6).

| Key | Default | Meaning |
|---|---|---|
| `repo` or `group` | one required | a repo with a registry entry, or a group |
| `exclude` | `[]` | with `group`: repos of the group to leave out |
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
that issue's priority for ordering (chapter 6), so "a low-priority Rhai bug blocks my Store
work" needs no override when the link exists.

### 1.6 Where each project's view is recorded

The tracker's state lives in `<root>/state/<project>/` and is committed to git: project
priorities, overrides with their reasons, accepted and rejected suggestions, and the
override log the triage agent learns from. The dashboard's database is the working copy a
person edits: your actions are recorded there as new documents, which each tracker run
consumes into the state files before rewriting its own collections. The page and the
tracker write disjoint collections (5.3), so an edit made during a run is never overwritten
by it. Keeping the state in files is what lets one project's run
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
judgement happens here; the triage session (chapter 7) reads the snapshot and adds the
judgement on top.

### 2.1 What is read

**From GitHub, per member repo** (GraphQL, through `gh api graphql`):

| Read | Used for |
|---|---|
| repo: visibility, the account's permission (`viewerPermission`), the label list | forced read-only (1.4), the public-comment warning, label sync |
| issues matching the member's view: the tracker's own `repo:` and `is:issue` qualifiers + `filter`, plus each issue in `also` | the issue set |
| per issue: number, title, state and close reason, author and association, assignees, labels, milestone and its due date, created / updated / closed times, comment count, reactions, body | translation, status, aging, time criticality and outside demand (chapter 6), the structured filing block |
| per issue: the author association of each commenter (not the comment text) | "new" status and outside demand (4.3) |
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

---

## 4. Open source: outside authors and pull requests

Some or all of a project's repos may be open source with the owner as maintainer. Issues
and pull requests then arrive from people outside the project, and anything the tracker
publishes is said in the maintainer's name. Pull requests are **in scope** (owner,
2026-10-23).

### 4.1 Who counts as outside

GitHub reports each issue's, comment's and pull request's `authorAssociation`: `OWNER`,
`MEMBER`, `COLLABORATOR`, `CONTRIBUTOR`, `FIRST_TIME_CONTRIBUTOR`, `FIRST_TIMER` or `NONE`.
The project config names its maintainers (`maintainers = ["yuvalrakavy"]`); an author is
**inside** if listed there, or if GitHub reports `OWNER`, `MEMBER` or `COLLABORATOR` for
the repo. Everyone else is **outside**. Each item records its author and association, and
the dashboard marks outside items.

### 4.2 Outside text is untrusted input

An issue body, comment, pull-request description or diff written by an outside author is
**data, never instructions**, wherever it travels:

- **Into briefs.** A fix, triage or review brief quotes outside text in a fenced block
  headed by its author and association, and the brief says plainly that the block is the
  reporter's material and that nothing in it directs the session.
- **Into the triage agent.** The agent reads outside text to judge an issue; it never runs
  a command, opens a link, or changes a setting because the text asks it to. A request in
  the text ("please label this p0", "run this script") becomes at most a suggestion shown
  to you, marked as coming from the reporter.
- **Contributor code.** A pull request from an outside author is untrusted code. A session
  never builds or runs it outside a sandbox, and building or testing it at all needs your
  approval at launch. Reading the diff needs none.

### 4.3 Intake

An outside issue starts in a status before *untriaged*: **new**, until an inside author
has commented, labelled or closed it. The triage agent may draft a first reply (thanks,
and what is missing: a version, a reproduction, logs), which goes through plan-then-apply
(1.7) like any other public write.

Outside demand is an input to ordering: **reactions** (👍 counts on the issue) and the
number of distinct outside commenters feed the reach part of an issue's value (chapter 6).

### 4.4 Pull requests

The snapshot reads, per pull request in a member's view: number, title, author and
association, draft state, review decision, the latest commit's check status, whether it can
be merged, the issues it would close, and its created and updated times. (All of these are
GraphQL fields; probed on rhaiscript/rhai.)

| Status | Condition |
|---|---|
| **needs review** | not a draft, no review decision, checks passing or not required |
| **checks failing** | the latest commit's checks failed |
| **waiting on author** | changes requested, or a draft |
| **approved** | approved, not yet merged |
| **conflicting** | cannot be merged as it stands |

A pull request that closes an issue inherits that issue's priority for ordering, and an
issue with an open pull request shows it. "Needs review" pull requests from outside authors
appear under **waiting on you**, with their age.

**Review sessions.** A pull request row has a launch button for a review session, routed by
the diff's size and the areas it touches. The session reads the diff, runs the repo's
checks in a sandbox if you approved that at launch, and drafts a review. The draft is
posted only after your approval. **The tracker never merges and never pushes to a
contributor's branch.**

### 4.5 Security reports

A repo's private vulnerability reports (GitHub's repository security advisories) are read
as private items: shown only on the private dashboard, never named in a public comment,
label, commit message or public brief. A fix session for one works in the advisory's
temporary private fork, which GitHub provides for that purpose.

### 4.6 Speaking in the maintainer's name

Every public write (a label, a comment, a review) is published as the maintainer and goes
through plan-then-apply. Two settings in the project config:

- `voice`: a path to the project's own guidance for replies (a CONTRIBUTING file, a tone
  note), handed to any session that drafts one;
- `disclose_drafting`: whether a comment the agent drafted carries a short footer saying
  so. **Default `false`** (owner, 2026-10-23): you approve every comment, so it is yours.
  The plan-then-apply step offers a per-comment switch to add the footer anyway, for the
  cases where you want contributors to know.

### 4.7 Contributor labels and metrics

`good_first_issue` and `help_wanted` join the modifiers (1.2), translated per repo. The
flow metrics gain: time to first response on outside issues, time to first review on
outside pull requests, and outside items opened and closed per week.

---

## 5. The dashboard page and its database

### 5.1 One page per project

Each project's dashboard is a private claude.ai Artifact declaring three capabilities:
`db` (the data), `comments` (for `sendToClaude`, the launch path) and `user` (to tell the
owner from anyone the page is shared with). It renders without GitHub access, so it works
on a phone. The page code is generic and lives in this repo; only its data is per project.

### 5.2 What the page shows, summary first

| Panel | Shows | You can |
|---|---|---|
| **Header** | project name; when each repo was last read, with stale repos flagged (2.5) | open `explain` for any row |
| **Waiting on you** | everything blocked on you, oldest first (2.3), including plans awaiting approval (1.7) and outside pull requests needing review (4.4) | act on each in place |
| **Next batch** | the top batch from chapter 6: its issues, why they are grouped, the estimated gate cost, what fixing them unblocks | launch one session for the batch |
| **Queue** | every open issue in order, filterable by repo, area, type, status, inside/outside | set a priority, add an override, launch a fix session |
| **Triage queue** | the triage session's suggestions, each with its confidence and reason | accept, reject (with an optional reason), or edit |
| **Pull requests** | open pull requests by status (4.4) | launch a review session |
| **Forks** | each fork's state and how far behind it is (chapter 3) | launch "go back" or "send upstream" |
| **Metrics** | flow, aging against the time limits, outside response times, session outcomes, decision accuracy (5.7) | change the time window |

At phone width the panels stack in that order, so the first screen is the header and what is
waiting on you.

### 5.3 The database

Document ids replace `/` and `#` with `:`, so `yuvalrakavy/Store#403` is
`yuvalrakavy:Store:403` (the database allows letters, digits and `_ - . ~ : @ +` in a
segment).

| Collection | Written by | Holds |
|---|---|---|
| `meta` | tracker | project summary, per-repo read times and errors |
| `issues` | tracker | one document per issue or pull request: the snapshot row (2.4) plus its rank and score terms (chapter 6) |
| `suggestions` | tracker | one per open suggestion |
| `plans` | tracker | one per GitHub write plan awaiting approval |
| `forks`, `metrics` | tracker | chapter 3 and the metrics panel |
| `decisions` | **page** | one per thing you did: accept, reject, edit, set priority, override, approve a plan |
| `views` | **page** | saved views (5.6) |
| `launches` | **page**, then dispatcher | one per launch request, moving through `requested`, `queued`, `started`, then `landed`, `stopped` or `failed`, or `not_sent` (7.3, 7.8, 5.4) |

**The page never edits a tracker document, and the tracker never edits a `decisions`
document.** Your actions are written as new `decisions`; each tracker run reads the ones it
has not yet consumed, applies them to the state files (1.6), marks them consumed, and
rewrites its own collections. Because the two writers own disjoint collections, an edit you
make during a run cannot be overwritten by it. Access rules make the tracker collections
read-only to viewers, and `decisions` and `launches` writable only by the owner.

### 5.4 The launch path

A launch button:

1. checks `comments.canSendToClaude()`; anything but `available` disables the button with
   the reason ("the dispatcher is not running" for `no_session`);
2. writes a `launches` document with the issue or batch and the routing the tracker
   suggested, which you may change;
3. calls `comments.sendToClaude` with a comment holding only the launch id. If the send is
   refused, the page marks the launch `not_sent` with the error code, so a request no
   dispatcher ever heard is never left looking `requested`.

The comment carries nothing but the id: the dispatcher reads the request from the database,
so nothing typed into the thread can change what is launched.

**One thread, not one per launch.** An artifact has a limit on its threads and replies, so
launches reply into a single launch thread whose id the page keeps in `launches/_thread`
(the page owns `launches`, see 5.3; the comments
capability does not hand a thread id back on a later visit). When that thread is refused
for being full, the page starts a new one and records its id. The dispatcher answers each
launch in the thread with what it started. `sendToClaude` works for the
artifact's editors only, so a person the page is shared with read-only cannot launch.

### 5.5 Freshness

Each panel shows the snapshot time it was built from. A repo whose last good read is older
than twice the run interval is marked stale in the header and on its rows.

### 5.6 Views: include and exclude by repo and group

A **view** decides which tracked repos the dashboard shows. It has an include list and an
exclude list, each holding repos and groups; an empty include list means every repo, and
**exclude wins** over include. "Without bridges" is `exclude = [group bridges]`; "hide
mqtt_dali" is `exclude = [repo yuvalrakavy/mqtt_dali]`; "only the PD libraries" is
`include = [group pd-libraries]`.

- Views can be saved by name and switched in the header. The project config names the
  default view; saved views are page-owned documents (`views`, 5.3).
- **Isolate** is a one-step view: every repo name and group name on the page has an
  "isolate" action that shows only that repo or group, with every panel, tile and metric
  aggregated over it alone ("Bridges: 2 open, 0 over time limit, first response 2.1 days").
  The header shows "Isolated: Bridges", and one tap returns to the view you came from.
  An isolated scope can be saved as a view like any other.
- The metrics panel can also break any metric down by group, so groups can be compared side
  by side without isolating each in turn.
- A view filters every panel, the summary tiles and the metrics, and the header names the
  view in force, so a number is never read without knowing what it covers.
- **Hidden is not gone.** When a view hides a repo that has something urgent (an open p0,
  a private security report, an item waiting on you), the header shows one line such as
  "Hidden by this view: 1 p0 in mqtt_dali", with a link that shows it.
- A view changes only what you see. Tracking, ordering, triage and write-back keep working
  for every tracked repo. To stop tracking a repo, remove it from the project config, or,
  when it arrives through a group, list it in that member's `exclude` (1.4).

### 5.7 Learning from your corrections

Every accept, reject, edit and override is recorded as a `decisions` document, with an
optional one-tap reason ("wrong area", "already fixed", "not a bug"). Those records do
three jobs:

- **ground truth for the decision seam** (7.1): each one is a labelled example, so the
  evaluation set grows by itself as you work;
- **a live accuracy figure per decision and per backend** on the metrics panel ("type
  suggestions: 41 of 44 accepted in the last 30 days"), so a backend whose accuracy drops is
  visible without running an evaluation;
- **examples for the triage session**: recent corrections are included in its brief, so it
  stops repeating a mistake you have already corrected.

---

## 6. Ordering

### 6.1 Two stages

1. **The priority level is a floor.** Issues are grouped by their effective priority
   (1.5), and every p0 comes before every p1, and so on. An issue that blocks another takes
   the higher of its own level and the levels of the open issues it blocks (within the
   project), so a p3 Rhai bug blocking a p1 Store issue is ordered as p1.
2. **Within a level, WSJF orders the issues.** Weighted Shortest Job First: cost of delay
   divided by job size, scaled by confidence.

`umbrella` issues are not ordered. Blocked issues keep their place but are shown as blocked
and never offered as "next".

### 6.2 Cost of delay

The sum of three terms, each on the relative scale 1, 2, 3, 5, 8, 13:

| Term | From |
|---|---|
| **Value** | severity (data loss, a hang, a silent wrong answer score highest) × reach in this project; outside demand (reactions, distinct outside commenters, 4.3) raises reach |
| **Time criticality** | how much of its time limit the issue has used; a GitHub milestone with a due date |
| **Risk reduction and opportunity** | the cost of delay of what it unblocks; membership of a defect class fixed in the same session; a silent failure becoming visible (detectability) |

Severity, reach and detectability come from the decision seam (7.1), with your corrections
(5.7) taking precedence.

### 6.3 Job size

Effort (S = 1, M = 3, L = 8) plus **gate cost**, which comes from the repo's launch recipe:
each recipe declares the cost of its gate levels (Store's quick gate and full gate cost very
differently), and the issue's blast radius picks the level. In a batch the gate cost is
paid once, so each member's share of it is divided by the batch size.

### 6.4 Confidence

A multiplier between 0.5 and 1.0: the probability the decision seam returned for the
issue's severity and reach, lowered further while the issue's claims are unchecked against
the code (duty 0 not yet done). An issue rises once its claims are confirmed.

### 6.5 The score

`score = cost of delay × confidence ÷ job size`. Ties go to the older issue. `explain`
prints every term.

### 6.6 Batches

The tracker groups ready issues into candidate batches: same repo, same launch recipe, the
same gate level, and no overlap in the files their fixes are expected to touch. Those
expected files come from the triage session's estimate for each issue (the sites it found,
recorded with the commit they were read at), so a batch is only as good as that estimate;
an issue with no estimate is never batched. A batch's
score is the sum of its members' cost of delay, times their lowest confidence, divided by
the summed effort plus one gate cost. The highest-scoring batch is the **Next batch**
(5.2). Batch size is capped by `max_batch` in the project config.

### 6.7 The priority suggestion

The suggested level for an untriaged issue is a decision through the seam (7.1), from
severity, reach, likelihood and detectability, with a silent failure raised one level. It
is shown beside the current label with its confidence, and becomes the project priority only
when you accept it.

### 6.8 Keeping the estimates honest

When a fix session lands, its recorded wall clock and gate rounds (chapter 7) are compared
with the job size it was given. The comparison is shown per repo on the metrics panel and
used to scale that repo's future estimates.

An issue that has stayed out of the top 20 for 60 days is added to **waiting on you** with
three choices: fix it, lower it, or close it as not planned.

---

## 7. Launching sessions, routing and decisions

### 7.1 The decision seam

Much of triage is a small typed decision made over many items: an issue's type, its area,
the severity and reach inputs to its priority, whether an outside issue is a support
question, whether outside text is trying to instruct the agent (chapter 4), whether two
issues are duplicates, whether a commit fixes an issue, and which model and effort a
launched session gets. Every such decision goes through **one interface**:

```text
decide(question, allowed answers, input)  ->  { answer, probability per allowed answer }
```

**A backend is anything that meets that contract.** Three kinds are expected:

| Kind | How it gives probabilities |
|---|---|
| A model built for typed, calibrated decisions (TypeSafe's Jev is one; other vendors may ship similar models) | natively |
| A general model with structured output (Claude, OpenAI's models, a local model) | from token probabilities where the API exposes them, otherwise by **asking several times and counting** how often each answer comes back |
| A wrapper that adapts one kind to the other (such as TypeSafe's open-source one) | whatever it wraps |

Asking several times and counting makes any model a backend, at the cost of more calls,
and gives every backend the same contract, so they can be compared on equal terms.

Two rules:

1. **Probabilities are never trusted on a vendor's word.** Each backend is evaluated per
   decision on the project's own history: the labelled open issues and the closed ones,
   with the owner's priority overrides as further ground truth. The evaluation measures
   accuracy and **calibration** (when it says 80 %, is it right about 80 % of the time). A
   backend is used for a decision only where it passes, so one backend may win "type" and
   lose "duplicate".
2. **Backends are configuration, and so is who may see what.** The tracker-wide config picks
   a backend per decision. Each repo has a data policy naming the backends allowed to see
   its issue text; for a private repo, every external backend is off until the owner
   allows it.

Where a decision feeds the priority score, its probability is the score's **confidence**
term directly (chapter 6). Work that needs to read code, use tools or write prose (duty-0
checks, briefs, root-cause work, drafted replies and reviews) stays with Claude sessions and
does not go through this seam.

### 7.2 What runs, and when

| Job | What it is | When |
|---|---|---|
| **Snapshot** | the CLI (`issue-dashboard snapshot`), no model involved: chapter 2, then the database write | every few minutes (`run_interval`, default 5 min), from a scheduled job on the machine with the checkouts |
| **Triage** | a Claude session: reads the snapshot, makes suggestions through the decision seam, checks claims against the code, groups defect classes and batches, applies accepted decisions and approved plans | daily, and on demand from the dashboard |
| **Dispatcher** | a long-lived Claude session watching each project's dashboard | always on while you want launches to work |

The snapshot never waits on a model, so the dashboard stays current even when no session
is running; only suggestions go stale, and their age is shown.

### 7.3 The dispatcher

A Claude Code session that watches the project's dashboard artifact and is woken by a
launch comment (5.4). It does no work of its own between launches, so it costs nothing while
idle.

**Its model sets the ceiling.** A session can start other sessions only at or below its own
model and effort (probed: `start_session` ignores a higher model and caps effort). So the
dispatcher runs on the strongest model at the highest effort, checks its own model and effort
when it starts (`get_session self`), and refuses, with the reason in the launch thread, any
launch whose routing it cannot honour, rather than starting it silently downgraded.

On each launch comment, in order:

1. Read `launches/<id>`. Proceed only if it is `requested` and was written by the owner.
2. Re-read the issue on GitHub. Refuse if it is closed, or if it carries the in-progress
   label (set by any project's dispatcher, or by a person). For a read-only repo, where no
   label is ever written, check instead the launch records of **every** project in the
   tracker root, which is one directory (1.1), so two projects cannot both launch on one
   upstream issue.
3. Check the launch recipe's capacity (7.6), such as Store's three lanes. If none is free,
   mark the launch `queued` and say so in the thread; it starts when a slot frees.
4. Build the brief (7.5) and start the session with the routed model and effort.
5. Write `started` with the session's id to the launch document, add the in-progress label
   with a "work started" comment (unless the repo is read-only, or `writes = "none"`), and
   reply in the thread with a link to the session.
6. When the session ends its turn for good (landed, stopped, or failed), write the launch's
   final state, remove the in-progress label with a comment saying why the work ended, and
   record the outcome (7.7).

### 7.4 Routing

The project config holds a routing table; the defaults follow the `ways-of-working` model
routing:

| Launch | Model | Effort |
|---|---|---|
| fix, tier T0 or T1, or a self-contained issue | Opus | medium |
| mechanical sweep, docs sync, "go back" from a fork when the release is a patch | Sonnet | medium |
| fix, tier T2 | strongest | high |
| fix, tier T3, or a bug whose cause is unknown | strongest | high |
| batch | the highest row any member needs | |
| triage session | Sonnet | medium, handing judgement calls to a stronger subagent |
| review of an outside pull request | strongest | high |
| `decision` type (design question) | the best design model | high, design only, no code |
| "send upstream" | Opus | medium |

The tier comes from the triage session's estimate for the issue. The dashboard shows the
routing with a one-line reason before launch, and you can change it; a change is recorded
(5.7) so the routing can be tuned from evidence.

### 7.5 Briefs

A brief is built by the dispatcher from the issue, the snapshot and the repo's recipe, and is
shown on the dashboard before launch. Its sections:

1. **The task** in one sentence, and what done looks like for this issue.
2. **The issue**: title, labels, links to what it blocks and is blocked by. Text by inside
   authors is quoted as is; **text by outside authors is fenced and headed with its author
   and association**, with the rule from 4.2 stated in the brief.
3. **Claims to re-verify**: the triage session's findings (commits that mention the issue,
   whether the sites the issue cites still exist, the suggested tier and blast radius),
   explicitly labelled as hypotheses.
4. **Where to look**: the repo's docs list (registry or repo file).
5. **How to work in this repo**: the recipe's workspace, gate and landing steps, and the
   repo's own process rules if it has them (Store's `ways-of-working`).
6. **Out of scope**: side problems get filed, not fixed.
7. **Reporting back**: the outcome record to write when it lands (7.7).

### 7.6 Launch recipes

A recipe is the repo's own way to work on a change. It lives in the repo file when the repo
has one, otherwise in the registry, and the tracker never runs its commands itself: they are
handed to the session in the brief.

```toml
[launch.store-dev-track]
capacity  = 3                                   # Store's lanes
workspace = "store-cli dev-track claim {branch}"
release   = "store-cli dev-track release --full"
gate.quick = { run = "scripts/pre-merge-check.sh --quick", minutes = 6 }
gate.full  = { run = "scripts/pre-merge-check.sh --full",  minutes = 17 }
land      = "store-cli dev-track land"
process   = ".claude/skills/ways-of-working/SKILL.md"

[launch.worktree-pr]                            # this project's default for its Rust repos
capacity  = 2
workspace = "git worktree add ../{repo}-{branch} -b {branch}"
release   = "git worktree remove ../{repo}-{branch}"
gate.quick = { run = "cargo nextest run", minutes = 2 }
land      = "gh pr create --fill"
```

`{branch}` is `fix/<repo>-<number>` (or `batch/<first number>` for a batch). Gate `minutes`
are the job-size input of 6.3, and are corrected from measured outcomes (6.8). The Store
figures come from Store's own measured cost table (quick gate 290-330 s, full gate
810-1020 s, in its `issue-closing-campaign` skill); the generic recipe's figure is a guess
until outcomes are recorded. The tracker itself names no tool: `cargo nextest` appears only
because this project's repos are Rust.

### 7.7 Outcomes

The brief tells the session to run, just before it ends, `issue-dashboard record-outcome
--launch <id>` with: landed or not, the commit or pull request, wall-clock time, gate runs
and their levels, findings fixed, and its context size from `get_usage self`. Token cost per
session is not available to a session (probed: `get_usage` reports only the live context
window), so it is not recorded. A session that ends without recording one gets a record from
the dispatcher with what the dispatcher saw (end time, final state) and the rest marked
missing.

### 7.8 When something goes wrong

| What happens | What you see |
|---|---|
| no dispatcher is listening | the launch button is disabled with "the dispatcher is not running" (5.4) |
| the routing is above the dispatcher's own model | the launch is refused in the thread, naming both |
| the recipe has no free slot | the launch is `queued`, and starts when a slot frees |
| the session cannot be started | the launch is `failed` with the reason in the thread; the in-progress label is not added |
| the session stops without landing | the launch is `stopped`; the label comes off with "work ended: stopped, not landed" |
| the dispatcher session itself ended | launches stay `requested`; the button is disabled until a dispatcher is watching again, and the requests are offered again then |

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
| GraphQL gives `authorAssociation` on issues and pull requests, and `isDraft`, `reviewDecision`, `mergeable`, `closingIssuesReferences`, the latest commit's `statusCheckRollup` and `reactionGroups` | confirmed | probe on rhaiscript/rhai, 2026-10-23 (e.g. #1194 by an outside author, association `NONE`, closes #1193) |
| The repository security-advisories endpoint answers on a personal public repo | confirmed | `gh api repos/yuvalrakavy/logmon-mcp/security-advisories` → `0` advisories |
| Renaming a label in a repo's settings does not change its issues' update time | believed | not probed; the daily full read covers it either way |
| The forks' upstreams are bytebeamio/rumqtt and rhaiscript/rhai | confirmed | `gh repo view … --json parent` on both forks |
| `sendToClaude` wakes a watching session | confirmed | probe artifact, 2026-10-22 |
