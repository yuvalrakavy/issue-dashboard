# Runner spec — rationale

Companion to [runner-spec.md](runner-spec.md). Read it to challenge a decision; the spec holds
only the decisions.

## Alternatives weighed

- **Q1 The dispatcher model launches bridge work too.** Rejected: every step after the choice
  is mechanical, and a model in the loop adds cost, latency and a non-deterministic retry path.
  The claim rule sits in `reserve` so neither side can take the other's launch by mistake.
- **Q2 AgentBridge grows a daemon that runs launches.** Rejected: AgentBridge stays daemonless
  and never allocates, merges or pushes (`DASHBOARD-WRITE.md`); the runner is a peer member.
- **Q3 Release a reservation after a timeout.** Rejected: a slow worker and a dead one look the
  same to a clock; releasing a live writer's slot lets a second writer start. Only a definite
  `never_started` releases, and it needs no 15-minute wait because the evidence is direct.
- **Q4 One flow per kind.** Rejected for phase 1: eval, triage and closures differ only in
  brief, attachments and result applier, which are data.
- **Q5 Worker in a dev-track lane.** Impossible: lanes are linked worktrees, which AgentBridge
  refuses for strict write (their Git metadata is outside the root). Hence full-clone slots
  for editing and a lane only for the gate (split B). Option C (make a full clone into a lane)
  was rejected: lanes carry per-index ports, `.dev-track` markers and a shared target dir that
  `store-cli` manages, and duplicating that is the parallel-mechanism red flag.
- **2.4 Briefs from `briefs/<kind>.md` files** (approved in ch. 2, withdrawn at self-review):
  `brief.rs` already builds briefs by code to hold the trust boundaries (4.2); a file template
  would bypass them.
- **5.3 Gate in a lane for every repo.** Only Store has lanes; issue-dashboard gates in a
  runner-owned clone instead, chosen per recipe by `gate_at`.
- **Ch. 5 Design workspaces concretely for slots.** Replaced by the seam at the owner's choice
  (A): Workspace Manager (Codex proposal v0.8, 2026-10-28) will replace lanes, so the runner
  speaks its vocabulary now and the swap is a provider change.
- **Q5b Open PRs and let the owner merge.** Rejected while the owner is the sole developer: the
  gate is the review, and a PR queue only adds a manual step. Revisit with other contributors.
- **Q6 Fail a launch when no capacity.** Rejected: capacity is transient; failing would force
  the owner to re-request. Queued with a visible reason instead.
- **Priority scheduling.** Deferred: FIFO with per-kind pools covers today's volume.

## Traps the design guards against

- **Uncertain treated as absent.** Every non-definite AgentBridge answer holds (3.1); only the
  owner's Cancel acts on it.
- **Two writers on one root.** AgentBridge's writer fence is host-local and released only by a
  confirmed archive; the runner never deletes a reservation to make a new launch pass.
- **A restored AgentBridge state replaying old answers.** The rollback guard (3.4): a token
  the ledger saw start that AgentBridge no longer knows.
- **An answer read by its name.** `token_status` folds several facts into one `state` by
  precedence; recovery reads the fields (3.1).
- **Ending before cleanup.** `launch-ended` releases the slot and drops the launch from the
  scan, so it is the last step (2.1).
- **Model drift.** Routing is verified from AgentBridge's reported actual model/effort (2.2),
  not from the request.
- **Lane theft.** A runner-held lane released by a human `dev-track release` would strand a
  gate; documented as a red flag (6.3).

## Facts the design rests on (verified 2026-11-02)

- `reserve` (`launch.rs:516`) and `reserve_next` (`launch.rs:614`) take a `dispatcher` string
  and a capacity closure; no claimer filter exists. `release` (`launch.rs:746`) enforces
  `SETTLE_AFTER` (`launch.rs:36`, 15 min).
