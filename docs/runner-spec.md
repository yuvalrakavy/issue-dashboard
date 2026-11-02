# The bridge runner (`issue-dashboard bridge-serve`) — design spec

Status: **chapters 1–6 approved by the owner 2026-11-02; awaiting the cross-family design
gate.** T2. Alternatives and reasons: [runner-spec-rationale.md](runner-spec-rationale.md).
Lands before [harness-spec.md](harness-spec.md), whose non-goal "implementation on a bridge
harness is unavailable" this spec's phase 2 retires.

## Intent

A launch routed to an AgentBridge harness (Codex, OpenCode, Claude via the bridge) is run by
code, not by a dispatcher model: an always-on service reserves it from the ledger, starts the
worker through AgentBridge, drives its stages, records the result, and — for implementation —
gates and lands the work. **Principle (owner):** whatever code can decide, code decides; a
model is used only for judgment.

**Settled with the owner:** one service per ledger (Q2); recovery acts only on a definite
AgentBridge answer, never on a timer (Q3); one generic read-only flow, eval first (Q4);
workspaces in runner phase 2 behind a provider seam that Workspace Manager will later fill
(Q5, ch. 5); landing is automatic while the owner is the sole developer (Q5b); the ledger is
authoritative and a full queue keeps a launch queued, never failed (Q6). No auto-cancel
anywhere.

**Non-goals:** launches the Claude-desktop dispatcher already starts (no `routing.harness`);
Workspace Manager itself; remote/multi-host workers (AgentBridge has no path translation);
PRs (none until other contributors); priority beyond FIFO.

---

## 1. Service and claim

1.1 **`issue-dashboard bridge-serve --root <ledger>`** runs as LaunchAgent
`com.issue-dashboard.runner` (a systemd user unit on Linux; same binary). It holds
`<root>/runner.lock` (exclusive, `crates/core/src/lock.rs`); a second instance refuses and
exits non-zero naming the holder's pid.

1.2 It owns exactly one `agentbridge-dashboard` child, membership `IssueDashboardRunner`, and
is that child's only restarter (AgentBridge `DASHBOARD-RUNNER.md`). `runner init` creates the
membership once (`--init`); `runner pin` records the AgentBridge package version and connector
contract the runner was qualified against. A mismatch at start refuses loudly.

1.3 **`runner_status`** is one ledger row: pid, started_at, pinned version, connector cursor,
last loop tick, per-harness health snapshot. The page and `issue-dashboard runner status`
read it; a tick older than 60 s reads as "runner down".

1.4 **The claim rule lives in `reserve`** (`launch.rs:516`), not in callers: a launch whose
request carries `routing.harness` is reservable only by claimer `runner`; one without it only
by a dispatcher. A wrong claimer is refused with both names. `Routing` (`docs.rs:17`) gains
`harness: Option<String>` here, in runner phase 1.

## 2. The read-only flow

2.1 Per launch: **reserve → strict start → wait for verified routing → prepare → stage
rounds → `launch_result` → archive.** Archive frees the slot. A worker that runs long is
shown "overdue" on the page; overdue never cancels.

2.2 **Strict start** uses the reservation token as AgentBridge `operation_id`, the launch's
routing (harness, model, effort), `access: read-only`, and `network: none`. The runner records
`started` only after AgentBridge reports the actual model/effort equal to the requested ones;
a mismatch archives the worker and ends the launch `failed` with both values.

2.3 **Kinds are config**, `[runner.kinds.<kind>] enabled = bool`. Order of enablement: `eval`,
then `triage`, then closures, each after clean real runs. A disabled kind stays queued with
reason `kind disabled`.

2.4 **Briefs** are built by `brief.rs`, the same code the dispatcher uses, so its trust
boundaries hold (facts-only task sentence, issue text only in author-headed fenced blocks); the
runner records the brief's SHA-256 with the start. A new kind is a new `brief.rs` template,
never a file. **GitHub data is fetched by the runner** (`gh`, off the
event loop) and attached as staged files; the worker has no network.

2.5 **Stage rounds:** the runner validates each `stage_result` by code (schema, required
fields, referenced items exist). A failure sends a correction `stage_input` with the
validation errors, replying on that stage's reply reference. At most **3 corrections**; the
fourth failure ends the launch `failed`, keeping every round's content.

2.6 **Applying a result** reuses existing commands: `record-suggestions`
(`suggest.rs:605-617`, idempotent by launch) for triage, `eval score` for eval (already
idempotent: `Dedupe::Latest`, `eval.rs:1250`; test row 6.1/9 pins it). Then `launch-ended`.

## 3. Recovery

