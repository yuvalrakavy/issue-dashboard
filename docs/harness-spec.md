# Harnesses, phase estimates and model choice — design spec

Status: **approved by the owner 2026-11-02, after three cross-family design passes** (dispositions in
the rationale). T2. Extends `docs/spec.md` §6.3, §6.8, §7.4 and `docs/triage-spec.md` §1.1;
this file is newer, and they are corrected when it lands. Alternatives and reasons:
[harness-spec-rationale.md](harness-spec-rationale.md). Learning arithmetic is a later spec:
[harness-learning-spec.md](harness-learning-spec.md). **Implementation waits for the bridge
runner** (its own T2, designed and landed first).

## Intent

Triage sizes each issue per phase (design, implementation, review). The tracker turns those
sizes into a ranked choice of (harness, model, effort) per phase, with estimated $ and share of
subscription quota, and the owner picks from a dropdown. Every phase's real usage and every
review's verified findings are recorded, so a later spec can calibrate estimates and quality.

**Settled with the owner:** design and implementation share a session unless implementation
is large; review is a separate launch on another family (5.3); cost is $ and quota share;
code ranks, the owner chooses; capability and cost are separate axes (2.2); only
implementation writes (2.5); grades are probed per repo; missing estimates re-triage (1.6).

**Non-goals:** the bridge runner; AgentBridge per-launch worktree writes (handoff 3) — so
**implementation on a bridge harness is unavailable** in this spec (2.5); the learning
arithmetic (later spec); a decision-model triage backend (Jev); WSJF beyond job size.

---

## 1. The per-phase estimate

1.1 **`Estimate.effort` is replaced by `phases`** (`crates/core/src/suggest.rs:96`):
`phases: {design: none|S|M|L, implementation: S|M|L, review: S|M|L}`, each with a one-line
`reason`. `sites`, `read_at`, `tier`, `blast`, `reason` stay.

| Phase | S | M | L |
|---|---|---|---|
| design | a T1 memo | a T2 spec of a few chapters | a T2/T3 spec needing a design gate |
| implementation | ≈15 min (1 pt) | ≈45 min (3 pt) | ≈2 h (8 pt) |
| review | self-review + one reader | depth reader + mutation finder | adds breadth or a second round |

1.2 **The model states sizes only.** Minutes and tokens come from the seed table (3.1).

1.3 **Consistency rules:** design is `none` iff tier is T0; T2/T3 → design ≥ M; T3 → review ≥ M.
One function, `estimate::check`, applies them; it runs in `record`, in the Details edit (1.7)
and on every effective estimate entering an offer (3.2). A failing estimate offers nothing and
shows its reason.

1.4 **Large** = implementation L, or a batch of ≥ 8 implementation points. Large splits
implementation into its own launch (5.3).

1.5 **Batch:** design = largest member's; implementation = **numeric points**, the sum of
members' (1/3/8 each), priced linearly (3.1); review = largest member's, run once over the
combined change; tier = highest member's. **Job size** (spec §6.3) = design + implementation +
review minutes + gate minutes from the recipe's `Recipe.gate` levels (`config.rs:81-94`).

1.6 **Automatic re-triage.** `triage::select_with` (`triage.rs:852`) also marks an open issue
**estimate-due** when its effective estimate (1.7) has no `phases`. Order: untriaged, then
estimate-due, then oldest last triage; the 40-item cap holds. `prepare` writes, per selected
item, an **estimate obligation** keyed by (item, issue `updated_at`, prepare id) into the
questions file; an item selected only for its estimate gets no label questions. `record`
checks obligations of that prepare id only: each is answered with a phased estimate or listed
in `left_out` with a reason, else the file is refused. An answer for an item whose issue
changed after prepare is recorded with `stale: true` and the item stays estimate-due. An item
left out twice in a row is skipped by selection for 7 days. Old estimates are not converted.

1.7 **Effective estimate and edits.** Effective = the latest `estimates` entry for the item
not marked `stale`, with the owner's Details edits merged over it (`docs.rs:195-229`). `EstimatesEdit` drops
`effort` and gains `phases` (each phase optional); an old entry with `effort` and no `phases`
reads as phases-unknown. An edit is refused if the merged result fails `estimate::check`. The
sheet's size controls are edits of this kind, applied to that one launch request only.

