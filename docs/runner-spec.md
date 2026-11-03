# The bridge runner (`issue-dashboard bridge-serve`) — design spec

Status: **chapters 1–6 approved by the owner 2026-11-02; cross-family design gate (Codex)
folded 2026-11-03 (F1–F5, dispositions in the rationale); awaiting the owner's approval.** T2. Alternatives and reasons: [runner-spec-rationale.md](runner-spec-rationale.md).
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
(Q5, ch. 5); landing is automatic while the owner is the sole developer (Q5b), once a
positive cross-family review verdict for the exact change exists (5.6); the ledger is
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

2.1 Per launch: **reserve → (phase 2: provider `claim`) → strict start → wait for verified
routing → prepare → stage rounds → `launch_result` → apply the result (2.6) → (phase 2:
`gate` → land per 5.6, which may hold as "ready to land" → `post_land`) → archive (confirmed)
→ provider `release` (phase 2, confirmed) → `launch-ended`.** A launch that fails at any point
takes the failure tail instead: archive → provider `release` as abandoned, evidence exported
(5.1) → `launch-ended failed`. The runner never writes `Unconfirmed`; a runner launch is
Reserved until `started`. `ended()` (`launch.rs:721`) accepts only a Started launch; it gains a
runner-owned path from **Reserved**, recording AgentBridge's status body as evidence, for a
launch whose worker finished or was archived before `started` was written.
`launch-ended` is always the LAST step: `ended()`
records Ended and Released in one transaction (`launch.rs:720`), and a released launch leaves
the held-launch scan (`State::holds()`, `launch.rs:149`), so ending earlier would strand an
unarchived worker and its AgentBridge write fence after a crash. The launch keeps its slot
through cleanup. A worker that runs long is shown "overdue" on the page; overdue never
cancels.

2.2 **Strict start** uses the reservation token as AgentBridge `operation_id`, the launch's
routing (harness, model, effort), `access: read-only`, and `network: none`. The runner records
`started` only when `status(token)` carries a `native_session_id` and a `routing` whose actual
model/effort equal the requested ones; a mismatch archives the worker and ends the launch
`failed` with both values. A `queued` answer has no native session yet and never records
`started`.

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
(`suggest.rs:605-617`, idempotent by launch) for triage, `eval score` for eval. `eval score` is
NOT idempotent by launch today: `Dedupe::Latest` compares only the latest event
(`ledger.rs:279`) and its `distinct` has no launch id (`eval.rs:1235-1250`), so A → B → retry
A records A again as the key's latest evaluation over B. It gains a **receipt**: a ledger event
`eval_applied`, subject = launch id, written in the same `tx` as the `evaluated` event. The
runner path checks for the receipt before anything else, including the map's `fs::write`; a
launch with a receipt is a no-op. The map file is content-addressed (`map_path`,
`eval.rs:259`, by `Map::id()` hash), so a crash between the map write and the `tx` leaves a
harmless orphan file and a retry rewrites the same file. Then the rest of 2.1.

## 3. Recovery

3.1 On start, and after any child restart, the runner derives each held launch's position
from the ledger plus `status(token)` (AgentBridge `src/dashboard/store.rs:1308-1372`), and
acts only on that answer. The answer is read by its **fields**, never by the `state` name
alone: `state` is one value chosen by a precedence (archived, result, queued,
awaiting_caller, waiting_user, preflight-blocked→`error`, failure→`error`|`unknown`, …),
while `never_started` is a separate boolean that accompanies `state: error`, and `result` is
non-null even when `state` is `archived`. The rows are evaluated **in order**; the first match
acts:

