# Harness spec — rationale

Companion to [harness-spec.md](harness-spec.md). Read it to challenge a decision; the spec
itself holds only the decisions.

## Alternatives weighed

- **1.2 The model states tokens and minutes directly.** Rejected: those numbers cannot be
  checked or calibrated per model; sizes can, against fixed minute boundaries (6.2).
- **1.6 Convert old `effort` into phases.** Rejected: a made-up design size would be shown as a
  real estimate. First proposal was "wait for the next triage"; the owner changed it to
  automatic re-triage, which needs `select` to know about estimates (it did not:
  `triage.rs:852-918` selects only on due label questions).
- **2.2 One class per model (A/B/C).** First proposal put Fable and Opus in one class A. The
  owner pointed out Fable costs far more; capability and cost became separate axes, cost
  computed from prices so it cannot go stale.
- **2.3 Level 4 as a minimum anywhere.** Rejected: it would force the most expensive model;
  offering it as a labelled option keeps the trade-off visible.
- **2.5 Owner declares in config which harnesses honor skills.** Replaced by the probe at the
  owner's request ("probe AgentBridge harnesses about their ability to write ... project
  specific"). First version required write ability for design too; the owner observed design
  only delivers a document, so the grade split in two.
- **3.3 One "value" score mixing capability and cost.** Rejected: hides the trade-off the owner
  asked to see.
- **3.8 Cheapest qualified first, forever.** Kept only as the cold-start behaviour; the owner
  asked to learn capability from reviews because intelligence need not follow price.
- **3.9 Redo receives the rejected design.** Rejected by default: it anchors the redo to the
  structure that failed. Overrulable.
- **3.9 Auto-start a redo.** Rejected: it spends money; a standing rule may come later, off by
  default.
- **4.1 Rank in page JavaScript.** Rejected: duplicates the ranking logic; nine embedded lists
  are small.
- **4.3 One dropdown of whole plans.** Rejected: combinations multiply and hide which phase
  drives the cost.
- **5.3 Design the runner inside this spec.** Rejected: it serves eval, triage, design and
  review launches, its risks are different (idempotency, replay, uncertain launches), and it was
  already queued. Owner then ordered: runner designed and landed first.
- **5.9 Implementation session runs its own gate subagents (Store's practice).** Rejected here:
  review must be a separate, recorded launch on another family.
- **Nine per-size lists embedded once (old 4.1).** Withdrawn at the design gate: a list
  depends on repo, tier, implementation family, quota and batch points, so offers are per item
  (3.2). Ranking still never happens on the page.
- **Learning inside this spec.** Split out (owner, 2026-11-02): the formulas would have been
  guessed without data and pushed the spec past its cap. Recording stays here so the data is
  right from the first launch.
- **Review launches keep holding items (kind `review`).** Rejected: a shared session paused at
  `design_ready` holds the item, so a reviewer could never reserve; the group owns the items
  instead (5.1).

## Design-gate dispositions (Codex gpt-6.1-sol, high, 2026-11-02)

| # | Sev | Finding | Where folded |
|---|---|---|---|
| 1 | S3 | reviews vs item reservations in a shared session | 5.1, 5.4 |
| 2 | S3 | phase order has forward dependencies | 6.3 |
| 3 | S3 | eligibility contradicts selection | 2.4 |
| 4 | S3 | nine lists miss ranking inputs | 3.2, 4.1 |
| 5 | S3 | batch points outside the schema | 1.5, 3.1 |
| 6 | S3 | attribution per launch insufficient | 5.2, 5.7, 6.1 |
| 7 | S3 | write-enabled bridge has no path | non-goals, 2.5 |
| 8 | S3 | plan cost omits launches | 3.5 |
| 9 | S2 | learning formulas missing | learning spec |
| 10 | S2 | selection bias | learning spec |
| 11 | S2 | estimates train themselves | learning spec |
| 12 | S2 | quota window semantics | 2.7, 3.6 |
| 13 | S2 | estimate obligations | 1.6 |
| 14 | S2 | edits bypass consistency | 1.3, 1.7 |
| 15 | S2 | artifact identity under-keyed | 5.2, 5.5, 5.6 |
| 16 | S2 | review completion undefined | 5.7, 5.8 |
| 17 | S2 | failure states incomplete | 5.10 |
| 18 | S2 | probe and ingestion contracts | 2.1, 2.5 |
| 19 | S2 | cost inputs missing; wrong gate source | 1.5, 2.2, 3.1 |
| 20 | S2 | tests permit these failures | 6.2 |

**Confirming pass** (Codex gpt-6.1-sol, medium, on 137cff6): 14 resolved, 6 partial, plus
N1 S3 review family is per slot (2.4), N2 S3 a round stays pending until every slot and
finding is settled; `reject` is a claim (5.7), N3 S2 holds are per slot (3.6), N4 S2 stale
answers leave the effective estimate (1.7), R1 S2 phase 4 is implementation-slot only (6.3);
each has a row under "gate-2 cases" (6.2). **Third pass** (Codex, high, on 40a9c97): N2–N4
and R1 resolved, trimming lost nothing; two S2 — T3 second-reviewer lists keyed by the family
pair and every family-rule breach overridable (2.4, 3.2), and every non-`none` design is a
plan slot whether split or not (3.5).

Verified against code before folding: #1 (`docs.rs:49`, `launch.rs:496`), #7 (AgentBridge
`store.rs:196`), #14 (`docs.rs:199`), #19 (`config.rs:81`).

## Traps the design guards against

- **Drifting size boundaries.** If S/M/L were defined by the learned table, a drifting table
  would redefine sizes and make triage's accuracy look perfect. Boundaries are fixed minutes.
- **Selection bias, reviewer strictness, cheap models never tried** — moved to the learning
  spec, which owns quality.
- **Double-counted quota.** A running launch's estimate and its later measurement must not
  both count; the hold is replaced (3.6).
- **Unknown usage read as zero.** Would make an option look free and empty a quota silently;
  unknown counts at its estimate.

## Facts the design rests on (verified 2026-11-02)

- `Routing{model, effort, workspace?, reason?}` has no harness (`docs.rs:17`).
- Estimates are volunteered entries recorded as decision `estimates` (`suggest.rs:420-439`);
  `select` ignores them.
- Outcome records hold launch totals only (`outcome.rs`).
- AgentBridge 0.2.2: strict `start` is read-only (`src/dashboard/store.rs:197`); output can be
  staged and sealed into a transfer manifest without checkout writes; Claude strict routing
  tested only at Haiku low; Codex and OpenCode ready; local host only.

## Not yet known

- Whether a read-only Codex or OpenCode worker can read a checkout outside its bound folder.
  The deliver-enabled probe answers it; nothing assumes it.
- Real token figures per (phase, size); seeds are guesses until outcomes exist.
- Bridge implementation (AgentBridge handoff 3, reconciled 2026-11-02 in its
  `docs/DASHBOARD-HANDOFF-3-REVIEW.md`, in progress): full clones first, linked worktrees
  likely blocked (their git state is outside the root); OpenCode may stay unavailable for
  unattended implementation (permission patterns are not an OS sandbox); dependency fetch is
  not proven separable from push, so the runner should prefetch and build offline.
