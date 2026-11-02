# Harness learning — design spec (not started)

Status: **placeholder, split out of [harness-spec.md](harness-spec.md) on 2026-11-02** at the
design gate. To be designed (T2) once real outcomes exist from the harness spec's phase 3
recording, so the arithmetic is fitted to data rather than guessed. Until then every figure is
the harness spec's seed (3.1), labelled "seed", and ranking uses plain $ (3.4).

## What it must decide

Each item is an open design-gate finding against the original combined spec (numbers are the
gate's).

- **Calibration of tokens and minutes** per (phase, size, repo): measured-only eligibility
  (`usage_source = native`; estimates never train themselves, #11), effort normalization so an
  observation is not multiplied by its effort twice, pooled-cell membership across repos,
  minimum samples, and treatment of failed or censored attempts. Size boundaries stay fixed:
  S ≤ 30 min, M ≤ 90, L > 90 (approved, chapter 6).
- **Quality per (model, effort, phase)** from verified findings: weights, denominators,
  severity weighting, the neutral prior, reviewer-strictness adjustment, and missing-data
  behavior, with worked numbers (#9).
- **Expected total cost** = $ + expected rework, its interval, and a deterministic total
  order when intervals overlap (#9).
- **Selection bias** (#10): record exposure and review coverage; label comparisons
  observational unless reviews are shared or the owner approves exploration; how unchosen
  options get observations.
- **Estimate accuracy** as an evaluated question with a badge; suggested level and budget
  changes the owner accepts (approved, chapter 6).
- **Demotion** of an option after repeated failed rounds (removed from the harness spec until
  quality is measured).

## Inputs it can rely on

The harness spec records, from its phase 3: per-attempt usage checkpoints and `usage_source`,
quota consumption records, the offer and pick per launch, overrides, and append-only
`finding` / `finding_disposition` events keyed by attempt, revision and round.