| # | Answer (fields) | Action |
|---|---|---|
| 1 | call fails: connector unreachable; or any error other than `unknown tracker token` (`token job missing`, `start intent missing`, `store.rs:1311-1316`) | unreachable: hold, retry with backoff (1 s doubling to 5 min); other errors: hold, shown to the owner |
| 2 | error `unknown tracker token`, ledger state **Reserved** (never recorded started) | identical start (AgentBridge replays the original selection) |
| 3 | error `unknown tracker token`, ledger state **Started or later** | rollback evidence (3.4): stop the runner loudly |
| 4 | `result` non-null | resume 2.1 at the first undone step, each read from its own evidence: applied (2.6 receipt / `record-suggestions` record), landed (`origin/main` contains `head_sha`, by `git`), archived (`state: archived`), released (provider `inspect`) |
| 5 | `state: archived`, `result` null | failure tail of 2.1 (provider release as abandoned, then `launch-ended failed`) |
| 6 | `never_started: true` | provider release if a workspace was claimed, then `release_never_started(token)` (3.2) |
| 7 | `state: queued` | stay reserved; wait for an answer with a native session |
| 8 | `state` started / awaiting_caller with `native_session_id` and matching `routing` | record `started` if missing, resume the flow |
| 9 | `state` started / awaiting_caller, routing absent or mismatched | as 2.2: mismatch → archive + `failed`; absent → stay reserved |
| 10 | `state: waiting_user` | show the question on the page; no action |
| 11 | anything else (`error` without `never_started`, `unknown`) | hold; only the owner's Cancel acts |

Tests drive these rows with response bodies copied from `token_status`'s real shape, including
a launch queued at restart that preflight then rejects (`queued` → `error` + `never_started`).

3.2 **`release_never_started(token)`** is a new ledger transition: runner-owned launches only,
no `SETTLE_AFTER` wait (`launch.rs:36`), records AgentBridge's evidence (the `never_started`
status body). The dispatcher's `release` (`launch.rs:746`) is unchanged.

