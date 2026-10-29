# Phase 2 triage — design spec

Status: **chapters 1–6 approved 2026-10-25; the fresh-context verification is folded** (T2;
dispositions in the rationale, "Verification"). Extends `docs/spec.md` §5.7–5.8, §6.7, §7.1–7.6
and §8; where this file and `spec.md` differ, this file is the newer decision and `spec.md` is
corrected when it lands. What was built differently is the "As built" table at the end. Alternatives weighed and the probes behind them:
[triage-spec-rationale.md](triage-spec-rationale.md).

## Intent

The triage session produces suggestions through the decision seam (spec §7.1): type, area,
severity, reach, detectability, priority, closures, support questions. A suggestion changes
nothing until the owner acts (spec §5.8). Every probability a backend states is measured on the
project's own history before it is trusted.

**Settled with the owner (2026-10-24/25)**, each in its chapter: prepare / answer / score (ch. 2);
every label is truth, tagged by provenance (3.1); second opinions only on a confident
disagreement (4.1); recommend closures merged into triage (5.1); priority its own decision with
the rubric as input (1.1, 1.5); the bars (3.4); the daily run through the dispatcher (5.2). The
alternatives are in the rationale.

**Non-goals here:** WSJF ordering (spec ch. 6) and the GitHub write engine (spec §10.9) are
later phase-2 steps; this spec names where they connect (ch. 5) and designs neither.

## Chapters

1. The decision catalogue — approved 2026-10-25
2. Prepare / answer / score, and the triage run — approved 2026-10-25
3. Ground truth and `eval` — approved 2026-10-25 (option B)
4. Suggestions on the page — approved 2026-10-25
5. Launch and routing — approved 2026-10-25 (daily run: option A)
6. Tests, phasing, risks — approved 2026-10-25

---

## 1. The decision catalogue

### 1.1 The questions

Every question below goes through `decide(question, allowed answers, input)`. **Blinding covers the
whole answering context:** no input carries a label of ANY label decision (type, area, priority),
the triage brief prints none, and the skill forbids reading an issue on GitHub while answering
label questions, so no answer can be copied from a sibling question's input. A replay input
(3.2) holds the body and only the comments before one cutoff per item: the earliest truth time
among its label decisions, so a comment like "bumped to p1" cannot reach its type question.

| Decision | Allowed answers | Input beyond title, body, inside comments | Scope | Shown as |
|---|---|---|---|---|
| `type` | bug · todo · decision · docs | non-decision labels only (e.g. `upstream`) | `full` | a card if untriaged; a second opinion (4.1) if labelled |
| `area` | the repo's area names, **a set** of at least one; a yes/no probability per area | the repo's area list | `full`, repos that declare areas | as `type` |
| `severity` | cosmetic · degraded · wrong_result · hang_or_data_loss (1/3/8/13, spec §6.2) | — | `full` | Details, editable; no card |
| `reach` | one_corner · one_subsystem · most · all (1/2/3/5) | outside demand (spec §4.3) | `full` | Details, editable |
| `detectability` | silent · visible | — | `full` | Details, editable |
| `priority` | p0 · p1 · p2 · p3 | stage 2 (2.1): the three above as recorded, calibrated, and the rubric table (1.5) | `full` | as `type`; the card notes any distance from the rubric |
| `likely_fixed` | true · false, about a named commit | commits mentioning the issue (spec §2.3), the session's check against the code; the answer names one commit | both | "Check and close…" |
| `duplicate` | true · false, about a named pair | both issues; the answer names one other open issue | both | "Close as duplicate…" |
| `obsolete` | true · false | what overtook it | both | "Close as not planned…" |
| `support_question` | true · false | outside items only | `full` | "Close as not planned…", the owner's close sheet with a drafted reply |
| `instructs_agent` | true · false | outside text only | both | no card: a warning on the item, in Details and in its briefs (spec §4.2) |