3.1 On start, and after any child restart, the runner derives each held launch's position
from the ledger plus `status(token)` (AgentBridge `src/dashboard/store.rs:1307-1372`), and
acts only on that answer:

| Answer | Action |
|---|---|
| `unknown tracker token` | identical start (AgentBridge replays the original selection) |
| live (queued / started / awaiting_caller) | record `started` if missing, resume the flow |
| `never_started` | `release_never_started(token)` (3.2) |
| `result` | apply it (2.6) |
| archived, no result | end `failed` |
| `waiting_user` | show the question on the page; no action |
| `error` / `unknown` / uncertain | hold; only the owner's Cancel acts |
| unreachable | hold, retry with backoff (1 s doubling to 5 min) |

3.2 **`release_never_started(token)`** is a new ledger transition: runner-owned launches only,
no `SETTLE_AFTER` wait (`launch.rs:36`), records AgentBridge's evidence (the `never_started`
status body). The dispatcher's `release` (`launch.rs:746`) is unchanged.

3.3 **Every step is idempotent** — start by token, `started` by (token, session), result
application by launch, archive by worker — so re-running the derived position never doubles
an effect.

3.4 **Cursor rollback guard:** `runner_status` keeps the highest connector cursor seen. A child
reporting a lower one (a restored or swapped AgentBridge state) stops the runner loudly; it does
not resume until the owner runs `runner pin --accept-cursor`.

## 4. Concurrency

4.1 Runner and dispatcher share recipe pools (`config.rs:91` `capacity`); FIFO by
`queued_seq` is kept. `reserve_next` (`launch.rs:614`) gains a **claimer filter**: it considers
only queued launches the caller may claim (1.4).

4.2 **Admission checks, in order** — the first that fails leaves the launch queued with that
reason shown on the page:

| # | Check | Source |
|---|---|---|
| 1 | no issue held by another launch | `held_item` (existing) |
| 2 | a free pool slot | `capacity` (existing) |
| 3 | kind enabled | 2.3 |
| 4 | harness healthy | AgentBridge `health`, read live |
| 5 | `[runner.harness.<name>] max_active` (default 1) not reached | counted from the ledger |
| 6 | (implementation) a free workspace and a free gate location (lane or gate clone) | provider `free(recipe)`, 5.1 |

4.3 **One event loop** waits on the child's events (`wait`); a 5 s poll finds newly queued
launches. `gh` and `prepare` run on worker threads and post completions back to the loop. State
machines are rebuilt from the ledger, never held only in memory.

## 5. Workspaces and landing (runner phase 2)

5.1 **The seam, `WorkspaceProvider`**, in Workspace Manager's vocabulary:

