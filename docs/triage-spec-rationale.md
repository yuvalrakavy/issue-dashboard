# Phase 2 triage: rationale

The reasons behind [triage-spec.md](triage-spec.md): what was checked before designing, the
alternatives that were weighed, and the simulation behind the bars. You only need this file to
challenge a decision; the spec itself is enough to build from.

## Duty 0: what was checked (2026-10-24)

| # | Claim | What is true | Evidence |
|---|---|---|---|
| 1 | Store has about 68 type-labelled and 69 priority-labelled open issues | Confirmed: all 69 open issues carry both. The spread is lopsided: priority p2 35 · p3 31 · p1 3 · p0 0; type todo 37 · bug 31 · docs 1 | `gh issue list` |
| 2 | Many priorities came from the 2026-10-23 bulk review | Mostly not: 52 were added at filing and about 20 later, clustered on 08-29 (6) and 10-22/23 (13) | label events on the open issues' timelines |
| 3 | Those labels are the owner's ground truth | Doubtful: all 247 label events are by the account the sessions also use, and filing-time labels were most likely set by the filing session | event authors |
| 4 | Closed issues give training data | 345 closed (275 typed, 253 prioritised), but the live `history_days = 30` keeps almost none in the ledger | `projects/VillaRakavy.toml` |
| 5 | Recommend closures produces dismissal data | Not yet: 0 `suggested` events and 11 decisions in the live ledger; `accept_suggestion` and `edit_suggestion` have no effect in code | ledger counts; `crates/core/src/docs.rs`, `derive.rs` |
| 6 | Phase 1 keeps label events | Only their ids, not names or times | `crates/core/src/read.rs:41` |

Point 3 led to the provenance model (spec 3.1). Points 4 and 6 led to `eval refresh-history`.
Point 5 led to chapter 4's derive effects.

## Where `decide()` runs

A Claude session can't read its own token probabilities, so estimating them by sampling inside a
session means one subagent per sample.

| Shape | How | Why not / why |
|---|---|---|
| A. Inside the session, sampled | k subagents per question | about 69 issues × 6 decisions × 5 samples ≈ 2,000 agent calls per run |
| B. Inside the CLI, over an API | `issue-dashboard decide` calls a model API | needs keys and paid calls, and puts judgement into code where the project keeps it in sessions |
| **C. Prepare / answer / score** (chosen) | the CLI writes blinded questions; a backend states probabilities in one pass; the CLI scores, calibrates and judges | one pass per run; evaluation is plain code; an API adapter can be added later as another backend |

Self-stated probabilities from one pass are expected to be miscalibrated. That is why every
backend is calibrated (temperature scaling) and measured on held-out history before its badge
reads "evaluated".

## The ECE simulation (why 100 and the debiased estimator)

The simulation draws a backend whose probabilities are perfectly calibrated and counts how often
it still fails ECE ≤ 0.10 (10 equal-width bins):

| Held-out examples | Plain ECE fails it | Debiased ECE fails it |
|---|---|---|
| 50 | 48 % | 18 % |
| 100 | 12 % | 6 % |

At spec §7.1's 50 examples, chance decides whether an honest backend passes. The owner chose 100
examples together with the debiased estimator (Kumar et al. 2019) on 2026-10-25.

## Other choices the owner made

- **Second opinions only on a confident disagreement (≤ 0.2), at most 5 a run.** The other
  options were a card for every disagreement, which would flood the page on the 69 labelled
  issues, or no cards for labelled issues at all, which would hide a mislabelled p0.
- **Priority as its own decision, with the rubric as input**, rather than computed from severity
  and reach. A computed priority can't see context that the rubric doesn't encode, and a direct
  answer with the rubric's distance shown keeps both views visible.
- **One triage session with two scopes.** Keeping recommend-closures as a separate session would
  read the same issues twice a day.
- **The daily run goes through the dispatcher (option A).** A scheduled task can't pin its model.
  If the scheduled task were the triage session itself (option B), its model would be whatever
  the task happened to run on, guarded only by a self-check. Answers recorded under the wrong
  backend would corrupt both calibration and evaluation without anyone noticing.

## Verification (2026-10-25)

This was one fresh-context pass by Codex (GPT family, high effort, read-only) with two lenses:
implementer and correctness. It found 8 S3 and 7 S2 issues, 0 S1. Every one was checked
against the code, all were confirmed, and all are folded into the spec. None reopened a decision
the owner had made.

| # | Sev | Finding | Disposition |
|---|---|---|---|
| 1 | S3 | Blinding leaked across questions: the type input carried the priority label, and one session answers every question | No input carries any label decision's label, the brief prints none, and replay cuts off at the truth time (1.1) |
| 2 | S2 | Priority's inputs depend on answers that don't exist when `prepare` runs; closure candidates were unnamed | Two-stage prepare (2.1); a closure answer names its subject (1.4) |
| 3 | S2 | §6.7's rubric is qualitative, so `rubric_level` couldn't be computed | A fixed table (1.5) |
| 4 | S3 | Area marginals give no set confidence | Area is calibrated and measured per area as yes/no; accuracy is exact-set (1.4, 3.4) |
| 5 | S3 | Top-answer ECE doesn't validate the 0.2 gate | Classwise ECE and the gate's false-alarm rate are reported and shown on cards (3.4, 4.1) |
| 6 | S3 | The debiased ECE formula was undefined at the edges | Exact formula, edge cases and bootstrap specified (3.4) |
| 7 | S3 | Bulk accepts would manufacture accuracy | `owner_bulk` is never truth; accepts record `from_backend` (3.1, 4.3) |
| 8 | S3 | Evaluation identity was too weak | Project in the key, immutable `map_id`, badge from the card's own map, hash covers the questions it depends on (1.3, 2.4, 3.5) |
| 9 | S3 | Freshness and retry stopped short of recording and accepting | Map frozen at prepare, stale answers dropped, stale accepts refused (2.1, 4.2) |
| 10 | S2 | No path for answering tuning items | `eval prepare` writes tuning and held-out files; fit, freeze, then score (3.2) |
| 11 | S3 | Truth folding and closure attribution undefined | Folding rules; closure truth bound to its own suggestion, its reason and a 14-day window (3.1) |
| 12 | S2 | Code-only changes never re-asked closures; abandoned runs counted as asked | Re-ask on a new commit or after 14 days; a question counts as asked once recorded (2.2) |
| 13 | S2 | Contract changes not listed | §4.6 |
| 14 | S2 | No wake path for a `schedule` request; launches with no items are refused | The request names its selected items; the task messages the dispatcher, marked verify during implementation (5.2) |
| 15 | S2 | Test rows lacked seams and fixtures | Rows added; replay fixtures gain comment bodies and label times (6.1) |

**Size.** With these folded in, the spec is about 25 KB, over the 20 KB T2 cap. What made it
grow is the definitions the measurement depends on (formulas, identity, truth folding). Leaving
any of them out is what the verification flagged as a defect. If the cap is to hold, the natural
split is (a) seam + evaluation, chapters 1–3, and (b) page + launch, chapters 4–5.