## 2. The pool

2.1 **Offering.** Harnesses = `app` + each AgentBridge profile. **App:** models and efforts
from config `pool.app` (the dispatcher's Claude models); its readiness is the dispatcher's
liveness; its identity hash is the hash of the configured dispatcher skill files. **Bridge:**
from AgentBridge `capabilities` and `health` (≤ 30 s old), read by the runner. A malformed or
unknown entry disables that entry only, shown with its parse error. A profile not ready stays
listed with its reason.

2.2 **Two axes per model, owner config (`pool.models`):** `family` and capability `level` 1–4.
Seeds: Fable 5.1 = 4, Opus 5.5 = 3, Sonnet 5.5 = 2, Haiku 5.5 = 1; gpt-6.1-sol and
deepseek-v4.1-flash unleveled. **Cost band** from the $ of a seed M implementation at medium:
cheap < $1, mid < $10, premium ≥ $10; computed, never set.

2.3 **Minimum level** (config `pool.minimum`, seeds): design T2/T3 ≥ 3, T1 ≥ 2; implementation
T3 ≥ 3, T2 ≥ 2, T0/T1 ≥ 1; review T3 ≥ 3, else ≥ 2. Level 4 is never a minimum.

2.4 **Eligibility — one function, `pool::eligible(option, slot) → verdict`**, used by every
list, the sheet, overrides and `reserve`:

| Verdict | When | Effect |
|---|---|---|
| launchable | ready, right grade, level ≥ minimum, family rule met, under 100 % quota | ranked (3.4) |
| overridable | below minimum; unleveled; unpriced; past 100 % quota; a review breaking its family rule | listed after the launchable ones with the reason; picking it asks the owner to confirm, and the override and reason are recorded on the launch |
| forbidden | not ready; missing or expired grade; bridge implementation (2.5); runner absent | listed greyed with reason; `reserve` refuses it |

The **family rule is per slot**: a design review differs from the designer's family; a T3
second design reviewer differs from the designer's and the first reviewer's; code and
confirming review differ from the implementer's. It uses the **observed** family (strict
routing for bridge, the session's model for app); a slot whose judged slot is not yet picked
takes the family of the sheet's current pick for it.

2.5 **Grades, probed per key** (harness, profile, model, repo, executable sha256, profile
config hash, recipe hash, process-skill hash):

| Grade | Phases | Probe |
|---|---|---|
| deliver-enabled | design, review | names the skill and instruction files it loaded; answers two questions generated from those files at the probe commit (the tracker picks two facts and checks the answers literally); returns a small document by transfer whose sha256 the tracker checks |
| write-enabled | implementation | app only, in this spec. Bridge write probes wait for AgentBridge handoff 3 |

The app harness holds both grades by its recipe, without a probe; its key is the dispatcher
skill hash (2.1).

Without the repo's checkout in its folder the probe fails "no checkout". A probe is kind `probe`: read-only, holds no items, takes a read-only slot; its result
is a ledger `grade` record (key, steps passed, evidence, date) or a `grade_failed` record with
the failing step. A grade **expires** when any key part changes, after 30 days, or on the
owner's `revoke`. Demotion is not automatic in this spec.

2.6 **Price table** (`pool.prices`): $/Mtok input, output, cache read per model, with `as_of`;
older than 90 days → "price may be stale". A model with no price is unpriced (2.4).

2.7 **Subscriptions** (`pool.subscriptions`): a stable `id`, the harnesses and models billing
it, a budget in tokens, and a window: `rolling` with a length, or `fixed` with a start
weekday/time and a timezone. The tracker sees only its own launches; the headroom label says
"this tracker's usage".

## 3. Offers and cost

3.1 **Seed table** (labelled "seed" wherever shown; replaced only by the learning spec):

| Phase | S | M | L | per point |
|---|---|---|---|---|
| design tokens / min | 150 k / 10 | 600 k / 40 | 1.5 M / 90 | — |
| implementation tokens / min | 300 k / 15 | 1 M / 45 | 3 M / 120 | 350 k / 15 |
| review tokens / min | 150 k / 10 | 500 k / 30 | 1.2 M / 60 | — |

Token split: 75 % cache read, 15 % input, 10 % output. Effort factor on tokens and minutes:
low 0.6, medium 1, high 1.6, xhigh 2.2, max 3; an effort name outside this list is unpriced.
Batch implementation uses the per-point row × points. $ = Σ tokens × price per kind. Gate
scripts cost minutes, not $.

3.2 **Offers are computed per queued item and per batch suggestion at each publish**, never
on the page. An offer is keyed by (item or batch, effective estimate, repo) and holds, per
plan slot (3.5), a ranked list for each size the owner may pick for that phase; each review
slot has one list per **combination** of the families of the slots constraining it (2.4) —
for a T3 second reviewer, per (designer family, first-reviewer family) pair. Each list is ranked as
if alone; the plan total and `reserve` check the plan as a whole.

3.3 **Two figures per option:** $ (on a subscription harness "≈ $X on subscription") and
quota share = estimated tokens / budget, with **headroom** = budget − consumption in the
current window (3.6).

3.4 **Order within launchable:** lower $ first; ties → more headroom; then level, then a
stable name order. A launch that would pass 80 % of its window drops below the launchable
options under 80 %, labelled. Until the learning spec, $ is the plain seed $.

3.5 **The plan is expanded before pricing** into its slots: design (every non-`none` design; splitting
decides only whether it is a separate launch), one design review slot (T2) or two from two families (T3), implementation, code review, confirming review
(priced, marked conditional). A same-session design + implementation (5.4) is one launch with
two slots. The plan total = Σ slots, per subscription and in $, plus gate minutes.

3.6 **Quota consumption** is a ledger record per (launch, slot, subscription): at `reserve` a
**hold** at each slot's estimate; at that slot's checkpoint (5.4) or at the outcome its hold
alone is **replaced** by the measured figure — a later slot of the same launch keeps its hold (or kept as an estimate when usage is unknown, never 0).
Consumption counts in the window containing its timestamp. `reserve` takes the quota check
and the hold in one transaction, so two concurrent reserves cannot both pass on one headroom.

3.7 **The offer is recorded:** each launch stores the offer it was picked from and the pick.

## 4. Launch sheet and Harnesses view

4.1 **Sheet:** a row per plan slot, each with an editable size (an edit per 1.7) and a
dropdown from the embedded offer; **Split design** toggle when the best deliver-enabled design
option is cheaper than the best write-enabled one, already on for a large implementation;
plan total ($, quota per subscription, gate). Changing a pick switches the lists of the
review slots it constrains. 4.2 `reserve` recomputes the offer: a forbidden pick is refused
with its reason and the sheet reopens; a pick that only moved down, or an overridable pick the
owner confirmed, is allowed.

4.3 **Entry:** `Opus 5.5 · medium · app — ≈$3.10 · 4% Max 5 h · L3`, badges "seed",
"override: below minimum", greyed entries with reason. 4.4 `Routing` (`docs.rs:17`) gains
`harness`; the request stores per-slot routings and the offer. 4.5 Batch: same sheet,
members on top, points per 1.5. 4.6 Phone width: full-width list, two-line entries.

4.7 **Harnesses view** (beside Queue and Inbox): a row per (harness, model) — ready, strict
routing, network read, cancel, archive, deliver-enabled, write-enabled, last probe and expiry,
level, cost band. **Every missing capability shows its reason, its source** (capabilities,
health, probe step and date, config) **and the known fix.** **Refresh** (no model turn);
**Re-probe** (a `probe` launch, cost shown).

## 5. The launch group

5.1 **A plan is a `group`** (ledger kind). The group **holds the items and the workspace**
from its first `reserve` until it ends; `held_item` (`launch.rs:494-508`) treats a holder in
the same group as no conflict. Members: the implementing launch (kind `fix`/`batch`, or
`design` when split) and review launches of new kind `group_review` — read-only, holds no
items, takes a read-only slot, refused outside a group. Ending a member releases its slot, not
the group's items.

5.2 **Identities.** An **attempt** = (group, phase, n). An **artifact revision** is a design
document's sha256, or a code bundle (repo identity, base sha, head sha). A **round** = (attempt,
revision, n) with one or two reviewer slots. Every review input, result, approval and
disposition names these; a result or approval naming a revision that is not the attempt's
current one is refused.