- `Routing{model, effort, workspace?, reason?}` has no harness (`docs.rs:17`).
- `capacity` per recipe (`config.rs:91`, `config.rs:406`).
- `record-suggestions` is idempotent by launch (`suggest.rs:605-617`). `eval score` is not:
  `Dedupe::Latest` compares only the latest event (`ledger.rs:279`), `distinct` = {manifest
  hash, answers hash} with no launch id (`eval.rs:1235-1250`). Maps are content-addressed
  (`map_path`, `eval.rs:259`). Hence the receipt (2.6). This line said "idempotent" until the design gate (F5).
- AgentBridge `token_status` (`src/dashboard/store.rs:1308-1372`): `unknown tracker token` is
  an error; otherwise `state` by precedence archived > result > queued > awaiting_caller >
  waiting_user > preflight-blocked (`error`) > failure (`error` if `never_started`, else
  `unknown`) > uncertain (`unknown`) > native idle (`unknown`) > active (`started`) > `queued`.
  Separate fields: `never_started` (bool), `result`, `routing`, `native_session_id`.
- `ended()` records Ended and Released in one transaction (`launch.rs:720`); `holds()` is true
  only for Reserved, Unconfirmed and Started (`launch.rs:149`).
- AgentBridge `events(after, limit)` returns only sequences above `after` (`store.rs:1153`).

## Design gate dispositions (Codex, 2026-11-03, verdict "fix then build")

| # | Sev | Finding | Disposition |
|---|---|---|---|
| F1 | S3 | recovery could record `started` from a `queued` answer with no native session; `never_started` is a flag with `state: error`, not a state | fixed: 2.2 + 3.1 ordered rows over fields; test 3 uses real bodies |
| F2 | S3 | a crash after `launch-ended` and before archive strands the worker and its write fence | fixed: `launch-ended` last (2.1); test 4 crashes between cleanup steps |
| F3 | S3 | automatic landing had no positive, revision-bound review requirement | fixed (owner chose A): 5.6 requires a harness-spec review verdict bound to `head_sha`, patch-id across rebase; owner-triggered `runner land` until then |
| F4 | S3 | the cursor rollback guard could not observe a rollback | fixed: 3.4 + 3.1 rows 2-3, ledger-based evidence; no AgentBridge change |
| F5 | S2 | `eval score` not idempotent by launch | fixed: launch-scoped receipt (2.6); test 9 A → B → retry A |

Confirming reader (sonnet, medium, over 68641b7), verdict "fix 4", no S3:

| # | Sev | Finding | Disposition |
|---|---|---|---|
| C1 | S2 | `ended()` accepts only Started; a result found while the ledger is Reserved could not end | fixed: runner-owned `ended` from Reserved (2.1); test 9b |
| C2 | S2 | row "archived, no result" named a ledger state that does not exist and skipped provider release | fixed: failure tail of 2.1; rows reordered |
| C3 | S2 | gate, ready-to-land hold, land and post_land were missing from the step order, so a restart could archive a held launch | fixed: 2.1 order; row 4 resumes at the first undone step by its own evidence |
| C4 | S1 | `never_started` row ahead of `result` | fixed: result and archived rows first |
| C5 | S1 | Unconfirmed + unknown token and other `token_status` errors unclassified | fixed: the runner never writes Unconfirmed; other errors hold (row 1) |
| C6 | S1 | patch-id and other-SHA test rows unexercisable before review launches; no row for `runner land` | fixed: test rows 8 / 8b |

Loop escape, mechanism: F1 and F5 both described an external answer by its name, not its
fields and dedupe rule. Rule for later specs: an API answer a spec relies on is cited by field
and by the code of its dedupe/precedence.
- AgentBridge 0.2.4: strict write is Codex full clone only; worktrees refused; OpenCode and
  Claude write refused at preflight; `network` none|fetch is all-or-nothing; fence host-local.
- Store `dev-track land` already pushes `origin/main`; a claim refuses with unpushed local
  commits (`docs/guides/dev-tracks.md`).

## Not yet known

- Real wall clock of a Store `gate` in a lane while the owner's sessions hold other lanes;
  `max_active` defaults to 1 until measured.
- Whether Workspace Manager's caretaker or the runner should own workspace `release` once it
  exists; decided when its prototype lands.
