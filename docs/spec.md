# issue-dashboard — design spec

Status: **chapters 1-9 approved by the owner (2026-10-23); chapter 10 added after the first T2 verification (17 findings) and revised after the second (3 closed, 14 partly, 11 new); a third, scoped review of the phase-1 surface pending.** Tier T2: a generic tool whose
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
7. Launching sessions: decisions, the dispatcher, routing, briefs, launch recipes — approved 2026-10-23 (7.2's sync job added after)
8. The CLI — approved 2026-10-23, revised for chapter 10
9. Documentation and skills — approved 2026-10-23
10. **State and coordination: the event ledger, and the phases** ← revised after the second verification; scoped third review pending

**Owner decisions still open** (proposed defaults are in the text, marked "proposed"):
the scoring terms (6.2-6.4) and the backend admission thresholds (7.1). Phase 2's write
rules are settled by its test-first build (10.9). **Phase 1 is read-only toward GitHub**
(10.0): chosen by the author on 2026-10-23 under the owner's delegation, reversible.

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
When it arrives through two groups and no entry of its own, the two members' settings must
agree; if they differ, the config check refuses and names both, so the answer is never
decided by the order of the file.
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

Every project's view (project priorities, overrides with their reasons, accepted and
dismissed suggestions, the correction log) is derived from **one event ledger per tracker
root**, written only by the CLI (chapter 10). The dashboard's database is where a person
acts: each action is a new page document, which the next cycle ingests as an event and
acknowledges (10.6). Because every project shares one ledger, one project's cycle sees
another's priorities for the shared-repo rule in 1.5, in a single transaction.

### 1.7 Writing to GitHub safely

Every write to GitHub goes through **plan, then apply**:

1. A run first computes every write it would make: issue, old labels, new labels, comment
   text, and the reason.
2. **A dry run is forced** on a repo's first run after it joins a project, and after its
   translation changes. The plan is shown on the dashboard and applied only once you
   approve it.
3. **A cap per run:** a plan with more writes than `max_writes_per_run` (default 10, set in
   the project config) waits for your approval.
4. **Every write is recorded in the ledger** (intent, then result), and
   `issue-dashboard undo <run id>` reverses what can be reversed, only where nobody has
   changed the same thing since.

How plans carry preconditions, how approval binds to a plan's content, how a crash midway is
reconciled, and exactly when undo is allowed: 10.9. **All of 1.7 is phase 2** (10.0): in
phase 1 the tracker writes nothing to GitHub but the owner's closes from the page (10.10).

### 1.8 Issue identity and links

Every issue is shown and linked as `owner/repo#N`, and stored by GitHub's permanent node id,
so a rename or transfer keeps its history (10.2). Links
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
- a member with no registry entry, or two `repo =` members naming the same repo (a repo that
  arrives both through a group and through its own entry is not a duplicate: its own entry
  wins, 1.4);
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
| per issue: node id, number, title, state and close reason, author and association, assignees, labels, milestone and its due date, created / updated / closed times, comment count, reactions, body | identity (10.2), translation, status, aging, time criticality and outside demand (chapter 6), the structured filing block (2.6) |
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

Each cycle does two reads per member:

1. **Membership, cheaply:** the member's full filtered query, asking for item ids only. Its
   result is the member's issue set; an issue that has stopped matching the filter (a label
   removed, say) leaves the set on this cycle, not on the next full read.
2. **Details, incrementally:** for the repo, every item updated since the last
   *successful* read of that repo (`updated:>` the stored time minus a five-minute overlap),
   **without** the member's filter, so a change that takes an issue out of a filter is still
   read. Details are fetched only for items in some member's set.

The watermark advances only when a read finished with every page fetched, and it is kept per
repo, shared by every member and project that includes the repo (the filter is applied by
read 1, not by the watermark). Closed items older than `history_days` are evicted from the
derived views, and kept in the ledger.

A **full read** replaces the incremental one: on a repo's first run, after its filter or
`also` changes (an incremental read cannot notice an issue that now falls out of the
filter), after the repo's translation changes, and once a day. The daily read also catches
what does not move an issue's update time: an issue transferred or deleted, and a label
renamed in the repo's settings.

A number in `also` is looked up as an issue or a pull request (they share one number space
in a repo) and joins the matching list (4.4); a number that exists as neither is reported by
the config check and skipped.

**An item entering a member's set is always read in full** on that cycle, even if it has not
changed since the watermark, so an issue that newly matches a filter (or is added to `also`)
appears with its details at once.

### 2.3 What is derived

| Derived value | Rule |
|---|---|
| canonical type, areas, modifiers | translation (1.3), with untranslated labels listed per repo |
| priority and its source | the three sources in order (1.5), plus every source's value for `explain` |
| status | the state first: a closed issue or pull request is `closed`, a merged pull request `merged`, whatever its labels, blockers or sessions say; an open one is `untriaged` (an axis missing), `needs-decision`, `in-progress`, `blocked` (an open blocker), else `ready`; `umbrella` issues carry `umbrella` instead |
| sessions working on it | the app's sessions matched to the item (7.2, 10.10 "Session tracking"): exact by launch or by pull request, probable by an issue reference in the title or branch; an open item that would be `ready` or `blocked` is `in-progress` while one is matched |
| age and idle time | from created and updated times |
| time-limit breach | idle time against the project's limit for the issue's priority |
| link graph | native relationships first, text links second, with the source of each edge recorded |
| "waiting on you" | issues with `needs-decision`, plans awaiting approval (1.7), overrides gone out of date (1.5), GitHub-side changes awaiting a decision in a shared repo (1.5) |
| commits mentioning an open issue | from `git log`; the input to the triage session's "fixed but still open?" check |
| flow | per week: opened, closed (completed / not planned), net, per repo and for the project |

### 2.4 What is written

A readable export derived from the ledger (chapter 10), never read back as input:
`<root>/state/<project>/snapshot.json`, replaced on each cycle, and a copy per day under
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

### 2.6 The structured filing block

An issue may carry, at the end of its body, a block the tracker reads:

````markdown
<!-- issue-dashboard v1 -->
```toml
sites      = ["store_server/src/command/delete.rs:212"]
read_at    = "95c2531bb"          # the commit the sites were read at
tier       = "T1"
blast      = "quick"              # the gate level a fix needs
related    = ["yuvalrakavy/Store#401"]
kind       = "related"            # related | side
```
````

Unknown keys are kept and reported; a malformed block is reported and ignored. The block
is trusted only from an inside author (7.5). Its JSON Schema is in `schemas/` with the
others.

### 2.7 Testing seam

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
3. otherwise, nothing: the fork is "no upstream fix recorded".

**Whatever the links say, every fork commit must be accounted for.** The tracker lists the
commits on the pinned revision that are not in the upstream's history (the fork's **delta**)
and matches each to an upstream change: by the linked pull request's commits, by an identical
patch (`git patch-id`, which also matches a cherry-pick), or by a squash-merged pull request
whose combined patch matches. A commit that matches nothing is **unaccounted**, and the fork
is never offered "go back" while one remains.

### 3.3 The states

| State | Condition | The dashboard shows |
|---|---|---|
| **Not offered upstream** | a fork commit is in no upstream pull request | "rumqttc: 1 local fix never sent upstream", with a launch button for a session that prepares the pull request |
| **Under review** | the fix pull request is open | the pull request and how long it has been open |
| **Merged, unreleased** | merged, and no release tag contains the merge commit | "merged on …, waiting for a release" |
| **Released** | **one** release tag contains every matched upstream change, the delta has no unaccounted commit, and that version is published on the package registry | **"Can go back to rhai 1.27.0: it contains rhai#1189"**, with a launch button |
| **Partly released** | some delta commits are released, others are unreleased or unaccounted | each commit and its state; no "go back" button |
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

Pull requests are read by a second query per member, with `is:pr` in place of `is:issue`
and the same `filter` and `also` rules, under the same membership and incremental reads
(2.2). Per pull request: number, title, author and association, draft state, review
decision, the head commit's id and check status, whether it can be merged, the issues it
would close, and its created and updated times. (All GraphQL fields; probed on
rhaiscript/rhai.)

A pull request has exactly one status, the **first** of these that holds:

| Order | Status | Condition |
|---|---|---|
| 1 | **draft** | a draft |
| 2 | **conflicting** | GitHub reports it cannot be merged as it stands |
| 3 | **checks failing** | the head commit's checks failed |
| 4 | **waiting on author** | changes requested since the last push |
| 5 | **checks running** | checks pending |
| 6 | **approved** | approved, not yet merged |
| 7 | **needs review** | anything else open; "mergeable unknown" counts here and is shown as such |

**Inside actions and response times.** An action (a comment, a label change, a close, a
review) is inside when its actor is listed in `maintainers` or is a collaborator of the repo
(the collaborator list is read once a day). An outside item's **first response** is the
earliest inside action after its creation; for a pull request, the first inside review.
Comment *metadata* (author, association, time) is read for every changed item; comment
*bodies* only for items whose metadata shows an inside comment (text links, 1.8) or an
outside comment newer than the last read (shown in Details, 5.8).

**Contributor code runs only for the revision you approved.** Approving a sandboxed build or
test of an outside pull request names its head commit; a later push needs a new approval.

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
as private items, with the source tag `private-advisory` (7.5): shown only on the private
dashboard, never in any write plan, and never placed in a brief for any other item. A fix
session for one is launched from its own card, works in the advisory's temporary private fork
(which GitHub provides for that purpose), and its brief tells it not to name the advisory in a
public commit, comment or pull request before the advisory is published.