5.3 **Order:** design → design review (T2 one reviewer, T3 two from two families, each a
different family from the designer) → **owner approval** → implementation with its tests and
gate scripts → freeze (bundle) → code review → implementer verifies and fixes → confirming
review of the fix delta → owner merges.

5.4 **Same-session design + implementation (app).** One launch, two attempts. The session
ends its design turn with a `design_ready` stage carrying the document; that is the design
attempt's end and a **usage checkpoint** (tokens and minutes so far, from the session). While
review and approval are pending the launch keeps its slot and is shown **suspended**; that
time is excluded from phase minutes. The dispatcher relays findings and, after approval, the
approved hash into the same session.

5.5 **Bridge launches run through the runner**; without it, bridge options are forbidden
"runner not installed". A bridge design worker returns the document as a transfer; the
tracker stores it. **Approval** is an owner-authored decision (`decision.rs:84`) naming
(group, attempt, document sha256); on approval the tracker writes that document into the
implementation workspace.

5.6 **Review input** is a frozen bundle with a manifest (group, round, repo identity, base,
head; `git diff base..head` plus each touched file at head), sent by transfer. A design review
receives the document plus the files it cites, under the same manifest shape.

5.7 **Review result** (one per reviewer slot): round id, verdict `pass | revise | reject`, and
findings {id, severity S1–S3, `file:line`, claim, evidence}. Findings and their dispositions
are **append-only ledger events**, never edits to an outcome: `finding` and
`finding_disposition` {verified: yes | no | disputed, disposition: fixed | declined | filed}.
A disputed finding goes to the owner and counts in no verdict until decided. A later round's
finding the implementer marks as the same as an earlier one links to its id (`repeat_of`). A
round is **pending** until every slot's result is in and every finding is verified `yes`/`no`
or decided by the owner; only then is its **verdict** final, from verified findings: none ≥ S2
→ pass; S2 → revise; S3 → **failed**. A reviewer's `reject` is a claim like a finding: it
fails the round only when verified or upheld by the owner.