3.3 **Every step is idempotent** — start by token, `started` by (token, session), result
application by launch (2.6's receipt for eval), archive by worker, provider release by
workspace, `launch-ended` last — so re-running the derived position never doubles an effect.

3.4 **Rollback guard:** a restored or swapped AgentBridge state is detected by the ledger, not
by a cursor: AgentBridge's `wait(after)` returns only newer events (`store.rs:1153`) and no call
reports its high-water mark, so a lower cursor is unobservable. The evidence is a token the
ledger recorded as started that AgentBridge no longer knows (3.1 row 3); AgentBridge never
deletes a `token_jobs` row (only inserted, `store.rs:345`), so this has no innocent cause short
of a reset the owner made. The runner then stops
loudly and does not resume until the owner runs `runner pin --accept-rollback`; an identical
start there would run the same work twice. No AgentBridge change is needed.

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

5.6 **Landing (automatic, Q5b)** requires all of: the gate green; the item's design
owner-approved; and a **positive review verdict** (`pass`, no S3 open) from the harness spec's
review launches (harness-spec 5.3–5.7: a `group_review` on another family over a frozen
bundle) **bound to the exact `head_sha`** being landed. "No S3 open" alone never suffices: it
is also true when nobody reviewed. Steps: (1) if `origin/main` moved, rebase and re-run `gate`;
the verdict still stands only if `git patch-id --stable` of the rebased change equals the
reviewed one's, otherwise a new review round runs; (2) `land`, then `post_land`; (3)
`release`. A rebase conflict goes back to the worker as a stage round; a push refused because
main moved again repeats (1), at most **3 times**, then the group is held for the owner. The
runner never resolves a conflict itself. **Until the harness implementation provides review
launches**, a green result is held, shown "ready to land", and the owner lands it with one
action (`runner land <launch>`), which runs steps 1–3.

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
| 3 | each 3.1 row → its action, in order, from real `token_status` bodies; queued at restart → preflight rejected → released | `AgentBridgeClient` trait, scripted fake | runner unit, one row per answer |
| 4 | crash between any two steps (including between apply, archive, release and `launch-ended`) + restart never doubles a start or a record and never strands a worker | step hook aborting after step N | runner test, loop over N |
| 5 | unknown token for a Started launch stops the runner; for a Reserved one starts identically | fake client | unit |
| 6 | 3 corrections then `failed` | fake client | unit |
| 7 | provider: same `request_id` → same workspace; release refuses dirty/unknown; unknown action outcome held; one gate at a time per gate clone; a claim refused after `free` keeps the launch reserved | fake provider + real `slots` on a temp dir | unit + integration |
| 8 | landing (phase 2): no verdict → held "ready to land"; `runner land`: moved main → rebase + re-gate, conflict → worker, 3 refusals → hold, bad `head_sha` → failed; crash between `land` and `post_land` resumes at `post_land`; a held launch is never archived or released | local bare repo as `origin` | integration |
| 8b | automatic landing (when review launches exist): verdict for another SHA → held; rebase with equal patch-id keeps the verdict, unequal one re-reviews | local bare repo + fake verdict | integration, with the harness implementation |
| 9 | `eval score` A → B → retry A records nothing new and B stays the latest evaluation | eval store | core unit |
| 9b | `ended` from Reserved: runner-owned only, records the evidence; a dispatcher launch is refused | core | unit |
| 10 | second `bridge-serve` refuses | `runner.lock` | integration |
| 11 | `release_never_started` refuses for dispatcher-owned launches and has no timer | core | unit |
| A | one real eval launch end to end, small model, low effort | live system | manual, recorded in the retro |

6.2 **Phasing.**
1. **Runner phase 1 (read-only):** ledger changes (`Routing.harness`, claim rule, claimer
   filter, `release_never_started`, `runner_status`, the `eval score` receipt, `ended` from Reserved), the service
   (LaunchAgent, lock, `init`/`pin`), the flow (§2), recovery (§3), concurrency (§4). Ends with
   acceptance row A; triage then closures enabled by config afterwards.
2. **Runner phase 2 (workspaces):** §5, with owner-triggered landing (`runner land`).
   Automatic landing switches on when the harness implementation's review launches exist.
   The Store staleness line lands separately in Store.
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

## As built (phase 1)

| § | Change | Why | Commit |
|---|---|---|---|
| 2.1, 2.4 | D1: the runner runs `eval refresh-history`, `prepare` and `score_for_launch`; a runner eval brief has the worker only answer staged files | a read-only, no-network worker cannot write the ledger | — |
| 2.5 | D2: a stage per replay key (`type`, `area`, `priority-first`, `priority`; `area@<repo>` where asked per repo); a key with no held-out candidate is left out; files staged by `transfer`, key `<token>-<stage>-<file>` | `score` would refuse it; a correction re-attaches the same files | — |
| 2.2 | start: profile = harness, access `read-only`, issue = first item else the launch id, revision = the request's `created_at`; the brief's SHA-256 is not recorded yet | `started` has no field for it | — |
| 2.1, 2.6 | D3: a stage is scored as its result is accepted; a refusal is a correction round | progress lives in the ledger | — |
| 3.1 | D4: each step derives from the ledger launch and `status(token)` alone | nothing only in memory | — |
| 2.1 | D5: a finish message after the last stage; ends with `launch_result`'s outcome only if every stage applied, else `failed` | | — |
| 4.2, 1.2 | pre-reserve checks before `reserve_next`; `wait`'s bound = connector timeout + 120 s | | 11a5623, 4f3e728 |
| 4.2 | checks 3-5 (`runner::admission`) run before the slot check; a queued launch the runner may not admit is not "ahead" in its own claim (it still is for another claimer); check 1 never stops an eval (read-only kinds hold no issue) | an unadmittable launch would block its pool | — |
| 2.2 | `network: none` is not sent | `start` has no network field; `read-only` access is the fence | — |
| 1.2 | setup in `runner.json` (connector, config, `state_dir`, pin, rollback); `runner init` refuses an existing setup or connector state and leaves it unpinned; `init` and `pin` take the runner lock | `--init` reopens existing state; one connector per root | — |
| 3.4 | a rollback is saved in `runner.json`; the service refuses to start until `runner pin --accept-rollback` | the flag must outlive the process | — |
| 4.3 | `wait` (≤ 5 s) is the poll; the cursor is the largest event `sequence`; history refreshes run on threads, held in memory (a restart refreshes again) | one loop, no stall on `gh` | — |
| 1.3 | `runner launchagent` prints the plist/unit only; launchd cannot skip a restart on exit 1, so `ThrottleInterval` 60 | install is the owner's | — |
| 2.5, 2.6 | a launch's manifests and its plan (`plan.json`, written at its first stage, never empty) live under `launches/<id>/`; its prepare and score skip 3.2's re-verify; a plan with no scored stage never lands | another eval's refresh or a hand `prepare` must not change what it was asked | gate fix |
| 2.6 | answers must cover every question of the stage, else a correction | a partial answer would score as unanswered | gate fix |
| 3.1, 4.3 | a reserved eval asks `status` before its refresh gate; every held/waiting/failed step is its reason row (a question shows its text) | a known token is reconciled whatever GitHub does | gate fix |
| 1.2 | `runner init` reads `state_dir` from the connector config | the one checked is the one the connector writes | gate fix |