**Backends and private text.** A decision-seam request (7.1) may combine text from several
items, for example a duplicate check comparing two issues. The request is allowed only to
backends that **every** source's data policy allows, and text tagged `private-advisory` goes to
no external backend at all.

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

### 5.2 Layout: a status strip and three tabs

The page is built around the owner's jobs, most frequent first: glance (is anything on
fire, is anything waiting on me?), decide, pick and launch the next work, and study trends.
Revised 2026-10-23 after a UX review of the first mockup.

**Status strip** (on every tab): the project name; the view or isolated scope in force as a
chip (5.6); one freshness line ("all repos read 08:14 · rhai stale"), expanding to per-repo
times on tap; the "hidden by this view" line; and summary tiles (p0 open, waiting on you,
untriaged, over time limit, open). **Every tile is a link** to the tab and filter it counts.

| Tab | Holds | Main actions |
|---|---|---|
| **Inbox** | everything that needs the owner, one card per item, oldest first: plans to approve (1.7), decisions, outside pull requests to review (4.4), stale overrides and GitHub-side changes (1.5), starving issues (6.8); then the triage suggestions with their confidence; then **Recently closed** (5.8) | the card's one main action; a `⋯` menu for the rest; accept or reject suggestions, singly or "accept all at or above a confidence"; Reopen or Dismiss a recent close |
| **Queue** | the next batch on top with its launch sheet; then open issues grouped by priority level; then pull requests and forks that are **not** already in the inbox | launch, set a priority, override, explain |
| **Trends** | flow, time limits, outside response times, decision accuracy, session outcomes, each splittable by group or repo | change the period, isolate a group |

**An item appears once.** Anything that needs a decision lives in the Inbox; the Queue and
its lists show only what is not already there, and members of the next batch are not
repeated in the level lists.

### 5.2a Interaction rules

- **One main action per card**, styled as primary; every other action in the `⋯` menu.
- **A decision shows that it is pending.** Decisions are applied on the next tracker run
  (5.3), so an acted-on card collapses to "Decided · applied on the next run (in N min)" with
  **Undo**, until the run consumes it.
- **Buttons say what they do.** A suggestion's buttons name the outcome ("Set bug · p1",
  "Label as todo", "Check and close…", "Close as duplicate…") and "Dismiss"; never a bare
  Accept / Reject. A trailing `…` means the button leads to a further step instead of acting
  at once. What each kind of suggestion does is fixed in 5.8.
- **"Dismiss" offers a reason afterwards**, optional, with one-tap reasons fitting the
  suggestion (5.7) and "Other…". Dismissing never waits on the reason.
- **Every issue and pull request can always be opened.** Its number is a link to it on
  GitHub (a new tab), wherever it appears: a card, a row, a batch, a brief preview. Every
  card's and row's `⋯` menu also holds **Open on GitHub** and **Details**, an in-page view
  of the issue's body, labels, links, priority sources (`explain`) and recent decisions.
- **Launch opens a sheet**, not an immediate action: the suggested model, effort and
  workspace (editable, with the reason they were suggested), "Preview brief", and Launch.
  After launch the row shows the launch state (`requested`, `queued`, `started`, `landed`).
  When no dispatcher is listening, Launch is disabled with that reason (5.4).