5.8 **Confirming review** covers the fix delta of a revise round; it is skipped only when the
delta touches no code and, for a design, no decision text. A confirming round that finds a
verified S2+ is failed.

5.9 **Redo rule.** A design attempt with a failed round, or a second revise round → design
**rejected** (kept, shown) and a **redo proposed** one level up, cheapest launchable
option preselected, receiving the problem statement and the findings, not the rejected design.
Code: a failed round is fixed and confirmed; a second failed round → propose redesign or
redo one level up, owner chooses. A failed redo
or no higher level → stop and ask. Proposals never start on their own; a failed attempt's
usage and findings stay with the option that produced it.

5.10 **Transitions** (every wake is a ledger event the dispatcher or runner already reads):

| Case | Durable state | Items / workspace | Recovery |
|---|---|---|---|
| member fails or stops | member ended; later slots never started | group keeps them | owner retries the slot (new attempt) or abandons |
| approval pending | group `awaiting_approval`, Inbox row with age | held | owner approves, asks revise, or abandons |
| reviewer unavailable | slot `waiting_reviewer` with reason | held | owner picks another option (override recorded) |
| transfer fails or unknown | member running, transfer id kept | held | runner retries the same transfer id; unknown is reconciled, never relaunched |
| record refused | same launch, returned to its session | held | existing refusal round |
| verifier session dies | findings stay unverified | held | a new implementation attempt resumes verification from the ledger |
| owner abandons | group `stopped`; outcome per member | items released; workspace kept | — |
| merged | group `landed` | items released | — |

## 6. Recording, tests, phasing

6.1 **`record-outcome`** (`outcome.rs`) gains, per attempt: minutes; tokens (input, output,
cache read) from the checkpoints; model, effort, harness; `usage_source` (`native` |
`estimate` | `unknown`, never 0); the override, if any. Findings live in 5.7's events, not
here. Existing refusals stay. The learning spec decides how these feed estimates and quality.

6.2 **Tests** (Rust, `crates/core/tests/`; each seen RED before green):