**Recorded as unevaluated estimates, not seam questions:** sites (with the commit they were read
at), tier, blast and effort S/M/L, which feed WSJF job size (spec §6.3), batches (§6.6) and
routing (§7.4); each is prose judgement after reading code (§7.1's last paragraph). Effort gains
ground truth from outcomes (§6.8) and becomes a seam question then. **Batch** is grouping, not a
typed answer; **routing** is the dispatcher's, from §7.4's table.

### 1.2 One question file per decision

`decisions/<decision>.toml` in this repo holds the question text, the allowed answers, the
rubric, and any per-repo extension (area names come from the repo's translation). `prepare` and
the triage skill both read it, so the wording a session sees and the answers the CLI accepts
cannot drift apart.

### 1.3 What a backend is, and what "evaluated" names

| Key | Example | Changes when |
|---|---|---|
| **backend** | `claude/opus-5-5@medium`, `jev/1` | the model or effort changes |
| **question hash** | first 12 hex of sha256 over `decisions/<d>.toml`, the files of the decisions it reads (priority: severity, reach, detectability), and for `area` the repo's area list | anything that changes what the question means |

An evaluation and its calibration map are keyed by **(project, backend, decision, question
hash)**, and every map has an immutable `map_id` (the hash of its contents). Rewording the area
question drops area's badge and no other. A card is marked "evaluated" only
when its answering backend has a passing evaluation under the same question hash.

### 1.4 Answer shape

A single-choice question answers with a probability per allowed answer; the CLI accepts a sum
within 1 ± 0.02 and normalises it. `area` answers with a marginal probability per area; the set
chosen is every area at 0.5 or above, or the top one if none is; area is calibrated and measured
as one yes/no question per area (3.4). A closure answer names its subject (one commit, or one
other open issue), at most one per decision per item, and `record-suggestions` checks it exists.
Every answer carries a short `reason`, shown on the card, and never fed into another question.

### 1.5 The rubric table

`decisions/priority.toml` holds it: points = severity points × reach points (spec §6.2); ≥ 26 →
p0, ≥ 9 → p1, ≥ 3 → p2, else p3; `silent` raises one level. `rubric_level` is the table applied to
the top answers. Spec §6.7's "likelihood" is folded into reach. Editing the table changes
priority's hash.

---

## 2. Prepare / answer / score, and the triage run

### 2.1 One run

| Step | Who | What |
|---|---|---|
| 1 | dispatcher | starts a `triage` launch (scope `full` or `closures`) whose request names the items 2.2 selected at request time (an empty list is refused, `crates/core/src/launch.rs:384`), with `brief --launch` (ch. 5) |
| 2 | session | `issue-dashboard prepare --launch <id> --session-model <m@e> --out questions.json` (refused when `m@e` is not the routing): builds each blinded input for the launch's items, writes one question per (item, decision) that 2.2 finds eligible (recomputed per item: the request names items, not questions), and records a **`prepared`** event: launch, backend, and per question its id, decision, question hash, input hash and the `map_id` in force |
| 3 | session | checks each item against the code (duty 0: cited sites, commits that mention it), then answers every question in `answers.json`, plus estimates, batches and left-out entries |
| 4 | session | `issue-dashboard record-suggestions --launch <id> answers.json`: every answer is checked against its `prepared` question, the calibration map (2.4) applied, and `suggested` events recorded with the **raw** and the **calibrated** probabilities |
| 5 | next sync | cards appear (ch. 4) |

**Two stages:** step 2 prepares every decision but priority; after step 4 records them,
`prepare --stage priority` builds priority's inputs from those calibrated answers, and steps 3–4
repeat for it. **Only asked questions can be answered:** an answer to an unprepared question
refuses the file; an answer whose item changed since prepare (its input hash, recomputed from the
ledger at record, differs) is dropped and reported, and the rest is recorded. The map applied is
the one `prepared` froze, so a retry of the same file records identical payloads (phase 1
compares them, `crates/core/src/suggest.rs:355-359`). **The backend is the launch's routing** (the model and
effort the dispatcher started), never the session's own claim; the answer file's `backend` must
equal it.

### 2.2 Which items a run asks about

A question is asked about an open item when it was never asked under the current question
hash, or the item's latest observation is newer than the one it was last asked against (spec
§5.8, "facts change"), or the owner asked (Triage… in an item's `⋯` menu). Untriaged items
first, then the oldest last triage, capped at `max_triage_items` (project config, default 40).
`closures` selects by the same rule over its decisions, and also re-asks when a new commit
mentions the item or after `closure_recheck_days` (default 14). A question counts as asked once
**recorded**, not prepared, so an abandoned run leaves its items eligible.

### 2.3 How a session states probabilities

One pass, no sampling. Per question the skill has the session note the evidence for and
against, then give a probability per allowed answer as "how often would I be right across many
issues like this"; the CLI clips to [0.01, 0.99]. The stated numbers are corrected (2.4) and
measured (ch. 3), never trusted.

### 2.4 The calibration map

**Temperature scaling, one `T` per (backend, decision, question hash)**, fitted on the tuning
split (ch. 3) by minimising log loss over the grid 0.25–4 in steps of 0.05: deterministic,
usable from a few dozen examples, and it keeps the backend's ranking. With a map, calibrated =
softmax(log p / T) (per area as yes/no); without one, the identity, and the suggestion is
marked **uncalibrated**. Maps live in `state/evals/<project>/<backend>/<decision>-<hash>-<map_id>.json`,
written by `eval`. **The map in force** for a key is the one its latest `evaluated` event names,
passing or not (the badge says which); with none, `map_id` is null and suggestions are
uncalibrated. A fit at the grid edge writes no map: the key stays uncalibrated and its
`evaluated` event records `uncalibratable`. A suggestion keeps the map its `prepared` event
froze, and its badge is that map's evaluation.

### 2.5 "Needs a stronger look"

A suggestion on `priority`, `type`, `duplicate` or `likely_fixed` whose calibrated top
probability is below `stronger_look_below` (default 0.6) is recorded, and an Inbox item "needs a
stronger look" lists it; its button is an ordinary `stronger_look` launch at the stronger routing
(spec §7.4, §10.8), which answers only those questions under its own backend. The newer
suggestion replaces the older on the card.

---

## 3. Ground truth and `eval`

### 3.1 Truth records

The truth for an (item, decision) is the **latest** record from these sources; each keeps its
provenance, so an evaluation can drop any of them:

| Source | Decisions | Provenance | Told apart by |
|---|---|---|---|
| a label added at filing | type, area, priority | `filed` | its labelled event is within 10 minutes of the issue's creation |
| a label added or changed later | same | `github_later` | any later labelled / unlabelled event |
| a tagged review | same | `bulk_review:<name>` | the label event is in a `truth tag` set (3.3) |
| accept on the page | any | `owner_accept` | `accept_suggestion` |
| an edit | any | `owner_edit` | `edit_suggestion`; `set_priority`; `set_override` with type or areas; a dismissed second opinion (confirms the current label); a Details edit of severity, reach, detectability |
| a closure verdict | likely_fixed, duplicate, obsolete, support_question | `owner_accept` / `owner_edit` | for its own suggestion document only: accept, or the owner's close of the item within 14 days with the matching reason (completed → likely_fixed, duplicate naming the suggested other issue → duplicate, not planned → obsolete / support_question): **true**; a dismissal with a reason: **false**; a close with another reason, or a dismissal without one: **no truth** |

**Folding:** records are replayed in time order; an area truth is the set built from adds and
removes, with the provenance of the latest event touching it; an undone decision removes its
record. **An apply-all accept (4.3) is `owner_bulk` and never truth.** `owner_accept` and
`owner_edit` keep the suggestion's backend (`from_backend`), and accuracy on them is reported
separately.

### 3.2 Two modes

- **replay** (label decisions): `eval prepare <decision> --backend B` writes two files, a tuning
  sample (at most `eval_tuning`, default 120) and a held-out sample, built as 2.1 with the
  replay cutoff (1.1), plus a manifest of question ids and input hashes; one `eval` session
  answers both; `eval score` re-verifies the manifest, fits the map on tuning, freezes it, then
  scores the held-out set.
- **live** (closure decisions): scores recorded suggestions against the verdicts that followed;
  nothing is re-asked, since "is this fixed" depends on the code of that moment.

**Label decisions are answered from the issue's text alone, live and in replay**; code reading
serves closures and estimates. So replay measures what the live session does, and a closed
issue is never judged against today's code.

**History:** `eval refresh-history` reads closed issues back to `eval_history_days` (default
730) with each translated label's events (name and time; phase 1 keeps only their ids,
`crates/core/src/read.rs:41`), through the `GitHub` trait (replay in tests), into
`state/evals/history/`, not the ledger: the page's `history_days` is unaffected.

