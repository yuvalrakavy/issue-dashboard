# Triage, as you will use it

A walk through triage from your side of the page: run it, read the cards, accept, edit or
dismiss, measure a model, read the bars, and notice drift. The design is
[triage-spec.md](triage-spec.md); section numbers such as (§4.1) point into it. The commands are
the real ones (`issue-dashboard … --help` lists them); the issue numbers on the example cards are
made up, but the evaluation figures in section 5 are the first real ones, measured on
VillaRakavy's history.

Throughout, `--root` is your tracker root; leave it out when `ISSUE_DASHBOARD_ROOT` is set.

---

## 1. What triage does for you

A session reads the open issues that are due and suggests a type, areas and a priority. It also
suggests which issues look fixed, duplicated, overtaken or like support questions. It states a
probability for each suggestion, and the dashboard checks those probabilities against your own
history before it calls them trustworthy. **A suggestion changes nothing until you act on it, and
nothing is written to GitHub**: an accepted type or area shows on the item as "not written to GitHub
yet" until the write engine exists.

---

## 2. Running a triage

**On demand.** In the Inbox, **Run triage…** (yours alone; greyed out while a triage launch is
live) opens the launch sheet:

- *Scope*: **full** asks every decision; **closures** asks only the closure decisions (likely
  fixed, duplicate, obsolete, support question). The old **Recommend closures…** button is the same
  as scope *closures*.
- *Model and effort*: the configured routing, Opus 5.5 at medium unless the project config says
  otherwise (`[routing.triage]`). Change it and you are choosing a different *backend*: its cards
  start unevaluated, and the sheet warns "This routing has no evaluation" with an **Evaluate…**
  button.
- A line saying how many open issues in view are due.

**Every morning.** The scheduled task `issue-dashboard-triage` (Haiku, low effort, 06:00;
`skills/issue-dashboard-triage/scheduled-task.md`) runs

```bash
issue-dashboard triage-request --project VillaRakavy
```

which prints one of three results:

| Result | Meaning |
|---|---|
| `requested` | a triage launch request was recorded, authored `schedule`, naming the items due |
| `pending` | an earlier daily request has not been taken by a dispatcher yet; nothing new is recorded, and the task wakes the dispatcher for it again |
| `nothing_to_triage` | no item has a question due; nothing is recorded |

It is refused while a triage launch is live. The task never answers a question: a scheduled task
cannot pin its model, so it only records the request and messages the dispatcher session, which
reserves it and starts the session at the routing. (The task is defined but not registered; you
register it from that file.)

**What the session does.** It runs `prepare --session-model`, which refuses to continue unless the
session really runs the launch's routing, so its answers are never filed under the wrong model. It
gets blinded questions: when the question is "what priority?", the issue's current labels are not in
the input. It checks closure candidates against the code, answers the label questions from the issue
text alone, records probabilities with a one-line reason each, and then does priority as a second
stage from its own severity, reach and detectability answers. The next sync shows the cards.

**Which issues.** Open issues never asked under the current wording of a question, issues with
something new since they were asked, and issues you asked about with **Request triage** in the item's
`⋯` menu. Untriaged first, then the oldest triage; at most 40 per run (`max_triage_items`).

---

## 3. Reading a card

Issue #512, *"Panel freezes after the broker reconnects"*, filed yesterday with no labels. It is in
the Inbox under **Triage suggestions**:

> **Suggested · type · priority · 82% · evaluated**
> The rubric gives p3.
> "Freeze after reconnect; every panel on the floor; no workaround short of a restart."
> [Set bug · p1] [Edit…] [Dismiss]

- **One card for both** when an issue lacks type and priority; a single card when only one is
  missing (§4.1).
- **The percentage** is the calibrated confidence in the answer, after the correction described in
  section 5.
- **The badge** says how far to trust it:

  | Badge | Means |
  |---|---|
  | *evaluated* | this backend, on this exact question, passed the Evaluated bar (section 5) |
  | *not yet evaluated* | not measured yet, or it failed the bar |
  | *uncalibrated* | no correction map for it; the probabilities are the model's own |
  | *drifting* | it passed once, but your recent decisions agree less (section 6) |

- **The rubric note** ("The rubric gives p3.") appears when the suggested priority is more than one
  level from what the rubric table gives for the session's severity and reach answers. You see the
  disagreement instead of it being averaged away.
- **"The reporter's own request"** marks a suggestion that repeats what the issue's author asked for.
- **Details** (open the item) holds severity, reach and detectability, each editable by you (a
  dash until something has answered), and the estimates (the code sites at the commit read, tier, blast, effort),
  which are shown but have no editor and no card.

