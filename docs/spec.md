# issue-dashboard — design spec

Status: **draft, chapter 1 for review.** Tier T2 (a generic tool whose config format is a
contract other repos depend on).

## Intent

One private dashboard that shows the open issues of a whole project — a project being a
set of GitHub repos — in one consolidated, prioritised queue, with triage suggestions and
a button that starts a fix or triage session with a fitting model, effort and brief.

**Settled decisions** (with the reason each was made):

| Decision | Reason |
|---|---|
| Generic code in this repo; each project's config lives elsewhere (for the first project, the private `yuval-claude-settings` repo) | The code must name no repo. A config lists private repos and their layout, so it must not sit beside public code |
| Rust for the deterministic core (snapshot, label sync, config check) | Matches the fleet's toolchain; the config becomes a typed struct the compiler and `deny_unknown_fields` check |
| The AI parts (triage judgement, briefs, dispatch) are Claude sessions, not code here | They are judgement, and the session already has `gh`, git and the model routing |
| The dashboard is a private claude.ai Artifact with a `db` | Works on a phone; the page never needs GitHub access |
| A launch button wakes the dispatcher through `comments.sendToClaude` | Probed 2026-10-22: a press woke the watching session in seconds, no polling |
| The triage agent suggests; only accepted suggestions are written to GitHub | Writes to a repo are outward-facing, and on a public repo they are publication |

**Non-goals:** replacing GitHub as the store of record (status always lives on the
issues), a multi-user tool, tracking anything but GitHub issues.

## Chapters

1. **The config format** ← this draft
2. The snapshot: what is read, what is derived, the JSON it writes
3. The dashboard page and its database
4. Ordering and the priority suggestion
5. Launching sessions: the dispatcher, routing, briefs, launch recipes
6. Label sync and config check (the CLI)
7. Documentation and skills

---

## 1. The config format

### 1.1 Shape in one example

```toml
# One file per project. Path: --config <file>, else $ISSUE_DASHBOARD_CONFIG.
schema = 1

[project]
name = "Villa Rakavy home system"

# The shared default vocabulary. Every repo inherits it, key by key.
[vocabulary]
type.bug       = ["bug"]
type.todo      = ["todo"]
type.decision  = ["design-question"]
type.docs      = ["docs"]
priority.p0    = ["p0"]
priority.p1    = ["p1"]
priority.p2    = ["p2"]
priority.p3    = ["p3"]
modifier.needs_decision = ["needs-decision"]
modifier.in_progress    = ["InProgress"]
modifier.self_contained = ["self-contained"]
modifier.partial        = ["partial"]
modifier.umbrella       = ["umbrella"]

[defaults]
writes = "labels-and-comments"      # what the triage agent may write after you accept
launch = "worktree-pr"

[[repo]]
name   = "yuvalrakavy/Store"
areas  = ["kernel", "sdl", "renderer", "admin-ui", "drivers", "hometouch",
          "dev-loop", "harness", "process", "media-port", "behaviors"]
launch = "store-dev-track"

[[repo]]
name = "yuvalrakavy/airzoned"
areas = []                          # no area axis in this repo (yet)
vocabulary.type.todo = ["todo", "enhancement"]

[[repo]]
name   = "yuvalrakavy/mqtt_ac"
writes = "labels"                   # public repo: no agent comments

[launch.store-dev-track]            # fields defined in chapter 5
[launch.worktree-pr]
```

### 1.2 The canonical model is the tool's; the label names are the config's

The dashboard compares issues across repos, so every repo's labels must land on **the same
axes**. The axes and their values are fixed by the tool, because the ordering, the derived
status and the time limits read them:

| Axis | Values | Cardinality on one issue |
|---|---|---|
| `type` | `bug`, `todo`, `decision`, `docs` | exactly one, else *untriaged* |
| `priority` | `p0`, `p1`, `p2`, `p3` | exactly one, else *untriaged* (unless `umbrella`) |
| `area` | the repo's own `areas` list | at least one, if the list is non-empty |
| `modifier` | `needs_decision`, `in_progress`, `self_contained`, `partial`, `umbrella` | any number |

The config supplies only **which label names mean each value**. A repo whose labels already
follow the shared scheme writes nothing; a repo with its own names maps them.