### 3.3 Tagging a bulk review

`issue-dashboard truth tag --name <n> --decision <d> --from <t> --to <t> [--issues …]` lists the
later label events in the window (Store, 2026-10-22/23 UTC: about 13 priority events) and
records one `truth_tag` event per label event only with `--yes`. `eval … --exclude
bulk_review:<n>` then drops them.

### 3.4 Split, metrics, bars

- **Split by item:** the first 40 % of `sha256(node id)` is tuning, the rest held out, for every
  decision; stable as truth grows, and no item is in both.
- **Sample:** a replay asks at most `eval_sample` held-out items (default 120), in hash order.
- **Accuracy:** the top answer equals the truth (`area`: the chosen set equals it exactly).
- **ECE:** on the top answer's calibrated confidence (area: on every per-area yes/no marginal), 10
  bins `[k/10, (k+1)/10)`, the last closed; **plain ℓ1** (owner, 2026-10-25): ECE =
  `Σ_b (n_b/N)·|ā_b − c̄_b|`, ā the bin's accuracy, c̄ its mean confidence, empty bins skipped.
  **Interval:** 1,000 bootstrap resamples
  by item, seed = the key's hash, the 5th and 95th percentiles. A non-finite result refuses the eval.
- **Gate checks:** each eval also reports per-answer (classwise) ECE and the second-opinion rule's
  **false-alarm rate**: the share of held-out items whose truth answer got calibrated p ≤ 0.2
  (area: the share of (item, area) pairs in the truth set given p ≤ 0.2). Second-opinion cards
  show it; the bars below are unchanged. An item whose folded area truth is empty is left out of
  area's eval.