| Area | Must fail without the feature |
|---|---|
| estimate | each 1.3 rule refused in `record`, in an edit and in an offer; obligation of another prepare id ignored; changed issue → `stale`, still due; twice left out → skipped 7 days |
| pool | malformed entry disables only itself; key-part change expires a grade; bridge implementation forbidden |
| eligibility | each verdict row; `reserve` refuses forbidden, accepts confirmed override and records it |
| offers | two items in different repos/tiers get different lists; batch of two L prices 16 points; T3 plan prices two design reviewers |
| quota | two concurrent reserves on one headroom: one refused; measured usage replaces the hold, never adds; unknown stays at estimate; fixed window rolls at its timezone |
| groups | review member holds no item; group keeps items across a member's end; result for an old revision refused; approval of a stale hash refused; verdict from verified findings only; disputed counts nothing |
| transitions | each 5.10 row, including replayed and out-of-order events (a result before its round's input event) |
| gate-2 cases | design reviewer of the designer's family overridable even when the implementer differs; T3 second reviewer of the first's family overridable; round with a missing slot or unverified S3 stays pending; unverified `reject` fails nothing; `design_ready` checkpoint keeps the implementation hold; stale answer leaves the item estimate-due; T3 second-reviewer list keyed by the family pair; shared-session design priced and held |
| page | size edit switches list; review list follows implementation family; phone-width entries; Harnesses reasons with source |

6.3 **Phasing** (each lands alone, after the runner; each ships its own skill and doc
changes): (1) phased estimate, `estimate::check`, obligations, edits — triage skill;
(2) pool config, app and bridge ingestion, deliver-enabled probe via the runner, Harnesses
view — config reference; (3) usage checkpoints, quota consumption, outcome fields, finding
events; (4) offers, eligibility, sheet **for the implementation slot only** (one routing,
one launch, as today) — `spec.md` §6.3/§7.4; (5) launch groups: the other slots, plan
expansion and per-slot `reserve`, app implementation with cross-family bridge review,
transitions — dispatcher skill (groups, `design_ready`, relaying), `spec.md` §6.8. Before (4),
launches keep today's routing.

6.4 **Hazards** (`docs/process/hazards.md`): end states are 5.10's; freshness is 2.5's keys,
4.2's recompute and 5.2's revisions; one source each — `pool::eligible`, `estimate::check`,
the group for item ownership.

## As built

Phase 1 (`feat/harness-phase1`). Each row is overrulable.

| § | As built | Why |
|---|---|---|
| 1.6 | The obligations live in the `prepared` event (`data.obligations`: item, alias, `updated_at`); "prepare id" = the launch's latest first-stage `prepared` event, the one `record` checks. A prepare with obligations and no question is recorded (`questions` may then be empty) | the event already freezes what `record` checks; no second store |
| 1.6 | An item closed after prepare owes nothing; an obligation is answered only by an `estimates` or `left_out` entry of the same launch | a closed item cannot be estimated; another launch's answer is to its own prepare |
| 1.6 | "Left out twice in a row" = the item's last two answered obligations were both `left_out`; the 7 days run from the second | the obligations give the sequence; "answered" excludes a launch that never recorded |
| 1.6 | "Issue changed" = GitHub's `updated_at` at record ≠ at prepare | the observation already carries it |
| 1.6 | A launch answers each obligation once: a re-prepare of it does not ask an answered item again, and a retry of its record keeps the first answer's staleness, so the retry is an exact repeat | a stale estimate still answers its obligation; the next launch asks again |
| 1.6 | The 7-day skip applies to estimate selection only; the item's label questions are still asked | a left-out says the item cannot be estimated yet, not that its labels are settled |
| 1.7 | Ingest's empty-`phases` refusal is a second line: the decided event's schema refuses `phases: {}` first, so no test can reach it | kept against a schema relaxation |
| 1.7 | Details stages the tier and phase sizes and saves them with one "Save sizes" | T0 ↔ T1 moves the tier and the design size together; either alone fails 1.3 |
| 1.6 | Only a full triage's first stage owes estimates: closures runs, stronger looks and the priority stage never do | estimates need the code read a full triage does |
| 1.7 | `EstimatesEdit.effort` stays as a read-only field; ingest refuses a new edit carrying it, an empty `phases` edit, and one whose merge fails `estimate::check` | old decisions were refused only at their own ingest, so they still load |
| 1.3 | The page mirrors `estimate::check` (`estimateProblems` in `page/core.js`) for immediate feedback; `crates/core/tests/fixtures/estimate-check.json` holds both to one set of cases. The offer-side check waits for phase 4 | two copies, one fixture, so they cannot drift silently |