| Call | Promise |
|---|---|
| `claim(recipe, base_sha, request_id)` | returns a descriptor (id, host, `write_root`, gate environment); same `request_id` → same workspace |
| `action(ws, name)` | runs a named recipe action; returns exit status, log ref, operation id |
| `inspect(ws)` | Git state (head, clean/dirty, unpushed) and runtime state |
| `release(ws)` | only when landed or abandoned with evidence exported; refuses on dirty/unknown state; never deletes on a guess |
| `free(recipe)` | advisory count of claimable workspaces and gate locations, for admission (4.2 #6) |

`claim` runs after `reserve`, with `request_id` = the reservation token. A claim refused
despite `free` (a human took the lane) leaves the launch reserved, retried each loop, shown
"waiting for workspace"; it never returns to queued and never takes a lane in use.
Operation ids are kept in the ledger; an action with an unknown outcome is held, never rerun
(3.1 applies).

5.2 **Recipe = named actions**, mapping 1:1 onto Workspace Manager recipe actions
(`[runner.recipes.<repo>]` in config):

| Action | Store | issue-dashboard |
|---|---|---|
| `gate_at` (a setting, interim provider only) | `lane` | `clone` |
| `prefetch` | `cargo fetch` | `cargo fetch` |
| `worker_checks` | build, clippy, nextest, data_lint | clippy, nextest |
| `gate` | `scripts/pre-merge-check.sh --quick` + derived set | clippy + nextest |
| `land` | `store-cli dev-track land` (pushes `origin/main`) | `git push origin HEAD:main` |
| `post_land` | `git -C <t0> pull --ff-only`, then `store-cli restart` | — |

5.3 **Interim provider `slots`.** The worker edits in a **full-clone slot** from the
membership's `worker_write_roots` (AgentBridge refuses linked worktrees, `DASHBOARD-WRITE.md:15`).
`claim` resets the slot to `base_sha` (`git fetch`, `reset --hard`, `clean -fdx -e target/`)
so builds stay warm; workspace id = slot id + claim counter. The runner runs `prefetch` with
network; the worker gets `network: none` with `CARGO_HOME` and `TMPDIR` inside the slot.
**Gate split:** the worker runs `worker_checks`; the runner fetches the worker's commit into
the recipe's `gate_at` location and runs `gate` and `land` there. `lane`: a **dev-track lane**
it claims (`store-cli dev-track claim`) and releases afterwards; a runner-held lane is released
only by the runner. `clone`: a runner-owned **gate clone** `<root>/gate/<repo>`, one gate at a
time per repo, reset like a slot. No free slot, lane or gate clone ⇒ queued (4.2 #6).

5.4 **Future provider `wsmanager`:** `claim` → `wsmanager claim`, the worker starts inside the
workspace (`start-worker` action), `gate` runs in-workspace and the split disappears. It needs
an AgentBridge handoff for in-pod workers, filed when the prototype exists. Chapters 1–4 do
not change.

5.5 **Write start:** as 2.2 with `access: workspace-write`, `write_root` = the descriptor's,
brief "local commits only; return `head_sha`". The runner verifies `head_sha` with its own
`git` (exists, descends from `base_sha`, tree clean); a mismatch fails the launch.

5.6 **Landing (automatic, Q5b)** when the gate is green, no S3 finding is open, and the
item's design is owner-approved: (1) if `origin/main` moved, rebase and re-run `gate`;
(2) `land`, then `post_land`; (3) `release`. A rebase conflict goes back to the worker as a
stage round; a push refused because main moved again repeats (1), at most **3 times**, then the
group is held for the owner. The runner never resolves a conflict itself.

5.7 **`runner probe <harness>`** is the paid write probe: a synthetic repo, one commit,
`head_sha` verified, workspace released; its result feeds the harness's write grade
(harness-spec 2.5). **Store:** `store-cli status` gains a t0 staleness line (t0 behind
`origin/main`), landed separately through a Store lane.

## 6. Tests, phasing, docs

6.1 **Tests.** Each guard's test is watched red against the unguarded code first.

| # | Property | Seam | Tool |
|---|---|---|---|
| 1 | claim rule both ways | `reserve` | core unit |
| 2 | each 4.2 check keeps the launch queued with its reason | `reserve_next` + check context | core unit, one row per check |
| 3 | each 3.1 row → its action | `AgentBridgeClient` trait, scripted fake | runner unit, one row per answer |
| 4 | crash between any two steps + restart never doubles a start or a record | step hook aborting after step N | runner test, loop over N |
| 5 | cursor rollback stops the runner | fake client | unit |
| 6 | 3 corrections then `failed` | fake client | unit |
| 7 | provider: same `request_id` → same workspace; release refuses dirty/unknown; unknown action outcome held; one gate at a time per gate clone; a claim refused after `free` keeps the launch reserved | fake provider + real `slots` on a temp dir | unit + integration |
| 8 | landing: moved main → rebase + re-gate; conflict → worker; 3 refusals → hold; bad `head_sha` → failed | local bare repo as `origin` | integration |
| 9 | `eval score` applied twice records once | eval store | core unit |
| 10 | second `bridge-serve` refuses | `runner.lock` | integration |
| 11 | `release_never_started` refuses for dispatcher-owned launches and has no timer | core | unit |
| A | one real eval launch end to end, small model, low effort | live system | manual, recorded in the retro |

6.2 **Phasing.**
1. **Runner phase 1 (read-only):** ledger changes (`Routing.harness`, claim rule, claimer
   filter, `release_never_started`, `runner_status`), the service
   (LaunchAgent, lock, `init`/`pin`), the flow (§2), recovery (§3), concurrency (§4). Ends with
   acceptance row A; triage then closures enabled by config afterwards.
2. **Runner phase 2 (workspaces):** §5. The Store staleness line lands separately in Store.
3. **Docs and skills:** 6.3.

Deep gate before each merge; the depth reader from another family (Codex).

6.3 **Docs and skills.** New `docs/runner.md` (install, `init`, `pin`, status, probe,
recovery, every queue reason). `issues-dashboard` skill: a step checking the runner service;
the dispatcher brief stops launching harness-routed work. Store `docs/guides/dev-tracks.md` and
the `using-dev-tracks` skill: a red flag — a runner-held lane is released by the runner, never
by `dev-track release`. Retro entry, plus the process rule "a state-transition chapter comes
before UI chapters".

6.4 **Skills for the implementer:** `ways-of-working`, `agentbridge`, `issues-dashboard`;
`using-dev-tracks` for the Store landing.
