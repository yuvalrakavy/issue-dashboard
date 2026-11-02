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
- **A restored AgentBridge state replaying old answers.** The cursor rollback guard (3.4).
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
- `record-suggestions` is idempotent by launch (`suggest.rs:605-617`); `eval score` records with
  `Dedupe::Latest` and reports `repeat` (`eval.rs:1250`), so it is idempotent too; test row 9
  pins it.
- AgentBridge `token_status` (`src/dashboard/store.rs:1307-1372`) returns `unknown tracker
  token` as an error, else archived / result / queued / awaiting_caller / waiting_user / error /
  unknown / started, plus `never_started`.
- AgentBridge 0.2.4: strict write is Codex full clone only; worktrees refused; OpenCode and
  Claude write refused at preflight; `network` none|fetch is all-or-nothing; fence host-local.
- Store `dev-track land` already pushes `origin/main`; a claim refuses with unpushed local
  commits (`docs/guides/dev-tracks.md`).

## Not yet known

- Real wall clock of a Store `gate` in a lane while the owner's sessions hold other lanes;
  `max_active` defaults to 1 until measured.
- Whether Workspace Manager's caretaker or the runner should own workspace `release` once it
  exists; decided when its prototype lands.