| Bar | Needs (**owner, 2026-10-25**: amends spec §7.1's 50) |
|---|---|
| **Evaluated** | at least **100** held-out examples and ECE ≤ 0.10 |
| **Bulk** | Evaluated, plus accuracy ≥ 85 % |

Why 100 and plain ECE: rationale, "The ECE simulation".

### 3.5 Records, drift, cold start

Each eval records an `evaluated` event (project, backend, decision, question hash, `map_id`, n,
accuracy, ECE and interval, classwise ECE, false-alarm rate, exclusions, bars passed, fitted `T`)
and writes the map. A card's badge is the evaluation of its own map; the launch sheet shows the
key's latest; `eval_exclude` in the project config makes an exclusion the default.
**Drift:** live accuracy is the agreement rate over a key's last 30 `owner_accept` / `owner_edit`
records whose `from_backend` is the key's backend (each a card the owner read; never `owner_bulk`):
the share where the truth equals the suggestion's top answer. When it falls more than 10 points
below its evaluated accuracy, its cards say "drifting" and "apply all" is suspended for it until
a new eval passes. **Cold start:** a key without a passing eval is unevaluated (uncalibrated
without a map) and never in "apply all"; severity, reach, detectability and the closure
decisions start there.

---

## 4. Suggestions on the page

### 4.1 Cards

| Case | Card |
|---|---|
| untriaged on type and priority | one combined card, "Set bug · p1" |
| one axis missing | a single card for it |
| labelled, the label's calibrated probability ≤ 0.2 | a **second opinion** naming the current label (area: a labelled area at ≤ 0.2) and the gate's false-alarm rate; at most 5 a run, ordered priority > type > area, then by disagreement |
| labelled, agreeing | no card; recorded silently |
| priority more than one level from the §6.7 rubric | the card adds a rubric note |
| severity, reach, detectability, estimates | no card: in Details, editable |
| closures | as phase 1 (spec §5.8) |

A reporter's own request is marked; every card carries one badge: **evaluated**, **unevaluated**,
**uncalibrated** or **drifting** (ch. 3).

### 4.2 What accepting does

- **Accept type or area:** the item's value, source `suggestion`; precedence override > accepted
  suggestion > label (`crates/core/src/derive.rs`); "not written to GitHub yet" until the write
  engine (spec §10.9) consumes the decision.
- **Accept priority:** the same effect as `set_priority`. **Edit:** as accept, with the edited value.
- **Dismiss:** as phase 1; a dismissed second opinion confirms the current label (`owner_edit` truth).
- **Details edit** of severity, reach or detectability is stored for ordering; undo is the generic one.
- **Stale accept:** an accept refuses when the item's effective value changed since the
  suggestion's observation; apply all also rechecks Bulk and drift per key when it runs.
- The page decision `value` gains `severity`, `reach`, `detectability`.

### 4.3 Apply all

Only type, area and priority cards for untriaged items, on keys that pass the **Bulk** bar and are
not drifting; never second opinions or closures. One accept decision per card, recorded as
`owner_bulk` (never truth, 3.1).

### 4.4 Metrics

A metrics collection per key: the last 30 days' accepted, edited and dismissed counts ("type: 41
of 44"), the evaluated accuracy and the drift flag, shown on Trends.

### 4.5 Document fields

Suggestion documents add `raw` and calibrated `probabilities`, `badge`, `second_opinion` with
`current`, `rubric_level`, `from_reporter`. Issue documents add `triage`: severity, reach and
detectability, each {answer, probabilities, backend, badge}, plus the estimates (sites with
commit, tier, blast, effort).

### 4.6 Contract changes

| Where | Change |
|---|---|
| `schemas/common.schema.json` `suggestionBody` | `answer` also an array (area); the decision enum (`:219`) gains `detectability` and `estimates` (a record, not probabilities); `map_id`, `raw` |
| `suggest.rs` | `record-suggestions` accepts `triage`, `stronger_look` and alias launches (today only `RecommendClosures`, `:188`) and requires a `prepared` question; `BACKEND` (`:31`, hard-coded `recommend_closures`) becomes the launch's routing |
| `docs.rs` | `LaunchKind` gains `Eval`; `holds_items` (`:43-45`) is false for `triage`, `stronger_look`, `eval`; `LaunchRequest` / `LaunchData` (`deny_unknown_fields`) gain `scope`; `DecisionValue` gains the 4.2 fields |
| schemas | `page-launch` (kind enum `:37`) and `event-launch` gain `scope` and `eval`; new `event-prepared`, `event-evaluated`, `event-truth-tag` with `Kind` entries; `event-suggested` gains `raw`, `map_id`; `doc-issue` gains `triage` (4.5) |
| `derive.rs` | the accepted-suggestion layer |
| launch author check | `crates/cli/src/main.rs:492-497` also accepts author `schedule`, for kind `triage` only; `triage-request` records through `record_doc` as `sync-now` does (`crates/core/src/sync.rs:30`), bypassing `ingest::owned` |
| page actions | accept/edit name their card and component (a combined card's type or priority) |

Phase-1 suggestion and launch documents keep parsing.

---

## 5. Launch and routing

### 5.1 Launch kinds

| Kind | Change |
|---|---|
| `triage` | gains `scope` (`full` · `closures`); locks no items; runs in the `read-only` recipe pool (capacity 1); its request names the items 2.2 selects (or the owner named) |
| `recommend_closures` | an alias of `triage` with scope `closures`; phase-1 documents and buttons keep working |
| **Run triage…** (page) | an Inbox button opening the launch sheet: scope (`full` · `closures`), the routing, the "no evaluation" warning (5.3); disabled while a triage launch is live; records an owner launch request the dispatcher takes as today |
| `stronger_look` | unchanged (2.5) |
| `eval` (new) | read-only; answers one `eval prepare` file (3.2); from Trends "Evaluate…" or the CLI; its routing is the backend measured |

### 5.2 The daily run (**owner, 2026-10-25: option A**)

A scheduled task cannot pin its model, so it never answers questions. `issue-dashboard-triage`
(Haiku, low) runs `issue-dashboard triage-request --project P`, which selects the items (2.2)
and records a launch request authored by `schedule` with the configured routing (4.6), refused while a
triage launch is live; with nothing eligible it records nothing and reports "nothing to triage".
The task then messages the dispatcher session (found as the
`issues-dashboard` skill finds it) to wake it. The dispatcher reserves `schedule` requests as it
does the owner's and starts the session with that model and effort. The session passes its own
model and effort (`get_session self`) to `prepare --session-model`, which refuses a mismatch, and
the launch ends unanswered. **Verify during implementation:** whether a scheduled-task session can
message the dispatcher session; if not, the request waits for the dispatcher's next wake and the
task reports it as pending. On-demand
runs go through the dispatcher unchanged; every run ends with `launch-ended`.

### 5.3 Routing

triage: **Opus 5.5 medium** (the owner's 2026-10-24 recommender ruling; replaces spec §7.4's
Sonnet medium). stronger_look: the strongest model, high effort. eval: the backend measured. All
editable in project config. A changed routing is a new backend: its cards are unevaluated, and the
launch sheet warns "this routing has no evaluation" with **Evaluate…**.

### 5.4 Connections (named, not designed)

- **WSJF** reads the issue document's `triage`: severity and reach probabilities give §6.4's `p`,
  duty 0 done gives `u`, the estimates give job size and batches.
- **The write engine** (§10.9) consumes accepted type, area and priority decisions.
- **Routing corrections** (§5.7) are the truth for a future routing seam decision.
- **Phase 3** drafts support replies.

---

## 6. Tests, phasing, risks

### 6.1 Tests (each written red first)

| Claim | Test · level |
|---|---|
| blinding | no input and no triage brief holds a type, area or priority label, for any question; a replay input holds no comment after the truth time · core, replay fixtures gaining comment bodies and label names/times |
| only asked questions | `record-suggestions` refuses an unprepared id and a backend unlike the routing; drops (and reports) an answer whose item changed; a retry after a new map records identical payloads · core |
| two stages | priority cannot be prepared before stage 1 is recorded; its hash changes when `severity.toml` does · core |
| rubric | table boundaries 25/26, 8/9, 2/3, and the silent bump · core unit |
| calibration | synthetic answers at a known `T` → fit within one grid step; a fit at the grid edge reports "uncalibratable" · core unit |
| ECE and bars | ECE equals a hand-computed value on a fixed two-bin fixture (literal, e.g. 0.07); a one-item bin and empty bins are handled; same seed → same interval; bars flip at 99/100, 0.10, 85 % · core unit |
| gate checks | the false-alarm rate on a fixture whose stated 0.1 answers are true 40 % of the time · core unit |
| split | adding items moves no existing item · core unit |
| provenance | label at 9:59 → `filed`, at 10:01 → `github_later`; area add/remove folding; paginated history · core, replayed `GitHub` |
| truth | `owner_bulk` never truth; a close after 15 days or with another reason is no truth; an undone decision removes its record · core |
| truth tag | without `--yes` lists, records nothing · CLI |
| derive | override > accepted suggestion > label; source `suggestion` · core |
| second opinions | cap 5, order priority > type > area; agreement gives no card · core |
| apply all | never second opinions, closures, sub-Bulk or drifting keys; combined-card gating · core + `page/suggestions.test.js` (today it excludes type cards) |
| stale accept | accept refuses after the item's value changed · core |
| drift | on at 30 records and > 10 points; off after a passing eval · core |
| daily launch | `triage-request` refused while triage is live, a no-op with nothing eligible; the dispatcher reserves `schedule` requests of kind `triage` only, still refuses other non-owner authors and `schedule` on other kinds · core + CLI |
| eligibility | an item re-selected only for a closure recheck gets no `full` questions · core |
| closure truth | a duplicate close naming another issue than the suggested one is no truth · core |
| area eval | false-alarm over truth-set pairs; empty folded truth excluded · core unit |
| replay cutoff | a comment after an item's priority label time is absent from its type input · core |
| self-check | `prepare --session-model M` refuses a mismatch with the routing · core |
| alias | phase-1 `recommend_closures` documents parse; existing `suggest` tests stay green · core |

### 6.2 Phasing

1/5 **Seam**: `decisions/*.toml`, question hash, `prepare`, `record-suggestions` for every
decision, `prepared`, raw + calibrated (identity). 2/5 **Eval**: `refresh-history`, truth,
`truth tag`, split, `eval prepare/score`, ECE, bars, maps, drift; ends with a first real eval on
VillaRakavy history. 3/5 **Page**: ch. 4. 4/5 **Launch**: ch. 5, the scheduled task. 5/5 **Docs +
skills**: a triage skill absorbing `docs/closing-recommendations.md`; the dispatcher and
`issues-dashboard` skills; README; `docs/triage-tutorial.md`; the spec.md corrections (§7.1's 50 →
100 with plain ECE ≤ 0.10, its "already have enough examples" line, §7.4's routing, §5.8's alias,
§6.7's likelihood folded into reach).
Inline, one pre-merge deep gate; phases 1–2 may merge before 3–4.

### 6.3 Risks

- **History volume:** 730 days of label events may meet rate limits. **Verify during
  implementation:** time `refresh-history` on VillaRakavy; over budget → resumable by page cursor,
  never a shorter window.
- **Thin truth:** area and sparse repos may never reach 100; they stay unevaluated, visibly.
- **Circular labels:** measured per provenance, excludable. **Cost:** one daily run, ≤ 40 items.

---

## As built

Where the code differs from this spec, the code is current. One row per divergence, by phase.
"Open" marks a ruling the owner has not made. Section references are to this file.

### 1/5 Seam (#25)

| § | As built | Why | PR |
|---|---|---|---|
| 1.3 | A backend is `model@effort` with the app's model id, `claude-opus-5-5@medium` | it is the routing's own two fields | #25 |
| 1.4 | The commit-exists check is a read-only GraphQL lookup whose returned `oid` must start with the hash named | a ref can pose as a hash | #25 |
| 1.1 | Live inputs carry no comment bodies | the sync reads none yet | #25 |
| 2.3 | Probabilities are clipped, then normalised; a boolean tie answers false | one rule for every kind | #25 |
| 2.2 | A new commit mentioning an item does not make its closure decisions due again; the recheck clock does | no commit-mention signal in `due()`; open S2 | #25, #32 |

### 2/5 Eval (#26)

| § | As built | Why | PR |
|---|---|---|---|
| 3.2 | A history records its window and the member filter; a resume or eval under another is refused | results stay comparable | #26 |
| 3.1 | A single choice takes the latest value still standing; closure exclusions apply before the latest verdict | an excluded verdict must not leave an older one behind | #26 |
| 3.2 | Manifest paths are percent-encoded; `score` refuses another key's manifest and exclusions unlike the manifest's | no collisions; no silent re-prepare | #26 |
| 3.3 | Tag identity is per project, deduped against the latest tag only | a changed tag records again | #26 |
| 3.5 | Drift counts closure keys too | their records are owner records | #26 |
| 3.4 | The split hash is 15 hex digits; `refresh-history` takes no lock | enough bits; it only reads | #26 |

### 3/5 Page (#31)

| § | As built | Why | PR |
|---|---|---|---|
| 4.1 | A combined card is two suggestion documents sharing `card`; the second-opinion cap counts the run's latest opinions | acting on one must not bring a sixth | #31 |
| 4.2 | An acted card hides and a run no longer re-answers it; the stale check covers accept, edit and a second opinion's dismissal | an accept must not change underfoot | #31 |
| 4.3 | Apply all needs the card's own map's Bulk, the key's latest Bulk and no drift, for every part of a combined card | an unread error is the cost | #31 |
| 4.5 | `doc-issue` also carries `type_source`, `areas_source`; a pre-phase suggestion has no `current` and no card | Details says "not written"; nothing to compare | #31 |
| 4.1 | Estimates were display-only in Details; editable since "Rulings" below | 4.2 and 4.6 gave the value three fields | #31 |
| 4.4 | Trends metrics and the `instructs_agent` warning are not built | later; built in "Spec gaps" below | #31 |

### 4/5 Launch (#32)

| § | As built | Why | PR |
|---|---|---|---|
| 4.6 | The schedule request goes through `launch::request` (`Via::Schedule`), not `record_doc`; `reserve --launch` checks the author | same effect | #32 |
| 5.1 | Only `triage` takes a `scope`; an eval names no items; triage, the alias and eval must use the `read-only` recipe | the table was ambiguous | #32 |
| 5.2 | `triage-request` also returns `pending`; an owner request unheard for 24 h stops counting as live; `launches --requested` | an unheard request blocked every run | #32 |
| 5.3 | The strongest model is `claude-fable-5-1`; `[routing.<kind>]` overrides; the warning keys on any evaluation of the backend | defaults must exist | #32 |
| 2.2 | "Untriaged" is the page's definition; read-only members are excluded; **Request triage** makes an item due | one definition | #32 |
| 5.1 | `recommend_closures` kept its phase-1 brief; on the triage brief since "Rulings" below | the scoped brief dropped its batching, which tests pinned | #32 |
| 2.5, 5.2 | No "needs a stronger look" item (built in "Spec gaps" below); the task is not registered, and whether it can message the dispatcher is unchecked | after approval | #32 |

### Spec gaps

What phases 1-5 left unbuilt. Every row is overrulable.

| § | As built | Why | PR |
|---|---|---|---|
| 2.5 | An answer is marked `needs_stronger_look` when its calibrated top probability is strictly below `stronger_look_below` (project config, default 0.6), a boolean closure included; a stronger look's own answers are never marked | the contract table's reading; no loop of stronger looks | gaps |
| 2.5 | The list is the issue document's `triage.stronger_look`: the project's latest marked answers, about the item as it is now, of an open item, not accepted or edited, and with no dismissal of that (item, decision) since the item was last observed (the card's rule). A stronger look prepares exactly that list | one rule for the Inbox, Details and the launch; an answer about older facts is due for an ordinary run, which re-asks it with fresh stage-one answers | gaps |
| 2.5 | A stronger look prepares in one stage (priority included, from the recorded stage-one answers); `--stage priority` is refused; it records answers only, no suggestions, `left_out` or estimates | "answers only those questions" | gaps |
| 2.5 | `stronger_look` is not added to the CLI's `read_only()` list; the page still sends the `read-only` recipe | 5.1's pinned "unchanged" | gaps |
| 4.4 | A metrics document per (backend, decision, question hash), `kind: "triage_key"`, beside the Trends shape in one `oneOf` schema; apply all's accepts count in `accepted` and apart as `bulk`; a key shows when the window holds a decision on one of its suggestions or the project holds an evaluation of it; a suggestion with no prepared question is not counted | "41 of 44" needs its base; `bulk` keeps bulk accepts visible | gaps |
| 4.4 | Accuracy is the key's latest passing evaluation's | the drift baseline (3.5) | gaps |
| 4.4 | A closure suggestion approved through the close sheet counts as accepted: a `close` decision on its item within 14 days after it, with the matching reason (a duplicate naming the suggested issue), as closure truth reads it (3.1); a suggestion counts accepted once, and a close does not count for a suggestion the owner dismissed or edited before it | closure cards are approved by a close, not an accept | gaps |
| 1.4, spec 4.2 | `instructs_agent` true: no card; issue `triage.instructs_agent` {reason, backend, probability}; a warning in every brief naming the item. Details shows the reason, briefs do not | the reason is a model's words about outside text | gaps |
| 1.4 | A support question answered true needs `reply` (refused without it, and on any other answer; at most 10,000 characters); the body stores it and the close sheet opens with it as the comment | a drafted reply the owner edits, well inside a GitHub comment | gaps |
| 4.2 | The `recommend_closures` brief did not carry the instructs_agent warning; it does since "Rulings" below | that brief awaited the owner's ruling (5.1) | gaps |

### 5/5 Docs and skills

| § | As built | Why | PR |
|---|---|---|---|
| 6.2 | The triage skill is `skills/issue-dashboard-triage-session`; `docs/closing-recommendations.md` stayed, because the alias brief compiled it in; removed in "Rulings" below | removing it needed a code change | #33 |

### Rulings and project scoping

Two rulings taken by the supervising session under the owner's "the correct option the user
expects" policy, and the last scoping item. Every row is overrulable.

| § | As built | Why | PR |
|---|---|---|---|
| 4.1 | **Ruling (overrulable):** the estimates are editable in Details. `DecisionValue.estimates` holds any of `sites` + `read_at` (together), `tier` T0-T3, `blast`, `effort` S/M/L; an `edit_suggestion` carrying it may name only its item. Per item the edits merge, a later field over an earlier one, undo taking one back; the issue document's `triage.estimates` carries the recorded fields (null when none were recorded) and `edited`, the merged edit or null. The page shows the edit over the session's estimate | the table says editable; one field per estimate, so an edit is never a whole re-entry | rulings |
| 4.1, 3.1 | An estimates edit records no truth: truth stays where severity, reach and detectability edits are | an estimate is never evaluated (1.1) | rulings |
| 5.1 | **Ruling (overrulable):** the `recommend_closures` alias is on the triage brief with scope `closures`. Its answers go through `prepare` and calibration, its brief carries the instructs_agent warning, and the daily closures run does not ask them again until due (2.2). Each repo's closures are judged by its campaign skill, or by the closing procedure of the `issue-dashboard-triage-session` skill, which also carries the batching (close together, one gate) that the phase-1 procedure had. `docs/closing-recommendations.md` is removed | the user expects one triage, one calibration, one warning rule | rulings |
| 5.1 | The alias still accepts phase-1 suggestion entries in a file (likely fixed, duplicate, obsolete without a question); the brief teaches only answers, estimates, batches and `left_out` | a session started on the old brief can still finish | rulings |
| 2.2 | `triage::asked` reads one project's `suggested` events: `select`, `prepare` (both stages, the stronger look's list) and `record-suggestions`' stage-one check see only that project's answers | two projects tracking one issue must not answer for each other (S1) | rulings |
| 1.4 | `suggest::instructs_agent` is not project-scoped: a warning one project's run recorded reaches another project's briefs | fails safe: a warning more, never one fewer | rulings |