`umbrella` is new: it marks a tracker or umbrella issue (Store #71, #81) that is shown but
never prioritised, ordered or aged. It replaces exempting issues by number, which would put
issue numbers into the config.

### 1.3 Resolution rules

1. **A repo's vocabulary merges over the default key by key.** `vocabulary.type.todo` in a
   repo replaces the default's `type.todo` list for that repo and nothing else.
2. **One canonical value may have several labels** (`todo = ["todo", "enhancement"]`); an
   issue carrying either counts as `todo`.
3. **A label means at most one canonical value in a repo.** The config check refuses a label
   listed under two values (`bug` under both `type.bug` and `type.todo`), naming both.
4. **Label names match case-insensitively**, because GitHub label names are unique
   case-insensitively. The config keeps the spelling the label sync should create.
5. **An area name is also a label name.** Areas are not mapped; `areas` lists the labels.
   A name may not be both an area and a vocabulary label in one repo. This is a real
   collision, not a theoretical one: the `ways-of-working` label scheme lists `docs` both as a
   type and as an example component. In a repo that wants a docs area, the area takes a
   different name (`documentation`), because the type is what the cross-repo view compares.
6. **A label that maps to nothing is reported, never dropped.** The issue is still shown;
   the label appears in the repo's "unmapped labels" list, so a vocabulary gap is visible.

### 1.4 Per-repo settings

| Key | Default | Meaning |
|---|---|---|
| `name` | required | `owner/repo`; also the repo's identity in every issue id (`owner/repo#N`) |
| `areas` | required | the area labels; `[]` turns the area axis off for the repo |
| `vocabulary.*` | inherited | overrides, as in 1.3 |
| `writes` | `[defaults].writes` | `none`, `labels`, or `labels-and-comments` |
| `launch` | `[defaults].launch` | names a `[launch.*]` recipe (chapter 5) |
| `docs` | `[]` | paths, relative to the repo root, a fix brief should point to (a CLAUDE.md, guides) |
| `local_path` | none | the local checkout, for duty-0 checks against git history; without it those checks are skipped and the dashboard says so |

`writes` exists because seven of the project's repos are public: a comment the agent posts
there is publication. The config check warns when a public repo (read from GitHub, not
declared) has `writes = "labels-and-comments"`, and requires `allow_public_comments = true`
on that repo to silence it.

### 1.5 Issue identity and cross-repo links

Every issue is `owner/repo#N` inside the tool, the snapshot and the database. A body or
comment's `blocked by #N` resolves within the issue's own repo; `blocked by owner/repo#N`
crosses repos. A link to a repo not in the config is kept and shown as external, never
followed.

### 1.6 The config check

`issue-dashboard check-config` refuses, naming the key:

- an unknown key anywhere (`deny_unknown_fields`), so a typo cannot silently mean "default";
- `schema` other than one the tool knows;
- a label under two canonical values, or both an area and a vocabulary label (1.3 rules 3, 5);
- a `launch` name with no `[launch.*]` table;
- two `[[repo]]` entries with the same name.

It reports, without refusing: a repo `gh` cannot read; a configured label missing from the
repo (label sync would create it); a label present on open issues that maps to nothing; a
public repo allowed to receive comments without `allow_public_comments`.

### 1.7 Open questions for the owner

1. **Is the fixed canonical model right?** The four types, four priorities and five modifiers
   are the ones `ways-of-working` already defines. A repo cannot add an axis or a value; it can
   only name the labels. The alternative — config-defined axes — makes ordering rules
   configurable too, which is a much larger contract. Recommendation: fixed.
2. **One config file per project, or one file listing several projects?** Recommendation: one
   per project, since a dashboard is one project and the file is its whole definition.

## Claims checked while writing this chapter

| Claim | Status | Evidence |
|---|---|---|
| 11 active repos; open issues only in Store (67) and airzoned (14) | confirmed | `gh issue list` per repo, 2026-10-23 |
| airzoned uses GitHub default labels; 3 of its 14 open issues carry `bug`, 1 `enhancement` | confirmed | `gh issue list -R yuvalrakavy/airzoned` |
| Store and logmon-mcp use the `ways-of-working` scheme; logmon-mcp's areas are `daemon`, `shim`, `protocol` | confirmed | `gh label list` per repo |
| Seven of the 11 are public | confirmed | `gh repo list` (tracing-init, mqtt_ac, logmon-mcp, mqtt_dmx, mqtt_dali, mqtt_ynca, mqtt_mdns) |
| GitHub label names are unique case-insensitively | confirmed | probe on this repo, 2026-10-23: creating `BUG` beside `bug`, and `probe-case` beside `Probe-Case`, were both refused as "already exists" |
| `sendToClaude` wakes a watching session | confirmed | probe artifact, 2026-10-22 |