- **Every issue can be closed** (10.0's exception): "Close…" in each issue card's and row's
  `⋯` menu and as a button in Details, except on a read-only repo and on pull requests. It
  opens a sheet: the reason (completed, not planned, duplicate of #N, which requires the
  number), a comment prefilled per reason and editable, and Close; on a public repo the sheet
  says the comment is published in the owner's name. The card then shows "Closing · applied on
  the next sync" with Undo until the ack. Undo of a close already applied reopens the issue,
  offered only while it is still closed by that close and nobody has commented since; the
  Inbox's Recently closed list (5.8) holds it for that.
- **A session working on an issue shows on it.** A card, a row and Details carry "In
  progress" with each matched session: its title, running or idle, when it was last active,
  and "probably" when the match is by an issue reference rather than exact (10.10). The
  session's link is shown as copyable text beside a link to it, since a `claude://` link may
  not open from inside the page. Sessions matching no item are not listed in phase 1.
- **Rows show the level and idle time; the score is in `explain`.** Scores only compare
  within a level (6.1), so they are not shown as a column beside the level.
- **Phone width:** queue rows become cards with one action; nothing scrolls sideways;
  touch targets are at least 40 px high.
- **Keyboard:** `j`/`k` move, `a` main action, `d` dismiss, `o` open on GitHub, `u` undo,
  `/` filter.
- **States:** an empty inbox says "Nothing waiting on you"; a stale repo's rows carry
  "read HH:MM, stale"; a panel whose data is loading says so.
- **Contrast:** every text colour meets WCAG 4.5:1 in both themes; priority levels are
  filled badges.
- **Metrics show their base:** "9 of 31", never a bare percentage, and a target tick where a
  threshold exists.

### 5.3 The database

Document ids are GitHub's node ids (10.2), which use only letters, digits, `_` and `-`,
all allowed in a database segment; each document also carries the current
`owner/repo#N` for display. A rename or transfer therefore changes a document's display
field, never its id, and a page action on it still names the same item.

| Collection | Written by | Holds |
|---|---|---|
| `meta` | tracker | project summary, per-repo read times and errors |
| `issues` | tracker | one document per issue or pull request: the snapshot row (2.4) plus its rank and score terms (chapter 6) |
| `suggestions` | tracker | one per open suggestion |
| `plans` | tracker | one per GitHub write plan awaiting approval |
| `forks`, `metrics` | tracker | chapter 3 and the metrics panel |
| `decisions` | **page** | one per thing you did: accept, reject, edit, set priority, override, approve a plan, close, dismiss a closed item |
| `views` | **page** | saved views (5.6) |
| `requests` | **page** | brief previews, on-demand triage and sync requests (7.2, 10.8) |
| `acks` | tracker | which page decisions each cycle consumed (10.6) |
| `launches` | **page** | one per launch request; written once and never changed (a refused send is a second document, `not_sent`, naming the first; each launch thread is a thread record of its own) |
| `launch_status` | tracker | each launch's current transition (10.7) |

"Tracker" in this table means the tracker's documents as written by the sync job (7.2),
which carries the CLI's prepared documents into the database; the CLI itself never touches
it.

**The page never edits a tracker document, and the tracker never edits a page document.**
Your actions are written as new `decisions`; each cycle ingests the ones it has not seen as
ledger events and records each in the tracker-owned `acks` collection (10.6). An ingested
decision is never applied twice, because its document id is the event's key. Because the two writers own disjoint collections, an edit you
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
launches reply into a single launch thread whose id the page keeps in a thread record in
`launches` (the page owns `launches`, see 5.3; the comments
capability does not hand a thread id back on a later visit). When that thread is refused
for being full, the page starts a new one and writes a new thread record; the newest record is
the current thread, so no record is ever rewritten. The dispatcher answers each
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

- **Scope pills, not tap cycling.** Each included or excluded repo or group is a pill that
  reads as a sentence ("repo is not mqtt_dali"); tapping a repo or group name anywhere opens
  a menu: Include, Exclude, Isolate. Content filters (type, author, status) are a separate
  row, since they do not change which repos are in scope.
- Views can be saved by name ("Save view" beside the view chip) and switched from the chip. The project config names the
  default view; saved views are page-owned documents (`views`, 5.3).
- **Isolate** is a one-step view: every repo name and group name on the page has an
  "isolate" action that shows only that repo or group, with every panel, tile and metric
  aggregated over it alone ("Bridges: 2 open, 0 over time limit, first response 2.1 days").
  The view chip in the status strip becomes "Isolated: Bridges ✕", and its ✕ returns to the
  view you came from.
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

### 5.8 What a suggestion's main action does

A suggestion changes nothing until the owner acts. **"Dismiss"** never writes to GitHub: it
records the reason (5.7), and the triage session does not repeat that suggestion for that
issue unless its facts change (a new commit, comment or label). The main action depends on
the kind:

| Suggestion | Main action | What happens |
|---|---|---|
| type or area | "Label as todo", "Set area: vm" | recorded at the next cycle and shown as the issue's value; **in phase 1 not written to GitHub** (the card says so); in phase 2 written by the write engine, where a forced dry run or the per-run cap can hold it as a plan |
| priority | "Set p1" | becomes this project's priority and is written back as the label under 1.5's rules (shared repos, read-only repos) |
| likely fixed | "Check and close…" | starts a short check that the fix is present on the default branch; if it holds, a plan to close the issue with a comment naming the commit goes to the Inbox for approval |
| duplicate | "Close as duplicate…" | a plan to close the issue as a duplicate, linking the other, goes to the Inbox for approval |
| combined (type and priority together) | "Set bug · p1" | both, as the rows above |

**The rule behind the table:** accepting a label change is approval enough to write it;
anything that closes an issue or posts a comment is visible to others and harder to undo, so
it comes back once more as a plan. "Apply all labels at or above a confidence" acts only on
label and priority suggestions, never on closes.

**Closing by hand is not a suggestion.** The owner's own "Close…" (5.2a) is the decision and
its approval at once: the sheet already shows the reason and the exact comment, so the cycle
applies it without a further plan. The plans above remain for closes the tracker proposes.

**Recently closed, and dismissing a closed item.** The Inbox ends with every issue closed in
the last N days (the project's `recently_closed_days`, 7 when unset, and never longer than
`history_days`, since an item closed before that is neither read nor kept), newest first. Each
entry says when and how it was closed (GitHub's reason, or "closed from the dashboard as …"),
the tracked pull requests whose closing references name it, and its matched sessions. A close
made from the dashboard keeps its **Reopen** while Undo is allowed (5.2a); every other close
offers **Dismiss**, which never writes to GitHub: it records a `dismiss_closed` decision naming
the close it dismisses (the item's `closed_at`), the item leaves the list at once with an
**Undo** in the toast, and the tracker marks the issue `dismissed_at`. A dismissed item is
hidden from every list and from Details, but stays in the snapshot, so Trends still counts
its close. The dismissal holds for that close only: reopened, the item shows again, and a
later close shows anew. Dismiss is offered on closed issues only; the tracker refuses one on
an open item, or on a close other than the one the page showed.

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
| **Value** | severity (data loss, a hang, a silent wrong answer score highest) plus reach in this project; outside demand (reactions, distinct outside commenters, 4.3) raises reach |
| **Time criticality** | how much of its time limit the issue has used; a GitHub milestone with a due date |
| **Risk reduction and opportunity** | the cost of delay of what it unblocks; membership of a defect class fixed in the same session; a silent failure becoming visible (detectability) |

Severity, reach and detectability come from the decision seam (7.1), with your corrections
(5.7) taking precedence.

**Exact terms (proposed defaults, for the owner to confirm; all in the project config):**
severity is 1, 3, 8 or 13 (cosmetic, degraded, wrong result, data loss or a hang); reach is
1, 2, 3 or 5 (one corner, one subsystem, most of the project, all of it), raised one step
when outside demand passes `demand_threshold` (default 3 distinct outside people); **value =
severity + reach**. Time criticality is 1 below half the time limit, 3 past half, 8 past the
limit, 13 with a milestone due within 7 days. Risk and opportunity is the sum of the values
of the open issues it unblocks, capped at 13, plus 3 for a defect class handled together,
plus 3 when the failure is silent.

### 6.3 Job size

Effort (S = 1, M = 3, L = 8) plus **gate cost**, which comes from the repo's launch recipe:
each recipe declares the cost of its gate levels (Store's quick gate and full gate cost very
differently), and the issue's blast radius picks the level. The two are put in one unit by
counting **one point per 10 gate minutes** (`gate_minutes_per_point`, proposed default 10).
In a batch the gate cost is paid once, so each member's share of it is divided by the batch
size.

### 6.4 Confidence

`confidence = clamp(p, 0.5, 1.0) × u`, where `p` is the lower of the two probabilities the
decision seam returned for severity and reach (1.0 where you set the value yourself), and
`u` is 0.8 while the issue's claims are unchecked against the code (duty 0 not yet done),
else 1.0. The clamp keeps one unsure answer from sinking an issue; an issue rises once its
claims are confirmed. (7.1's "the probability is the confidence term directly" means this
`p`.)

### 6.5 The score

`score = cost of delay × confidence ÷ job size`. Ties go to the older issue. `explain`
prints every term.

### 6.6 Batches

**The floor applies to batches too.** Candidates come only from the highest priority level
that has ready, unreserved work, and a batch never mixes levels; a single issue is a
candidate of size one. So a batch of p3s can never come before an open p0.

Within that level the tracker groups ready issues into candidate batches: same repo, same
launch recipe, the same gate level, and no overlap in the files their fixes are expected to
touch. Blocker elevation (6.1) is transitive; issues in a dependency cycle are flagged on the
dashboard and take the highest level in the cycle. Those
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
   backend is used for a decision only where it passes (proposed default: at least 50
   labelled examples held out from any tuning, accuracy at least 85 %, and expected
   calibration error at most 0.10), so one backend may win "type" and
   lose "duplicate". **Cold start:** until a decision has 50 labelled examples, the default
   backend (Claude) answers it unevaluated; its suggestions are marked "unevaluated" and are
   never included in "apply all at or above a confidence". For decisions with no ground truth
   in labels (duplicates, "is this fixed"), the examples come from your accepts and dismissals
   (5.7) alone.
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
| **Sync** | a scheduled Claude task on the smallest model: reads the page documents from the dashboard database and the app's session list, runs `issue-dashboard cycle` on them (10.4), then publishes the cycle's outbox as a new generation (10.6) | every `sync_interval` (default 5 min); this is the cycle that applies your decisions |
| **Triage** | a Claude session: reads the snapshot, makes suggestions through the decision seam, checks claims against the code, groups defect classes and batches, and records all of it with `record-suggestions`; it applies nothing (10.4) | daily, and on demand from the dashboard |
| **Dispatcher** | a long-lived Claude session watching each project's dashboard | always on while you want launches to work |

**The session list.** The sync job is a Claude session, so it can read the app's session
list (`list_sessions`, archived ones included) and hand it to the cycle as a file
(`cycle --sessions <file>`, 10.10); the CLI never calls the app, so 10.1's rule holds. The
file holds each session's id, title, working directory, branch, pull-request number, whether
it runs, whether it is archived, when it was last active and its link; never a transcript.

**Why a sync job exists.** The dashboard's database is reachable through Claude's artifact
tools; no route from a plain program to it is known (**unverified**: it is the first probe
of the build). So the CLI never touches the database: it reads and writes files, and the
sync job, the smallest possible session, carries those files to and from the database. Its
whole work is two tool calls: one read of the unconsumed page documents, one batch write of
the prepared ones (`ArtifactData` accepts a document from a local JSON file, so the content
never passes through the model). If a direct route turns up, the CLI writes the database
itself and the sync job goes away; nothing else changes.

**Sync now.** A sync request asks for a run sooner than the next tick, for the whole project
or for one item (`owner/repo#N`). A session that has just closed, relabelled or landed a fix
for an issue runs `issue-dashboard sync-now --project P --issue owner/repo#N`: it records the
request in the ledger and prints the project's `sync_task`, the scheduled task's id, and the
session then runs that task now through the app's scheduled-tasks "run now". A per-item
request makes the next read of the item's repo read it by node id, whatever the watermark
says, as it reads an entering item; a global request wakes an ordinary cycle. Requests
coalesce: a good read of the repo serves every request recorded before its plan was made, so
ten requests before a run cost one read, a request made while a run is going waits for the
next run, and a failed read leaves it waiting. An item the project does not track is refused. The page
writes the same request as a `requests` document of kind `sync` (5.3): "Sync now" in the
status strip, "Refresh" in an item's `⋯` menu and Details. The page cannot wake the task, so
in phase 1 the next scheduled run serves it (10.8).

The snapshot never waits on a model; the dashboard is as fresh as the last sync, and shows
that time (5.5). Suggestions only change when a triage session runs, and show their age.

### 7.3 The dispatcher

A Claude Code session that watches the project's dashboard artifact and is woken by a
launch comment (5.4). It does no work of its own between launches, so it costs nothing while
idle.

**Its model sets the ceiling.** A session can start other sessions only at or below its own
model and effort (probed: `start_session` ignores a higher model and caps effort). So the
dispatcher runs on the strongest model at the highest effort, checks its own model and effort
when it starts (`get_session self`), and refuses, with the reason in the launch thread, any
launch whose routing it cannot honour, rather than starting it silently downgraded.

On every wake, first settle uncertain reservations from its session list (10.7). Then, on a
launch comment, in order (every state change goes through a CLI command, so it is a ledger
event; chapter 10):

1. Read `launches/<id>`. Proceed only if it was written by the owner.
2. Re-read the issue on GitHub; refuse if it is closed, or carries the in-progress label set
   by a person.
3. `issue-dashboard reserve --launch <id>`: one ledger transaction checks that no live
   reservation exists for the issue (or any batch member) in **any** project, and that the
   recipe's pool has a free slot (10.7). It returns a token, or `queued`, or a refusal with
   its reason, which the dispatcher posts in the launch thread.
4. `issue-dashboard brief --launch <id>` returns the brief (7.5); start the session with the
   routed model and effort and the token in its title.
5. `issue-dashboard launch-started --token … --session …`, and reply in the thread with a
   link to the session. In phase 1 the tracker writes nothing to GitHub but closes (10.10); the launched session
   follows its repo's own process, including any in-progress label (10.0). In phase 2 the
   write engine can take over the label, under the repo's write policy (10.9).
6. When the app continues the dispatcher because the session ended its turn for good,
   `issue-dashboard launch-ended --token … --state landed|stopped|failed` (which also
   releases the slot), then `reserve` for the next queued launch.

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
| triage session | Sonnet | medium; a judgement it cannot settle is recorded as "needs a stronger look", and the dispatcher (which runs on the strongest model) launches a short stronger session for those items, since a session cannot start one above itself |
| review of an outside pull request | strongest | high |
| `decision` type (design question) | the best design model | high, design only, no code |
| "send upstream" | Opus | medium |

The tier comes from the triage session's estimate for the issue. The dashboard shows the
routing with a one-line reason before launch, and you can change it; a change is recorded
(5.7) so the routing can be tuned from evidence.

### 7.5 Briefs

A brief is built **by the CLI from a fixed template** (`issue-dashboard brief`), not by a
model, so its trust boundaries are enforced by code. The same command produces the dashboard's
preview (10.8). Every piece of text in it carries a **source tag** from the ledger: tracker
(the template, config, derived facts), inside author, outside author, or private advisory
(4.5), which is placed only in that advisory's own brief. The template never
places outside text anywhere but inside a fenced, labelled block, and the task sentence is
built from tracker facts only: it names the issue by number and kind ("Fix
yuvalrakavy/airzoned#41, a bug"), never by an outside author's title. A structured filing
block is trusted only when the issue's author is inside; otherwise it is quoted as reporter
material like any other outside text. Its sections:

1. **The task** in one sentence, and what done looks like for this issue.
2. **The issue**: labels and links to what it blocks and is blocked by, then the title and
   body. Text by inside authors is quoted as is; **text by outside authors is fenced and
   headed with its author and association**, with the rule from 4.2 stated in the brief.
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

---

## 8. The CLI

One binary, `issue-dashboard`, with every subcommand taking `--root <dir>` (or
`$ISSUE_DASHBOARD_ROOT`) and, where it acts on one project, `--project <name>`. It never
writes to GitHub in phase 1 at all; from phase 2 only through `apply` and `label-sync`. It
never touches the dashboard
database (7.2).

| Command | Does | Writes |
|---|---|---|
| `check` | the config check (1.9) | nothing |
| `cycle --pages <file>` | the cycle of 10.4: ingest the fetched page documents, observe, derive the outbox | the ledger; `outbox/` |
| `snapshot [--full]` | the observe step alone (chapter 2), for testing and `doctor` | the ledger |
| `sync-now [--issue <owner/repo#N>]` | a sync request (7.2): the next run reads the item by node id; prints the scheduled task to run now | the ledger |
| `plan`, `apply <plan> [--yes]`, `undo <run>` | **phase 2** (10.9): the write engine | the ledger; GitHub |
| `reserve`, `launch-started`, `launch-ended`, `release` | the launch reservation steps (10.7) | the ledger |
| `brief --launch <id>` | the brief from its template (7.5) | nothing |
| `record-suggestions <file>` | the triage session's suggestions, validated against their schema | the ledger |
| `rebuild` | rebuild the SQLite ledger from the JSON Lines history (10.2) | the ledger |
| `explain <owner/repo#N>` | how each value was reached (1.9) | nothing |
| `label-sync [--dry-run]` | **phase 2**: create in each member repo the translated labels it lacks; never renames or deletes; shows its plan first | GitHub; the ledger |
| `forks` | chapter 3 for each checkout with a manifest | the ledger |
| `eval <decision> --backend <name>` | score a decision backend on the project's history (7.1): accuracy and calibration | `state/evals/` |
| `record-outcome --launch <id> …` | what a session reports at its end (7.7) | the ledger |
| `doctor` | checks the installation end to end: `gh` authenticated, each repo readable, each checkout present, last snapshot and sync times, whether a dispatcher answered recently | nothing |

**Exit codes:** 0 done; 1 refused by a check (the message names the file and key, or the
issue); 2 a GitHub or file error. `snapshot` exits 0 when some repos failed to read, because a
failed repo keeps its last good data (2.5); it reports each failure, and `doctor` turns them
into a non-zero exit.

**Logs:** structured, through the fleet's logging library, one line per GitHub write with the
run id, so the ledger and the log tell the same story.

**Tests** run against the replay `GitHub` implementation (2.7). The two commands that write to
GitHub, `apply` and `label-sync`, are also tested end to end against a scratch repository
created for the test and deleted after it.

---

## 9. Documentation and skills

**Where each piece lives:** the code, its docs and its skills live in this repo, since none of
them names a project. Each installation's configuration and state live in its tracker root.

| Piece | Audience | Contents |
|---|---|---|
| `README.md` | someone deciding whether to use it | what it is, what it needs (`gh`, Claude Code with artifacts), a five-minute setup from an empty tracker root to a first dashboard |
| `docs/configuration.md` | the person writing a registry or project config | every key of chapter 1's three layers, with defaults, and the config check's messages |
| `docs/operating.md` | the maintainer running it | the scheduled jobs, starting the dispatcher, what each dashboard state means, recovering from a failed run, `undo` |
| `docs/spec.md` | contributors | this document; its as-built notes are added as each phase lands |
| `examples/` | everyone | a registry and a project config for a small fictional project, checked by `check` in CI so they cannot rot |
| skill `issue-dashboard-triage` | the triage session | how to read the snapshot, the decision seam, duty-0 checks against the code, defect classes and batches, outside text as data (4.2), and never writing without an approved plan |
| skill `issue-dashboard-dispatcher` | the dispatcher session | the launch steps (7.3), the model-ceiling self-check, the claim protocol, brief assembly (7.5) |
| skill `issue-dashboard-sync` | the sync job | its two calls, and that it never interprets what it carries |

The skills are installed into a user's skills directory by `issue-dashboard install-skills`,
which copies them from the installed version so the skills and the binary always match.

**Changes outside this repo,** for this installation, each its own small change in its own
repo once the tracker works:

- the `ways-of-working` skill's label rules point to the tracker for priority (1.5) and to
  the structured filing block for new issues;
- Store's CLAUDE.md gains a catalog row for the tracker;
- each member repo that wants areas, a launch recipe or brief docs gains its
  `.issue-dashboard.toml`.

---

## 10. State and coordination: the event ledger

Five parts act on shared state: the CLI, the sync job, the triage session, the dispatcher and
the page. This chapter is the one design for how they coordinate; the earlier chapters
describe state in its terms. It was added after the first design verification (2026-10-23),
whose serious findings shared that missing design as their cause, and revised after the
second.

### 10.0 Phases

| Phase | Toward GitHub | What it contains |
|---|---|---|
| **1** | **read-only, but for one write: the owner's close of an issue from the page (10.10)** | the ledger, the cycle, the snapshot, the dashboard, triage suggestions, priorities and overrides kept in the tracker, views, forks, launches and reservations, briefs, outcomes, `explain` |
| **2** | the write engine (10.9): labels and priority write-back, label sync, plans, undo | built test-first against a scratch repository, so its timing questions are answered by running code and its review is a review of code |
| **3** | closes, comments and reviews in the maintainer's name | the open-source writes of chapter 4 |

**The one exception: closing an issue** (owner, 2026-10-23: "I need to be able to close an
issue from the dashboard"). Phase 1 closes, and reopens on Undo, an issue the owner closed on
the page: a deterministic CLI step with no model, under 10.9's intent-then-result rule (10.10,
*Closing from the page*). Nothing else is written to GitHub in phase 1.

In phase 1 a launched session still works on its issue the way it does today, under its
repo's own process (the in-progress label, its commits, its pull request); the tracker
itself does not write. Everything in this spec about writing to GitHub (1.5's write-back,
1.7, label sync, 5.8's labels "written on the next run", undo) is **phase 2 or 3**; in
phase 1 an accepted label or priority is recorded in the tracker and shown, and the
dashboard says it is not written to GitHub yet.

### 10.1 Who writes what

| State | Written by | Read by | On retry | On a crash midway |
|---|---|---|---|---|
| the **ledger** (`<root>/ledger.sqlite`) | CLI commands, each under the root lock (10.4) | CLI commands | every event has an identity, so a repeat is a no-op (10.3) | a transaction committed or did not |
| the ledger's **backup** (`<root>/ledger/*.jsonl`) | the same command, before it releases the lock (10.2) | `rebuild` | appends from an export cursor, so a repeat writes nothing twice | SQLite is the authority; the next command exports what is missing |
| page documents (`decisions`, `launches`, `views`, `requests`) | the page | the sync job, then the CLI | the document id is part of the event identity | the document stays; ingested next cycle |
| tracker documents (everything else in the database) | the sync job, carrying an outbox | the page | an outbox is written only if its sequence is newer than the published one (10.6) | the page shows the previous outbox |
| sessions (fix, review, triage) | the dispatcher, through `start_session` | the dispatcher | a launch holds a reservation first (10.7) | the reservation stays held until the dispatcher reconciles it (10.7) |
| comments in the launch thread | the page (requests) and the dispatcher (replies) | the dispatcher, you | replies carry the launch id | none; the thread is a notification channel, not state |
| the app's session list | the app | the sync job (`list_sessions`), then the CLI (`cycle --sessions`) | a session whose content is unchanged records nothing (10.3) | nothing is recorded; the next cycle reads a newer list |
| GitHub | **nobody, in phase 1**; the write engine, in phase 2 (10.9) | the snapshot | — | — |

**The rule:** no ledger state changes except through a CLI command. The triage session and
the dispatcher change it by calling commands (`record-suggestions`, `reserve`,
`launch-started`, `launch-ended`, `record-outcome`), never by editing a file or a database
document.

### 10.2 The ledger

An append-only log of events in SQLite (WAL mode) in the tracker root. The tracker's derived
values (priorities per project, the queue, the dashboard's documents) are computed from the
events plus the configuration in force, which is itself recorded as an event, so a derived
value can be recomputed for any point in the log.

**Backup.** Each mutating command, before it releases the root lock, appends the events it
committed to monthly JSON Lines files (`<root>/ledger/2026-10.jsonl`) and fsyncs them, then
advances an export cursor stored in the ledger. Each line holds one event with its sequence
number and a checksum of the line. On start, any command first checks the file's tail:
a final line that does not parse or fails its checksum (a torn write) is cut off (as built,
the whole trailing run of such lines, 10.10), events whose
sequence number is already in the file are not appended again, and events past the cursor are
exported. So a crash between the append and the cursor update neither loses nor duplicates an
event. The files are committed to git; `issue-dashboard rebuild` recreates the SQLite file from
them, verifying every checksum and refusing, with the line number, on a corrupt line in the
middle of a file. When the two disagree, SQLite is the authority and the files are re-exported
from it.

**Identity.** Repos, issues and pull requests are stored by **GitHub's permanent node ids**.
Each observation records the current `owner/repo#N` as an alias, so a renamed repo or a
transferred issue keeps its history; display, links and database document ids use the node id
plus the alias (5.3). An item GitHub no longer returns is confirmed on the daily full read and
recorded as `item_gone`, which removes it from every derived view; its events stay in the log.

### 10.3 Events

Every event has a **sequence number**, a **time**, a **kind**, a **subject** (the node id,
launch id or decision id it concerns), and an **identity**: a hash, over a canonical JSON
serialisation (sorted keys, no whitespace), of the kind, the subject, the **sequence number of
the subject's previous event of that kind** (0 for the first), and the fields that make this
occurrence distinct. Recording an event whose identity exists is a no-op; that is what makes a
repeated command safe. Because the predecessor is part of it, a value that returns to an
earlier state (a configuration A → B → A, a label removed and put back) is a new occurrence and
is recorded; a retry of the same command against the same predecessor is not.

*Correction (build, 10.10):* the identity alone cannot make a retry a no-op once the first
attempt has committed, because the retry then sees that attempt as its predecessor and computes
a new identity. So a repeat is decided by the distinguishing fields: for state-like kinds
(`config`, `observed`, `exported`) an event equal to the subject's **latest** of its kind is a
repeat, so A → A is a no-op and A → B → A is recorded; for kinds whose fields never recur
(`suggested`, `item_gone`, `outcome`) an event equal to **any** earlier one is a repeat;
`decided` is distinguished by the ingesting project and its verdict (10.10); `launch` events are
checked against 10.7's table before they are recorded. The identity
stays unique per event and is what the backup and `rebuild` verify.

| Kind | Identity is the hash of | Recorded when |
|---|---|---|
| `config` | the configuration's content | a command sees a configuration it has not recorded |
| `observed` | node id + a hash of the observed content (labels, state, assignees, links, the timeline event ids read) | a read returns an item whose content differs from its last observation; **the update time is not part of it**, so a change that does not move `updatedAt` is still recorded |
| `item_gone` | node id + the read that confirmed it | 10.2 |
| `decided` | the page document's id | ingest reads a page decision; it carries the author's user id (10.5) |
| `view_saved`, `request` | the page document's id | ingest reads a saved view or a request (a brief preview, on-demand triage) |
| `suggested` | triage run id + item + decision kind | the triage session records a suggestion |
| `launch` | launch id + transition (`requested`, `reserved`, `queued`, `started`, `ended`, `released`, `unconfirmed`, `withdrawn`, `orphan`, `orphan_settled`) + token (+ the session, for `started` and the orphan transitions) | 10.7; only the transitions in 10.7's table are legal, and a command refuses any other |
| `outcome` | launch id | a session records its outcome |
| `exported` | the export cursor's new value | 10.2 |
| `session_seen` | session id + a hash of the observed fields (title, working directory, branch, pull-request number, running, the hour of the last activity) | a cycle's session list shows a session unarchived whose content differs from its latest `session_seen`, or which was gone since |
| `session_gone` | session id + the `session_seen` it ends | a session seen before is archived, or is missing from a list that would have shown it (10.10) |

Phase 2 adds the write events (10.9). The full field list per kind is the build's first
artifact: JSON Schema files in `schemas/`, which the CLI, the sync job and the page validate
against.

### 10.4 The root lock and the cycle

**Every mutating command takes the root lock** (an exclusive lock on `<root>/lock`) for the
time it writes the ledger, and holds it only for that: **no network call is made while the
lock is held.** A command that needs network data (the snapshot) fetches first, then takes the
lock and records. So a dispatcher's `reserve` waits at most for one short ledger write. A
command waiting for the lock gives up after 30 s with a message naming the holder's command and
process id, so a stuck holder is visible rather than a silent hang.

`issue-dashboard cycle --project P --pages <file>` runs, in order:

1. **ingest** (under the lock): the page documents the sync job fetched become `decided` and
   `launch` events, and their acknowledgements are prepared (10.6);
2. **observe**: the snapshot reads GitHub (chapter 2) without the lock, then records
   `observed` and `item_gone` events under it. **A stale fetch never overwrites a fresher
   one:** when two projects' cycles read the same repo, each observation carries the item's
   GitHub `updatedAt` (and the read time), and an observation older than the item's latest
   recorded one is discarded at commit, not appended;
3. **derive**: take the next outbox sequence number under the lock, then compute the views
   and write the outbox **outside** it, from one SQLite read transaction (a consistent
   snapshot), so derivation never holds the lock.

**What runs under the lock is small and bounded:** recording events and exporting them
(10.2). Every lifecycle command (`reserve`, `launch-started`, `launch-ended`, `release`) is
idempotent, and the dispatcher retries one that timed out on the lock, up to five times with
backoff; if it still fails, the dispatcher posts the failure in the launch thread and the
dashboard shows the launch as needing attention, so a held slot is never silent.

Phase 2 inserts its reconcile, plan and apply steps between observe and derive (10.9).

The sync job runs: fetch page documents → `cycle` → publish the outbox. So "on the next run
(in N minutes)" means the next sync cycle. The triage session never applies anything: it
records suggestions, and the next cycle shows them.

### 10.5 Decisions: authorship, ordering and undo

- **Only the owner's decisions count.** A `decided` event carries the author's user id from
  the page; ingest accepts it only if that id is the project's configured `owner_id`, and
  records any other as refused. The database's access rules already restrict page writes to
  the owner (5.3); this check does not trust them alone.
- **Undo is a decision that names another decision.** If the named decision is not yet
  ingested (the two arrived in different fetches), the undo is recorded anyway, and the named
  decision is marked cancelled the moment it is ingested, before it has any effect. Order of
  arrival does not matter.
- **Applying is deterministic.** Decisions take effect in sequence order; two decisions about
  the same item resolve to the later one.

### 10.6 The outbox and acknowledgements

Each cycle writes `<root>/outbox/<project>/<seq>.json`: the complete set of tracker documents,
stamped with the cycle's sequence number, plus the acknowledgements of the decisions it
consumed (the `acks` collection: decision id → cycle). **The tracker never edits a page
document**; the page shows a decision as pending until its ack arrives.

An outbox is larger than one database batch can carry (a batch holds at most 50 documents),
so it is published as an **immutable generation** and switched on by one pinned write:

1. read the `meta` document, which names the active generation and its sequence;
2. if the outbox's sequence is not greater, stop: a newer one is already published;
3. write the outbox's documents under `gen-<seq>/…`, in as many batches as needed; nothing
   reads them yet;
4. write `meta` alone, pinned to the version read in step 1, naming `gen-<seq>` as active;
5. delete the generation that was active before, once the switch has succeeded.

The page reads only the generation `meta` names, so it never sees half an outbox. Two
overlapping syncs cannot both switch: the second's pinned write in step 4 fails because the
first changed `meta`, and an older outbox is stopped at step 2 or step 4.

**Cleanup keeps the previous generation.** Step 5 deletes only generations older than the one
that was active before the switch, so a page that read `meta` just before the switch can still
finish reading. A page that finds a document missing re-reads `meta` and starts over. A
generation that never became active (a sync that died after step 3) is deleted only once it is
more than one hour old, so a publisher still uploading is never cut off. Only one sync job runs
per project (it is one scheduled task), so overlapping publishers arise only from a slow run
meeting the next one, which the pinned switch already settles.

**Acknowledgements are cumulative.** Every generation carries the acks of every decision
consumed in the last 30 days, not only the last cycle's, so a page that missed a generation
still sees each decision acknowledged.

Launch status shown on the page lives in a tracker-owned `launch_status` collection; the
page's own `launches` documents are requests and are never changed after they are written.

### 10.7 Launch reservations

A launch moves through these transitions, each a `launch` event, and only these:

| From | To | By |
|---|---|---|
| — | `requested` | ingest of the page's launch document |
| — | `requested` | `reserve --request <file>`, which records the launch document the dispatcher just read (its id is the identity, so the later ingest of the same document is a no-op) |
| `requested` | `reserved` (with a token) or `queued` | `reserve` |
| `queued` | `reserved` | `reserve`, when a slot frees |
| `reserved` | `started` (with the session id) | `launch-started` |
| `reserved` | `unconfirmed` | the owning dispatcher, on restart, when its start call never returned |
| `reserved`, `unconfirmed` | `released` | the owning dispatcher, when its session list shows no session with the token and the reservation is older than 15 minutes |
| `unconfirmed` | `started` | the owning dispatcher, when its session list shows the session |
| `started` | `ended` (landed, stopped or failed) | `launch-ended` |
| `ended` | `released` | the same command, freeing the slot |
| `requested`, `queued` | `withdrawn` | a page decision cancelling the launch |

**Retries and orphans.** `launch-started` with the token and session id already recorded is
a no-op (it returns the recorded result). Only a session reporting a token that is
`released`, or a different session id for a `started` token, is recorded as an `orphan`
**alongside** the launch, not as its state: the dashboard shows it in the Inbox as possible
duplicate work, and an orphan holds a slot until you settle it.

**Ownership.** A reservation records the dispatcher session that made it. Only that
dispatcher settles it from its session list, since only it is guaranteed to see the sessions
it started; another dispatcher leaves it alone, and the dashboard shows it as held by that
dispatcher. A session list is used as evidence of absence only for a reservation older than
15 minutes, well past the time a `start_session` call takes to return.

**A slot is freed only by a release, never by time.** `reserve` checks, in one ledger
transaction across all projects, that no reservation for the issue (or any member of a batch)
is in `reserved`, `unconfirmed` or `started`, and that the recipe's pool has a free slot.

**The dispatcher is the only one who can see sessions**, so it, not the cycle, settles
uncertain reservations: on its start, and whenever it is woken, it lists its sessions and, for
each `reserved` or `unconfirmed` token, records `started` if a session carries the token in
its title and `released` if none does. A session that was started under a token that has
since been released reports `orphan` through `launch-started`; the dashboard shows it in the
Inbox as possible duplicate work. A reservation that stays `unconfirmed` because no dispatcher
is running is shown on the dashboard with its age, and you can release it.

**The queue's wake source:** the app continues the dispatcher whenever a session it started
ends a turn; it then records `ended` and `released`, and reserves the next queued launch.
Releasing an unconfirmed reservation is also followed by a `reserve` for the next queued one.

### 10.8 What wakes each piece of work

| Work | Woken by |
|---|---|
| a cycle | the sync job, every `sync_interval`; sooner when a session that ran `sync-now` runs the sync task now (7.2) |
| a page sync request (Sync now, Refresh) | the next scheduled run in phase 1, since the page cannot wake the task; once the dispatcher exists, it runs the sync task when it sees the request |
| a brief preview | a `requests` document; the next cycle builds the brief (7.5) into the outbox |
| a launch | the page's `sendToClaude` comment (5.4) |
| the next queued launch | the app continuing the dispatcher when one of its sessions ends a turn; or a release |
| settling uncertain reservations | the dispatcher's start, and every time it is woken |
| triage | the daily schedule, or a `requests` document launched like a fix |
| a stronger look at a hard triage call | an Inbox item "needs a stronger look", whose button is an ordinary launch at the routed model |

### 10.9 Phase 2: the write engine

Recorded here so phase 1 leaves room for it; its exact rules are settled by its test-first
build against a scratch repository, since GitHub offers **no conditional write for labels**:
any re-read-then-write leaves a window in which a person's edit can land, and the design must
state that residual rather than claim it away. What phase 2 must provide, and its tests must
show:

- **writes triggered only by decisions**, never by an observation, and for a shared repo the
  highest project priority, written only when that maximum changes (1.5);
- **preconditions** on every operation, checked immediately before the write, with the
  remaining window measured and reported;
- **approval bound to an operation's content**;
- **intent and result as separate committed events**, each operation under the root lock for
  its own intent, write and result, so a crash is reconciled from operation-specific evidence
  (the label event's timeline id, a comment's id), not from the issue merely showing the
  target state;
- **telling the tracker's own writes apart** by the timeline event ids its requests produced,
  reconciled before the cycle classifies other observations;
- **undo** only where a fresh read of the item's full timeline shows no other change to the
  same label or state since the operation.

### 10.10 As built: phase 1

Notes added while building phase 1, for the owner to review. Each is either a choice the spec
left open or a correction to it.

**Schemas (`schemas/`).** One JSON Schema (draft 2020-12) per event kind, page document,
tracker document, the filing block and each config layer; `schemas/examples/<schema>/` holds
valid examples and `bad-*` ones that must be refused, checked by the test suite. Names the
spec mentions but does not spell, chosen by the build and reversible until something consumes
them: in the project config `owner_id` (10.5), `time_limits_days` (per priority, 2.3),
`sync_interval_minutes` (7.2), `routing` (by launch kind, 7.4); the page decision's `action`
values and the launch document's `kind` values; a launch document names its `recipe`, whose
pool it takes a slot from. The filing block allows unknown keys, since 2.6 keeps and reports
them rather than refusing.

**The ledger (`crates/core`).**

- *Repeats:* see the correction under 10.3.
- *Append-only* is enforced by SQLite triggers that refuse an `UPDATE` or `DELETE` of an event.
  Sequence numbers are contiguous from 1; `rebuild` checks that.
- *Checksum:* each backup line's checksum is the sha256 of its event's canonical JSON, so a
  change of whitespace in a line is harmless and a change of content is caught. `rebuild` and
  the tail check also recompute each event's identity, and `rebuild` checks each predecessor.
- *The export cursor* is the value of the latest `exported` event. The backup is always
  appended from its own last sequence number, not from the cursor, so a crash between the
  append and the cursor update (or a lost cursor) can neither duplicate nor skip a line. An
  `exported` event is recorded only when non-`exported` events passed the cursor, so a command
  that records nothing adds nothing; the `exported` event itself reaches the backup with the
  next command.
- *Files:* an event goes to the file of its month, or to the latest file if the clock has
  stepped back, so the files in name order are in sequence order and only the latest file is
  ever appended to.
- *Torn tail:* one export writes its lines under one fsync, so a crash can garble several
  trailing lines of the latest non-empty file. Every trailing line that fails to parse or verify
  is cut back to the last good one (SQLite holds their events and re-exports them); an
  unterminated final line that still verifies is kept and terminated.
- *Disagreement:* every mutating command verifies the whole backup against SQLite (each line
  equal to SQLite's event of that sequence number, contiguous from 1). If they differ anywhere,
  the backup files are moved to `ledger/diverged-<time>/` and re-exported from SQLite, which is
  the authority. **Open:** this reads the whole backup and ledger on every command; a manifest
  or hash chain should replace it before the history grows large.
- *A ledger missing or behind its backup* (a fresh clone, where only the backup is in git; a
  lost file; an older copy restored) is refused, naming `rebuild`, and never opened empty: a
  command that re-exported from it would set the newer history aside.
- *`rebuild`* refuses when `ledger.sqlite` exists unless `--force`, which keeps the old file
  as `ledger.sqlite.before-rebuild-<time>` (made unique). Nothing of the old database is deleted,
  copied or checkpointed: the main file and any `-wal` / `-shm` beside the target are renamed
  aside together (main file first), then the new file is renamed in. A crash in between leaves
  no `ledger.sqlite`, which the next command refuses, naming `rebuild`; a stray WAL is never
  left beside the new file. `rebuild.pending` names the aside set while the old files move, so
  a move interrupted by a crash resumes into the same set. (Two earlier shapes, copy-aside with
  a checkpoint, kept drawing findings in the gate: an incomplete checkpoint, a WAL removed before
  the rename.) An existing ledger is opened without SQLite's create flag. *Readers:* every
  lock-free read (`Root::read`, and the cycle's derive) holds a shared lock on `<root>/readers`
  while its connection is open; `rebuild` takes it exclusively after the root lock, waiting up to
  the lock wait and then refusing with `ReadersActive`, so no reader holds the old file across
  the renames. (This closes the open item slice 1 left here.)
  `rebuild` tolerates a torn tail by the same rule as the tail repair (the trailing run of bad
  lines of the latest non-empty file) and reports how many lines it dropped; a corrupt line
  anywhere else, that is one followed by a good line, refuses with its file and line number. A
  read that fails (permissions, a failing disk) is an error, never taken for divergence.
- *Schema version:* the ledger keeps its version in SQLite's `user_version`; a version this build
  does not know is refused.
- *Validation:* every event a module records is checked against its kind's schema before it
  is inserted, and each config layer against its schema when it is loaded, so values
  serde's types cannot express (a capacity of 0, `exclude` without `group`, an empty session id,
  a launch id outside the document-id pattern) are refused, naming the file or the event.
- *Time:* a command's time is read once it holds the root lock, and `reserve` loads the config
  under the lock, so neither can be older than what the previous holder recorded.
- *Stale observations:* compared by GitHub's `updatedAt`, then by read time, at full precision,
  so of two reads with the same `updatedAt` (a change that did not move it) the earlier read is
  the stale one. An unchanged read that is fresher still advances the item's freshness, kept in
  a `freshness` table beside the events (derived, not in the backup); so an older read that
  commits after it is stale too. A change of alias alone (a rename or transfer) is recorded.
  **Accepted residual (for the owner to confirm):** the freshness of an unchanged read is not in
  the backup, so after `rebuild` an older read taken before the rebuild and committed after it
  could be recorded over an unchanged fresher one; the next read records the truth again.
  Closing it would take an event per unchanged read.
- *Launches:* 10.3's transition list lacked `withdrawn`, which 10.7's table has; it is added,
  with `orphan_settled`, which frees the slot an orphan holds ("until you settle it", 10.7). A
  recipe's pool is keyed by the recipe's name, across every project and repo using it, so a
  recipe shared by several repos shares one pool (**owner, 2026-10-23**: capacity is about how
  many sessions run on this machine, not per repo). The queue is first
  in, first out: a new request queues behind an earlier queued launch of the same recipe that
  could take the slot. `reserve` retried returns the recorded token; another dispatcher is
  refused. A held issue refuses the reservation without changing the launch. `launch-ended`
  retried is a no-op, and with a different end state is refused. `release` needs the owning
  dispatcher and a reservation older than 15 minutes. `unconfirm` records `reserved` →
  `unconfirmed`. A session that reports an orphan again, before or after it is settled, is a
  repeat, and settling twice is a repeat. `reserve --next` skips a queued launch whose recipe can
  no longer be resolved, and refuses with that reason only when nothing could be reserved, so
  an unresolvable queued launch is never silent; any other refusal stops it.
  `launch-started` is authorised by the token alone, not by a dispatcher id (the dispatcher holds
  the token); so `unconfirmed` → `started` does not check the owner.
- *Decisions:* a decision naming another project is refused like a non-owner's, but a refusal
  does not consume the document: ingested again with the right project or a corrected
  `owner_id`, it is accepted and takes effect at that point in the sequence. Once accepted, it
  stays accepted. A different document under an ingested id is refused. An undo cancels
  its target only while the undo is not itself cancelled, so undoing an undo restores the
  decision; status is derived from every ingested undo whenever decisions are read, which is
  what makes arrival order irrelevant.
- *Outcomes:* one per launch; the same record again is a no-op, a different one is refused.

**The cycle (`cycle`, `snapshot`; slice 2).** Phase 1 reads GitHub and never writes to it.

- *GitHub access* is one trait, `GitHub::graphql`, with three implementations: `gh api graphql`
  (the real one), `Recorder` (the real one, saving each response) and `Replay` (recorded
  responses, keyed by the query name and its variables with `since` dropped and `closed:>=`
  dates blanked, so a recording answers a later clock). A search recording keeps its own
  `closed:>=` date and answers only that window or a shorter one: a longer window would need
  closed items it does not hold, so it is an error. `nodes` requests are keyed by their
  exact ids; the tests answer them from any node found in the recordings. The CLI picks one by
  `ISSUE_DASHBOARD_REPLAY` / `ISSUE_DASHBOARD_RECORD`; `ISSUE_DASHBOARD_PAGE_SIZE` sets the page
  size (default 100). Every request sends `X-Github-Next-Global-ID: 1`: without it GitHub still
  returns legacy node ids for older repos (rhaiscript/rhai's is `MDEw…` without it, `R_kgDOAyYOjQ`
  with it), and the ledger keys everything by node id. A `NOT_FOUND` comes back as `null` with an
  error and `gh` exits 1 with the JSON on stdout; the reader accepts it only where `null` is a
  valid answer (`nodes`, an `also` number). GitHub refuses a query that defines a fragment it
  does not use, so fragments are appended per query.
- *Membership* is a new event kind, `membership`: subject the repo's node id, one per project
  and set, recorded only when the set or the repo's name changes. The since-lists are read
  unfiltered, and the record step keeps the details of an item that this read's set holds, or
  the latest set of another project that is configured to read the same repo, matched by node
  id (that project's last read of the name it configures), not by name, since a project may
  still name a renamed repo by its old name. The per-repo watermark is shared. An item that
  enters a set is read by node id; when another project's read has already moved the watermark
  past this read's start (less the overlap), a change between the two would be in no later
  since-list and was dropped by that read (no set held the item yet), so the entrants go into
  `rereads` and the project's next read reads them by node id again.
- *The read state* lives in derived side tables beside the events (not in the backup, like
  `freshness`): `watermarks` (per repo node: the start of the last complete read) and `reads`
  (per project and configured repo: node id, view hash, mode, attempt, last good and last full
  read with the ledger seq it was recorded at, error) and `rereads`. After `rebuild` they are
  empty, so the next read of each repo is full. A ledger whose `reads` table predates the
  `full_seq` column has it dropped and recreated when the ledger opens: it is derived, so the
  cost is one full read.
- *Full reads:* the first read, a changed view or translation (hashed), a missing watermark,
  and once every 24 hours. A full read reads the whole set by node id and checks every item that
  was in the set at any time since the last full read and is not now (including items that left
  at an incremental read), recording `item_gone` for those GitHub no longer resolves. "Since the
  last full read" is by ledger order: the sets from the one current at the full read's seq on,
  never a time compare (event times carry milliseconds, read times do not). An item that
  resolves elsewhere (transferred) is observed under its new alias, not gone. An item entering
  the set that GitHub does not resolve is not a member (it enters when it resolves). A member
  the search still finds but GitHub does not resolve by id leaves the set at whichever read
  meets it; a full read records it `item_gone`. A later read meets it as an entrant that does
  not resolve, so it is recorded gone once.
- *The read report:* `entered` counts only true entrants (in the set now, not in the previous
  set, and resolved), on a full read as on an incremental one, though a full read reads every
  member by id; `left` counts the previous members not in the set now; `gone` the `item_gone`
  events this read recorded.
- *Renames:* searches, `also` numbers and the since-lists use the repo's current
  `nameWithOwner` from the repo query, not the configured name, so a renamed repo keeps its
  items. Config keeps naming the repo by its old name until the owner edits it.
- *Stale fetches:* a fetch whose start is older than the project's last good read of the repo
  (`superseded`) offers only its observations, each checked for staleness, and records neither
  its set nor its read times; a superseded fetch that failed records nothing, so the newer
  read's status stands. The watermark only moves forward.
- *Ingest* (page documents, passed to `cycle --pages <file>` as `{decisions, launches, views,
  requests}`; an unknown collection is refused): launches first, then decisions, then the
  cancels, then views and requests. A document that fails its schema or the owner check is
  skipped and reported; one bad document never stops the rest. `not_sent` launch records and
  thread records are not ingested in phase 1. Views and requests become `view_saved` and
  `request` events, one per document id.
- *Cancels* are applied by `apply_cancels`, run on every ingest and after `reserve --request`,
  which withdraws each requested or queued launch an effective `cancel_launch` names, whichever
  arrived first. A launch already reserved is past cancelling: the decision's ack says refused.
  `reserve --request` for a withdrawn launch answers `withdrawn` on every try, so a retried
  command gets the same answer. *Undo too late:* an undo recorded after the withdrawal a
  `cancel_launch` caused is refused ("too late") when it would cancel that cancel, directly or
  through a chain of undos (undoing the undo of an undo), since a withdrawal is final.
- *The outbox:* the cycle's sequence number is the seq of a new `outbox` event (subject the
  project), so it rises and survives `rebuild`. Each cycle writes
  `outbox/<project>/<seq>.json` (temp file, fsync, rename) with the meta, issue, acks and
  launch-status documents, each validated against its schema. Acks are cumulative: every
  decision consumed in the last 30 days, with the cycle that consumed it (the first outbox after
  it). Derive runs after the root lock is released, over a read transaction begun while the lock
  was held, so it sees exactly its own commit.
- *Derived status* follows 4.4 for pull requests and then: umbrella, new, untriaged,
  needs-decision, in-progress, blocked, ready. `waiting-on-author` needs the changes requested
  on the head commit ("since the last push"): the read takes each reviewer's latest
  change-requesting review commit (`latestOpinionatedReviews`, probed on rhaiscript/rhai#1194,
  stored as `changes_requested_on`); an observation without it, or where GitHub answered the
  field `null`, keeps GitHub's aggregate `reviewDecision`. Only the latest 50 comments of an issue are read, so an issue with more is
  not "new" (an earlier inside comment may be unseen). Priority is override, then the project's
  priority, then the label. Closed items older than the history window are left out.
- *Page fields (slice 3):* the observation keeps the body, trimmed and cut at 8000 characters
  (`body_truncated` says so; `body_hash` stays the hash of the whole body), because Details
  shows it (5.x). Adding it changed every item's content hash once, so the first read after the
  upgrade records each item again. The issue document carries `body`, `body_truncated` and the
  raw `labels`; meta carries `groups` (each group a member names, with the tracked repos it
  holds) and the effective `sync_interval_minutes` (default 5), which the page uses for
  "applied on the next run (in N min)". The page shows a body as plain text, never as HTML
  (4.2). The issue document also carries `closed_at` (null while open), since Trends counts
  closes per week and `updated_at` moves on later activity.
- *The page database layout and publish (slice 3).* A program cannot write the artifact's
  database (claims table), so the sync job is a skill (`skills/issue-dashboard-sync`) that
  performs, with `ArtifactData`, what three CLI helpers plan, none of which needs a root:
  `pages` (the four page collections, listed with `out_dir`, into the `--pages` file),
  `publish-plan` and `cleanup-plan`. A generation is a marker `gens/gen-<seq>` (sequence,
  project, `created_at`, per-collection `counts`), written in the first batch, then every
  outbox document at `gens/gen-<seq>/<collection>/<id>`, in batches of at most 50 entries and
  900 KiB of files; then the switch: `meta/active` set to the outbox's meta, pinned to the
  version the skill read (unpinned on the first publish, which the database refuses if the
  document exists). An outbox no newer than the active generation is a stop that writes
  nothing; a marker for the outbox's own generation is refused (a name is never reused: a run
  that failed after its first batch is abandoned, and the next cycle has a new sequence).
  Probed on the page (2026-10-23): an unpinned write to an existing document is refused, an
  `out_dir` file holds only the document (its version comes in the tool's reply), and a pinned
  delete of a missing document fails its whole batch. So generation documents, created once
  and never edited, are deleted pinned to version 1, and cleanup is planned from a listing of
  what is there, which finishes a cleanup that stopped part way. Cleanup runs only after a
  successful switch and keeps the new generation and the previous active one; it deletes a
  generation older than the previous active one, or one whose marker is more than an hour old
  (abandoned), each generation's documents before its marker. A younger generation newer than
  the active one may be another run's and is kept. Access rule: everyone who can open the page
  reads, only the owner writes (`[{path: "", read: "view", write: "owner"}]`).
- *Closing from the page (slice 3, 10.0's exception).* A `close` decision carries
  `value.close = {reason: completed | not_planned | duplicate, duplicate_of, comment}`. The cycle
  gains a write step between ingest and observe, so the same cycle's read and outbox already
  show the result. The step holds the root lock across each operation's intent, GitHub call
  and result (10.9), each a committed transaction of its own, and takes the repo and number
  from the item's latest observation, never from the page's alias. Event kinds:
  `write_intended` (subject the decision; the operation, repo, number, reason, comment),
  `write_applied` (`closed`, `already_closed`, `reopened`), `write_failed` (stage
  `precondition` or `apply`, and why). Preconditions, refused as `write_failed` without an
  intent: the item is an observed issue (not a pull request) in one of the project's repos, and
  the repo is not read-only (declared, or the viewer's permission below write). Then a fresh
  read (`gh issue view`): already closed records `already_closed` and posts no comment;
  otherwise `gh issue close <n> -R <repo> --reason completed|"not planned"|duplicate`
  (`--duplicate-of N` for a duplicate) with `--comment`. A failure is recorded and not retried:
  the ack says refused with GitHub's error, and the owner can press Close again. A read that
  follows a write (the `closed_at` after the close, the precondition read before a reopen) is
  read again after 1, 2 and 4 s while it does not show what the write did, since GitHub's read
  can lag its own write: without it a lagging read records no `closed_at`, and every Undo of
  that close is refused. **A crash** leaves
  an intent with no result; the next cycle's step reconciles it first from a fresh read: closed
  at or after the intent's time is `closed` (reconciled); still open with a comment of the
  intended text posted after the intent is a close re-sent without the comment; otherwise the
  close is applied as new. **Undo.** An undo cancels its close; a cancelled close with no
  intent writes nothing, and one already closed on GitHub is reversed by a reopen, applied only if a fresh read shows the issue still closed at the close's recorded
  `closed_at` with no comment since, and only for a `closed` result (an `already_closed` one was
  someone else's close, so its undo is refused). Each issue document carries `dashboard_close`
  (the decision, its reason and time, and `undo_allowed` from the latest observation), and
  `meta.repos[]` carries `visibility` for the public-repo notice. **Residuals:** a person's
  comment or reopen landing between the fresh read and the write is not seen (GitHub has no
  conditional close); a crash after `gh` posted the comment and before it closed is caught by
  the comment-text match, but a person posting the same text in that window would make it
  skip the comment. The per-run cap of 1.7 does not apply: each close is one explicit decision.
- **Session tracking** (slice 3, owner-directed). `cycle --sessions <file>` takes
  `{"listed_at", "limit", "sessions": [...]}`, the rows exactly as `list_sessions` returns them
  (`sessionId`, `title`, `cwd`, `branch`, `prNumber`, `isRunning`, `isArchived`,
  `lastActivityAt`, `link`; other fields are dropped, and a row that does not validate is
  skipped with its reason). Ingest, in the cycle's first transaction: an unarchived row records
  `session_seen` unless the session's latest session event is a `session_seen` with the same
  content; an archived row, and a seen session missing from the list, record `session_gone`
  when its latest session event is a `session_seen`. The list is sorted by last activity and
  cut at `limit`, so a missing session is gone only when the list is complete (fewer rows than
  `limit`) or its last seen activity is no older than the oldest listed row: a session that
  only fell off the end stays as it was. The hash takes the last activity to the hour, so a
  running session records at most about one event an hour, not one a sync. Matching is in
  derive, so a change of config re-matches every session, and it is deterministic. A session's
  **repo** is the project member whose `local_path` is its working directory or a parent of it
  (a worktree under the checkout); the longest such path wins. **Exact:** the session is a
  launch's (its id is the launch's `started` session, or its title holds the launch's token):
  the launch's items; or its `prNumber` is a pull request of its repo in the project: that pull
  request and the issues it closes. **Probable** (only when there is no exact match): the title
  and branch name one item: `owner/repo#N` (a member repo), and against the session's repo
  `#N`, `gh #N`, `issue N`, and `issue-N` or `gh-N` in a branch name; two different items, a
  reference with no repo to resolve against, or a number not in the project match nothing.
  Each issue document carries `sessions`: id, title, running, last active, link, and the match
  (`exact` via `launch` or `pull_request`, `probable` via `title` or `branch`), newest first.
  As built, the working directory is kept in the ledger for matching (the page never gets it);
  the rest is the list above. `list_sessions` leaves out the session that calls it, so a sync
  run by hand from a session shows that session as gone until a later run lists it; the
  scheduled task's own session works on no issue. Without `--sessions` nothing is ingested and
  nothing is taken for gone. **Page:** a card or Queue row shows its newest session ("In
  progress", the title linking to the session, running or idle, how long since it was active,
  and "probably" for a probable match) and a count of the rest; Details shows each, with its
  link as selectable text and a Copy link button, since a `claude://` link may not open from
  the page. The Queue's "In progress" pill keeps the items with a session or the
  `in-progress` status.
- *Corrections from slice 3's gate (round 1).* A refused send's `not_sent` outranks the
  tracker's `requested` (every ingested launch is `requested`, heard or not); only a later
  transition supersedes it. Each launch thread gets its own thread record (`T_…`, written once;
  the first slice's rewritten `_thread` still reads, as the oldest). The page reads the current
  thread before it writes the launch document, so a failed read records nothing, and a thread
  record that fails after a send that succeeded is reported, not turned into `not_sent`. A
  decision older than the ack window (`ACK_DAYS`, 30 days before the generation was taken) with
  no ack is settled, not pending: its ack has aged out. A generation announced while the page
  loads another is loaded after it. Page ids carry 8 random base-36 characters (there is no
  create-only write, so the randomness keeps two writes in one second apart).
- *Corrections from slice 3's gate (round 2).* A close older than the ack window is refused,
  not made: the page stopped showing it as pending. A crash's dangling close intent on an issue
  still open is closed only if the decision still stands and its preconditions still hold (an
  undo since, or a repo made read-only since, records the close refused). Of two closes of one
  issue, the one that closed it stays the one shown, with its Undo, over a later one that found
  it already closed. Left open (S1): a close by someone else inside the moment between the
  fresh read and `gh issue close` is recorded as ours, as is a close inside a crash's gap; the
  intent's time is the local clock, compared with GitHub's.
- *Sync now (after slice 3).* A sync request is a `request` event whose document has
  `kind: "sync"`: `sync-now` writes it with author `sync-now` and an id `sync-<hash>`; the page
  writes it to `requests` like any request (owner only, ingested once). With `items` it is a
  per-item request; without, a global one (an empty `items` is refused by the schema).
  A request is served by ledger order, never by the clock: the read plan (`snapshot::plan`)
  notes the last event seq (`requests_upto`) and adds to the by-id reads every item of a
  request after the seq the project's last good read of the repo served; recording that read
  good stores `requests_upto` in the derived `served` table (project, repo node, seq; kept at
  the maximum). So a request recorded while the read fetches, even in the same millisecond
  as its start, waits for the next read. `served` is derived and not in the backup: a rebuild
  costs one by-id read of each item ever requested, never a missed request. An item both read
  in full and requested is read once; a requested item no longer in the repo's set is not
  read by the incremental read that serves the request, since the daily full read checks what
  left the set. `sync-now --issue` resolves the alias against the
  current members of the project's sets, case-insensitively, by the number within the repo
  named either as the config has it or by its name at its latest read (an unchanged member
  keeps the alias it was read under, so a rename never strands it), finding the repo through
  the ledger when `reads` is empty after a rebuild; it refuses anything else (exit 1, "not
  tracked by project P") and never reads GitHub. The
  task id comes from the project config's new optional `sync_task`; without it `task` is
  null.
- *Closed items and Recently closed (Store#403).* The status chain tests the state first, so a
  closed issue is `closed` and a pull request `merged` or `closed`, never `untriaged`, `blocked`
  or `ready`; the session lift to `in-progress` applies to open items only (a session matched
  to a closed issue still shows on it). The issue document gains `state_reason` (GitHub's,
  lowercased; null for a pull request), `closed_by` (the tracked pull requests whose
  `closingIssuesReferences` name it, by alias; issues only) and `dismissed_at`. `dismiss_closed`
  carries `value.closed_at`; ingest refuses it when the item was never read, is open, or was
  closed at another instant than the one named (compared as instants, not strings); the latest
  effective dismissal of an item wins, and derive sets `dismissed_at` (the decision's
  `created_at`) only while the item's `closed_at` is that instant. Undo is the generic one.
  The page hides an item it holds an unrefused, un-undone dismiss of the shown close for, before
  any ack, and otherwise follows `dismissed_at`; meta carries `recently_closed_days`. A
  dashboard close past its Undo window offers Dismiss too, so every closed item can leave the
  list. Left for a follow-up: the closing commit and who closed it, which need a further GitHub
  read and re-recorded fixtures; Recently closed lists issues only, not merged pull requests.
- **Open (for the owner):** found by the gate and left for after landing: an upload abandoned
  for over an hour and resumed after a cleanup leaves documents no marker names, which no later
  cleanup finds; the page's own collections (`decisions`, `launches`, `views`) are read to
  1000 documents with no partial notice; a group names repos by their configured name, so a
  repo renamed on GitHub drops out of its group's scope.
- **Open (for the owner):** "new" is approximated as open, with no comment by an inside author
  and no labels; `commits`, `rank` and `score` are empty; the `flow` and history snapshot files
  are not written yet; suggestions, plans, forks and metrics documents are empty until triage.

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
| A plain program can write a claude.ai artifact's database | **unverified, believed not**: the only known route is Claude's artifact tools; chapter 7.2's sync job exists because of this; first probe of the build |
| `ArtifactData` can write a document from a local JSON file | confirmed | its tool definition (`file_path` on `set`, `update` and `batch` entries) |
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
| GraphQL returns legacy node ids for older repos unless the request sends `X-Github-Next-Global-ID: 1` | confirmed | probe on rhaiscript/rhai, 2026-10-23: `R_kgDOAyYOjQ` with the header |
| A `NOT_FOUND` node comes back as `null` plus an error, and `gh api graphql` exits 1 with the JSON on stdout | confirmed | probe 2026-10-23, `nodes` with an unknown id |
| GitHub refuses a GraphQL query that defines a fragment it does not use | confirmed | the first recording run, 2026-10-23 ("Fragment PrFields was defined, but not used") |
| GitHub's issue search does not follow a repo rename (`repo:old/name`) | **believed** | not probed (no rename to probe on); the reader uses the current `nameWithOwner` either way |
| One home cycle reads Store (132 items), airzoned (14) and rhai (6) in full, then incrementally with nothing to read | confirmed | the fixture recording, 2026-10-23 (`crates/core/tests/fixtures/replay-meta.json`) |
