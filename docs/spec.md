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

## Chapters

1. **Configuration, translation and priority** ← this draft
2. The snapshot: what is read, what is derived, the JSON it writes
3. The dashboard page and its database
4. Ordering and the priority suggestion
5. Launching sessions: the dispatcher, routing, briefs, launch recipes
6. The CLI: label sync, config check, `explain`, plan/apply
7. Documentation and skills

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
between issues come from GitHub's own sub-issue and blocked-by relationships where the
repo has them, and otherwise from `blocked by #N` / `blocked by owner/repo#N` in an issue's
body or comments. Whether personal repos have the native relationships is **unverified**:
a probe before chapter 2 decides which is primary. A link to a repo in no project is kept
and shown as external.

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

## Claims checked while writing this chapter

| Claim | Status | Evidence |
|---|---|---|
| Open issues only in Store (67) and airzoned (14) among the 11 active repos | confirmed | `gh issue list` per repo, 2026-10-23 |
| airzoned uses GitHub default labels: 3 of 14 open issues carry `bug`, 1 `enhancement` | confirmed | `gh issue list -R yuvalrakavy/airzoned` |
| Rhai's labels are `bug, duplicate, enhancement, help wanted, question, wontfix, new feature, priority, regression, docs, vm` | confirmed | `gh label list -R rhaiscript/rhai`, 2026-10-23 |
| `yuvalrakavy/rhai` (the fork) has issues disabled, so Rhai is tracked as `rhaiscript/rhai` | confirmed | `gh repo list` (`hasIssuesEnabled: false`) |
| No open Rhai issue is authored by `yuvalrakavy`, so the example filter matches nothing today and the `also` numbers are placeholders | confirmed | `gh issue list -R rhaiscript/rhai --author yuvalrakavy` returned none |
| GitHub label names are unique case-insensitively | confirmed | probe on this repo: `BUG` beside `bug`, and `probe-case` beside `Probe-Case`, both refused as "already exists" |
| Seven of the 11 active repos are public | confirmed | `gh repo list` |
| GitHub has native sub-issues and blocked-by links on personal repos | **unverified** | probe before chapter 2 |
| `sendToClaude` wakes a watching session | confirmed | probe artifact, 2026-10-22 |
