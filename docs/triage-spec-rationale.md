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

## The ECE simulation (why 100 and plain ECE)

The simulation draws top-answer confidences (0.25 + 0.75·Beta(5,2)), outcomes at exactly that
probability, 10 equal-width bins, and counts how often a backend fails the bar ECE ≤ 0.10.
**Re-run 2026-10-25 with the spec's exact formula** ([triage-ece-sim.py](triage-ece-sim.py), `python3 -I`, 3,000 runs per
cell; 2,000 for false passes). The first run's debiased column (18 % / 6 %) did not use the
formula the spec states and is withdrawn.

| Held-out | Honest backend fails — plain ℓ1 ECE (spec §3.4) | — debiased ℓ2 | Backend 0.15 overconfident passes — ℓ1 | — debiased ℓ2 |
|---|---|---|---|---|
| 50 | 47 % | 26 % | — | — |
| 100 | 13 % | 11 % | 8 % | 21 % |
| 200 | 0.4 % | 1.8 % | 4 % | 10 % |

Plain ℓ1 is biased upward, so at 0.10 it acts as a stricter bar; debiased ℓ2 estimates the true
error, so a backend truly at 0.10 passes about half the time. At 100 examples the two fail an
honest backend about equally often, and debiased lets more overconfident backends through.
The owner first chose 100 examples with the debiased estimator (Kumar et al. 2019) from the
withdrawn numbers, then, on these, **switched to plain ℓ1 ECE ≤ 0.10 at 100 held-out examples
(2026-10-25)**. 200 examples would cut both error rates but is out of reach: Store's priority
history holds about 150 held-out items.

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

### Confirming read of the fold (2026-10-25)

One reader (Sonnet, medium) checked the 15 fixes against the spec and code: 0 S3, 9 S2, 2 S1.
Every finding was checked against the code and all were folded:

| Finding | Fold |
|---|---|
| §4.6 missed the owner-only launch author check (`main.rs:492`), `holds_items`, schema and event additions, `suggest::BACKEND` | §4.6 rewritten as a table; §5.2 names it |
| estimates had no `decision` enum entry | `estimates`, a record (4.6) |
| `prepare` wrote every decision for every item | only the eligible (item, decision) pairs (2.1) |
| "the `map_id` in force" undefined; uncalibratable only in a test row | 2.4 defines both |
| ECE weights and unit not exact; the simulation used another quantity | simulation re-run (above); the owner then chose plain ℓ1, stated exactly (3.4) |
| false-alarm rate undefined for `area`; empty area truth | 3.4 |
| tuning size, eval manifest | 3.2 |
| drift population; duplicate close must name the pair; other-reason close | 3.5, 3.1 |
| comment after one label leaks to a sibling question | one cutoff per item (1.1) |
| empty `triage-request`; a line anchor 10 off | 5.2; 2.1 |

**Size.** With these folded in, the spec is about 28 KB, over the 20 KB T2 cap. What made it
grow is the definitions the measurement depends on (formulas, identity, truth folding). Leaving
any of them out is what the verification flagged as a defect. If the cap is to hold, the natural
split is (a) seam + evaluation, chapters 1–3, and (b) page + launch, chapters 4–5.