### Second opinions

Issue #447 is labelled **p3**. The session thinks p1, and after correction the probability of p3 is
0.08. At 0.20 or below this is a confident disagreement, so you get:

> **Second opinion · priority · 88% · evaluated**
> Second opinion: p3 is 8% likely · 13% false alarms
> [Change to p1] [Edit…] [Dismiss]

A run shows at most 5, priority before type before area, strongest disagreement first. The card also
shows how often this kind of second opinion has been wrong on held-out history (the *false-alarm
rate*). When the session agrees with your label you see nothing; the agreement is recorded.
**Dismissing** a second opinion means "my label is right" and counts as your confirmation.

---

## 4. Accept, edit or dismiss

| You do | What happens (§4.2) |
|---|---|
| **Accept** (the card's main button) | Type and areas become the item's value, marked as coming from a suggestion; a priority has the same effect as setting it yourself. Toast with **Undo** |
| **Edit…** | Pick or type your value and save: the same as accepting, with your value. The dashboard keeps that the suggestion said p1 and you chose p2, which counts toward measuring it |
| **Dismiss** | The card goes away until something new happens on the issue |
| **Undo** | The usual undo |

What wins when values conflict: **your override > an accepted suggestion > the GitHub label.** An
accept, edit or dismissal is refused when the item changed since the suggestion was made; the card
comes back and you look again.

**Apply all · n.** A button over the cards appears only when every card it would take is for an
untriaged item, on a question that passes the **Bulk** bar and is not drifting. It records one accept
per card, each undoable. A bulk accept is never used as truth for measuring the model, because you
did not read those cards one by one. It never takes second opinions or closure cards.

**Closures** keep their phase-1 cards (*Check and close…*, *Close as duplicate…*, *Close as not
planned…*). Your close, or a dismissal **with** a reason, becomes truth for measuring them; a
dismissal without a reason counts as nothing, since it might only mean "not now". A support question
from an outside author opens your close sheet prefilled as not planned, with the reply the session
drafted for the reporter as the comment: edit it or keep it; nothing is posted until you close.

**Needs a stronger look.** An answer on type, priority, duplicate or likely fixed whose top
probability (after calibration) is below 0.6 is marked as unsure (`stronger_look_below` in the
project config sets the bar). The Inbox lists those issues under **Needs a stronger look · n**, each
with the unsure answers and their probability, and Details shows a *Stronger look* row. **Take a
stronger look…** opens the launch sheet for an ordinary `stronger_look` launch over them, at the
stronger routing (Fable 5.1 at high effort unless `[routing.stronger_look]` says otherwise). That
session answers only the marked questions, in one pass, under its own backend, and its answers
replace the earlier ones on the cards after the next sync. An issue leaves the list when you
accept, edit or dismiss its answer, when it closes, or when the stronger look answers. Its own
answers are never marked again.

**An issue that tries to instruct an agent.** When triage judges that an issue's text tries to
direct an AI agent (the `instructs_agent` question, asked of outside authors), there is no card.
Details shows a red *Warning* row with the judgement's reason, and every brief that names the issue
(a fix, a batch, a triage run, a stronger look) carries a warning telling the session to read it as
data only and to report what it asked for. A later triage answer of no removes both.

---

## 5. Measuring a model: eval

Every card's probability is a model's claim. An *eval* checks the claim against history and builds
the correction.

**Start it** from **Trends → Evaluate…** (or from the "no evaluation" warning on the launch sheet);
the dispatcher starts a read-only session at the backend you are measuring. That session runs, per
label decision (type, area, priority), the commands you can also run yourself:

```bash
issue-dashboard eval refresh-history --project VillaRakavy
issue-dashboard eval prepare --project VillaRakavy --decision type \
  --backend claude-opus-5-5@medium --out eval-type
issue-dashboard eval score --project VillaRakavy --decision type \
  --backend claude-opus-5-5@medium --answers <the tuning and held-out answers files>
issue-dashboard eval status --project VillaRakavy
```

1. **History.** `refresh-history` reads closed issues back two years (`eval_history_days`) with
   every label change and its time. It is resumable. It is separate from the page's 30-day window.
2. **Truth.** For each issue the truth is its latest label, or your decision on the page. Each truth
   keeps where it came from: set at filing, changed later on GitHub, part of a tagged review (section
   7), or accepted or edited by you.
3. **Split.** An issue's id decides, permanently, whether it is in the 40 % used to fit the correction
   or the 60 % held out to test it. No issue is in both.
4. **Asking.** The session answers up to 120 tuning and 120 held-out issues from their text alone,
   exactly as the morning run would.
5. **Scoring.** `eval score` fits the correction on the tuning set, freezes it, and scores the
   held-out set.

### The numbers

Accuracy is how often the top answer equals the truth. **ECE** answers "when it says 80 %, is it right
about 80 % of the time?": the gap between stated confidence and actual accuracy, averaged over ten
confidence bins (lower is better), with a 5th to 95th percentile interval from resampling. **T** is
the correction: above 1 the model was overconfident and later suggestions are softened, below 1 it was
underconfident and they are sharpened. The correction is a map with its own id; a suggestion keeps the
map it was prepared under.

### The bars

| Bar | Needs | Unlocks |
|---|---|---|
| **Evaluated** | at least **100** held-out examples and ECE ≤ 0.10 | the *evaluated* badge |
| **Bulk** | Evaluated, plus accuracy ≥ 85 % | **Apply all** for that question |

The bar uses the point ECE, not the interval's upper end.

### A worked example: VillaRakavy, 2026-10

All three used `claude-opus-5-5@medium`:

| Question | n held out | Accuracy | ECE [interval] | T | Evaluated | Bulk |
|---|---|---|---|---|---|---|
| type | 120 | 88.3 % | 0.061 [0.043, 0.114] | 0.55 | yes | yes |
| priority | 120 | 50.8 % | 0.052 [0.034, 0.145] | 0.75 | yes | no |
| area (Store) | 120 | 60.0 % | 0.015 [0.012, 0.026] | 0.90 | yes | no |
| area (logmon-mcp) | 13 | 76.9 % | 0.062 [0.025, 0.133] | 0.45 | no: too few examples | no |

- **type passes both.** 88 % accurate and its stated confidence is honest, so *Apply all* can take its
  cards. A T of 0.55 says the raw answers were underconfident; the map sharpens them. Its interval's
  upper end (0.114) is above 0.10, but the bar takes the point value (0.061).
- **priority is trustworthy but not accurate.** Evaluated yes, Bulk no: when it says 60 % it is right
  about 60 % of the time, and it is right about half the time overall. Read each priority card; a
  priority is never applied in bulk. Its worst bins are p1 and p2 (classwise ECE 0.158 and 0.151).
- **area in Store passes Evaluated at 60 % accuracy**, so its badge reads *evaluated*, while it cannot
  be bulk-applied. **logmon-mcp has 13 examples**: too thin, so it stays *not yet evaluated*.
- Every truth here came from labels. There were no owner truths yet, so the eval reports no accuracy
  by your own decisions; as you accept and edit, it will.

An eval's result is also keyed to the question's wording: reword `decisions/area.toml` and only area
goes back to *not yet evaluated*. Switch triage to another model and every card under it is
unevaluated until you evaluate that model.

---

## 6. Drift

For each question the dashboard follows the last 30 cards you read (accepted or edited, never bulk
accepts) on that backend: the share where your final answer equals the suggestion's top answer. When
that falls more than **10 points** below the evaluated accuracy, its cards say *drifting* and **Apply
all** stops for it until a new eval passes. Fewer than 30 records, or no passing eval, means no drift
flag. `eval status` prints each key's latest evaluation and its drift.

Drift means the world the model was measured on has moved: new kinds of issue, a changed convention,
or a model update. The remedy is to run the eval again.

**Trends** has a table, *Triage suggestions · last 30 days*, with one row per question and backend:
how many of the suggestions you acted on you accepted ("type: 41 of 44"), how many you edited or
dismissed, how many accepts were *Apply all*, the evaluated accuracy and the drift flag. A row
appears once you act on one of its suggestions or the backend is evaluated on it.

---

## 7. Tagging a review you did in bulk

On 22 and 23 October you re-prioritised about 13 Store issues in one sitting. Those labels may make a
model look better or worse than it is, so tag them:

```bash
issue-dashboard truth tag --project VillaRakavy --name oct-review --decision priority \
  --from 2026-10-22T00:00:00Z --to 2026-10-24T00:00:00Z
```

That lists the label changes and records nothing. Add `--yes` to tag them. Then
`eval prepare … --exclude bulk_review:oct-review` measures without them. To make that the default, put
it in `eval_exclude` in the project config.

---

## 8. What is not built yet

- **Ordering (WSJF)** and **writing labels to GitHub**: later phases. Accepted decisions wait, marked
  "not written to GitHub yet".
- **Estimates are display-only** in Details, and **Recommend closures** keeps its phase-1 brief: both
  wait on your ruling (see "As built" in the spec).
