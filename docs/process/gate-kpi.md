# Gate KPIs

One row per gated feature. Severity weights: S3 = 10 (a silent defect, or a test plan that
cannot detect what it exists for), S2 = 3 (loud failure, missing required test), S1 = 1
(precision), S0 = 0 (traced false). A convergent finding (two finders independently) counts once,
at S3 unless clearly smaller. SG (spec-delivery gap) is a tagged subset of IG.

| Date | Feature | Tier | Loop | DG | IG | SG | Post-merge | Lenses and cost |
|---|---|---|---|---|---|---|---|---|
| 2026-10-23 | phase 1: schemas, ledger core, ledger CLI | T2 | spec gated 3x before the build (no design gate this session) | n/a | round 1: 6 convergent S3 = 60; singletons: stale WAL adopted by rebuild S3, several torn lines S2, aside overwritten S2, reserve-next blocked S2, 5x S1 = 24; 11 mutation survivors (coverage gaps) S2 = 33. Round 2: 4 convergent S3 = 40; read error taken for divergence S3, WAL removed before rename S3 = 20; freshness lost on rebuild S2 (accepted residual) = 3; stale docs S1 = 1. Round 3: checkpoint status ignored S3, missing-ledger read error S2 = 13. Round 4: checkpoint error path S3, VACUUM failure blocks rebuild S2 = 13 (shape changed: rename aside). Round 5: lock-free reader across the renames S3 (left open, recorded), interrupted move separates main and WAL S2 = 13. **IG = 220** | schema validation: 10.3 says the CLI validates against the schemas; the first build did not (S3, convergent) | — | mutation (Sonnet, worktree): 11 survivors; breadth (Sonnet) x2; depth (Codex) x3. lens: convergent breadth+depth on 10 findings |

**Diagnosis (round 1):** the convergent class was "recovery and retry paths": a missing ledger, a
crash mid-rebuild, a retry after settle, a re-ingest after a refusal. The tests covered the
forward path of each mechanism and one crash point each, never the state the recovery itself
leaves. Round 2's class was "a fix narrower than its finding": the tail rule changed in repair and
not in rebuild; validation added after the insert rather than before it.

**Rounds 3-5:** every finding was in `rebuild`'s replace step, two per round: the decay did not
reach zero there. After round 4 the step was redesigned (rename the old set aside, delete
nothing) rather than patched a fourth time; round 5's two findings were concurrency with a
lock-free reader (recorded as open: needs a reader lease) and resuming an interrupted move (fixed
with `rebuild.pending`). Gating stopped at round 5 with the residual named, not at an empty round.
