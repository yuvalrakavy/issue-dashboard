# Triage, as you will use it — a tutorial of the design

This walks through the phase 2 triage design ([triage-spec.md](triage-spec.md)) from your side
of the page. **Nothing here is built yet.** The issue numbers and figures are made up, but each
one behaves exactly as the spec says. Section references such as (§4.1) point into the spec.

---

## 1. What triage does for you

Every morning a session reads the open issues that have changed and suggests a type, areas and a
priority. It also suggests which issues look fixed, duplicated, overtaken or like support
questions. It states a probability for each suggestion, and the dashboard checks those
probabilities against your own history before it trusts them. **A suggestion changes nothing until
you act on it.** It never writes to GitHub: that comes later, with the write engine.

---

## 2. A morning, step by step

**07:00. The timer asks.** The scheduled task `issue-dashboard-triage` runs on Haiku, because all
it does is run one command:

```bash
issue-dashboard triage-request --project VillaRakavy
```

That records a request to launch a triage session, authored by `schedule`, at the routing in your
config: Opus 5.5 medium. If a triage session is already running, the request is refused, so two
never overlap (§5.2).

**07:00:30. The dispatcher launches.** It treats the `schedule` request the same way it treats
one of yours, and starts the session at exactly the model and effort the request names. A
scheduled task can't pin its own model; the dispatcher can. That is why the timer only asks.

**The session checks itself.** Its first act is to compare its own model with the routing. If it
somehow started on Sonnet, it stops without answering. Its answers would otherwise be filed under
the wrong model and spoil that model's measurements.

**It prepares the questions.** The CLI picks the items (§2.2):

- open issues never asked about under the current wording of a question;
- issues with something new since they were last asked about (a comment, a commit, a label);
- anything you asked about with **Triage…** in an item's `⋯` menu.

Untriaged issues come first, and there are at most 40 (`max_triage_items`). For each item and
question it writes a **blinded** input: when the question is "what priority is this?", the
current priority label is removed, so the session can't just copy it (§1.1).

**It answers.** For closures it reads the code: whether the commit really fixed the issue. For
labels it uses the issue's text alone. For each question it gives a probability per answer, such
as `bug 0.82 · todo 0.12 · decision 0.04 · docs 0.02`, with a one-line reason.

**It records.** `record-suggestions` refuses any answer to a question that wasn't asked, or whose
issue changed in the meantime. It applies the calibration map for that model and question
(section 6), and stores both the raw and the corrected probability.

**07:20. The next sync shows the cards.**

**Any time: Run triage…** You don't have to wait for 07:00. The **Run triage…** button on the
Inbox opens the launch sheet, where you pick the scope (*full*, or *closures* only) and see the
routing, with a warning if that model hasn't been evaluated. It starts the same run through the
dispatcher, and it is greyed out while a triage run is already going (§5.1). To triage a single
issue, use **Triage…** in that item's `⋯` menu instead.

---

## 3. Reading a card

Issue #512, *"Panel freezes after the broker reconnects"*, filed yesterday with no labels:

> **Set bug · p1** — *evaluated*
> "Freeze after reconnect; every panel on the floor; no workaround short of a restart."
> Rubric: p1 ✓
> [Accept] [Edit…] [Dismiss]

- **One card for both axes.** A brand-new issue gets type and priority together. If only one is
  missing, the card has only that one (§4.1).
- **The badge** tells you how far to trust it:

  | Badge | Means |
  |---|---|
  | *evaluated* | this model, on this exact question, passed the measurement on your history |
  | *unevaluated* | not measured yet, or it failed |
  | *uncalibrated* | no correction map yet; the probabilities are the model's own |
  | *drifting* | it passed once, but recent accuracy has fallen; re-measure |

- **The rubric note.** Priority is asked after severity, reach and detectability, with spec
  §6.7's rubric as input. If the suggested priority is more than one level from what the rubric
  gives, the card says so ("Rubric says p2"). You see the disagreement instead of it being
  averaged away.
- **Details** (open the item) holds severity (*hang_or_data_loss*), reach (*most*),
  detectability (*visible*) and the estimates: the code sites at a named commit, plus tier, blast
  and effort. These have no cards. They feed the later ordering step, and you can edit them.

---

## 4. Acting on a card

| You do | What happens (§4.2) |
|---|---|
| **Accept** | The item's type becomes *bug*, marked as coming from a suggestion. Priority p1 has the same effect as setting it yourself. The item shows "not written to GitHub yet" |
| **Edit…** → p2 | The same as accepting, with your value. The dashboard records that it suggested p1 and you chose p2, which counts toward measuring it |
| **Dismiss** | The card goes away until something new happens on the issue |
| **Undo** | The usual undo |

What wins when values conflict: **your override > an accepted suggestion > the GitHub label.**

---

## 5. When you've already labelled something

Issue #447 is labelled **p3**. Today's triage thinks it's p1, with the calibrated probability of
p3 at 0.08. That's below 0.2, a confident disagreement, so you get a **second opinion**:

> **Second opinion: p1** (currently p3) — *evaluated*
> "Data loss on an interrupted save; it was filed before the save path was shared."

- At most **5** of these a run, priority before type before area, the strongest disagreements
  first.
- When the model **agrees** with your label you see nothing. The agreement is recorded quietly.
- The card also shows how often this kind of second opinion turns out wrong (the eval's
  *false-alarm rate*), so you can tell how much weight to give it.
- **Dismissing** a second opinion means "my label is right". It's recorded as your confirmation,
  and that also counts toward measuring the model.

---

## 6. How the dashboard learns to trust it

### Day one: everything starts cold

No model has been measured yet, so every card says *unevaluated* (and *uncalibrated*), and
**Apply all** doesn't appear (§3.5).

### Evaluating

Go to **Trends → Evaluate…** and pick a question, say `priority`. Or run:

```bash
issue-dashboard eval prepare priority --backend claude/opus-5-5@medium
```

1. **History.** `eval refresh-history` reads closed issues from the last two years, with every
   label change and when it happened. This is separate from the page's 30-day window.
2. **Truth.** For each issue, the answer is its latest label or your decision. Each one keeps
   where it came from: set at filing, changed later on GitHub, part of a tagged review, or
   accepted or edited by you on the page (§3.1).
3. **Split.** Each issue's id decides, permanently, whether it is in the 40% used to fit the
   correction or the 60% held out to test it. No issue is ever in both.
4. **Asking.** An `eval` session (same model) answers up to 120 held-out issues from their text
   alone, exactly as the morning run would.
5. **Scoring.** For example:

   ```text
   priority · claude/opus-5-5@medium · question a3f9c21e0b4d
   n=114  accuracy 81%  ECE 0.07 (90%: 0.04–0.11)  T=1.35
   by source: filed 84% · github_later 74% · owner_edit 88%
   bars: Evaluated ✓  Bulk ✗ (needs 85%)
   ```

   - **ECE** answers "when it says 80%, is it right about 80% of the time?" Lower is better.
   - **T** is the correction: here the model was a little overconfident, and every later
     priority suggestion is softened by it.

| Bar | Needs | Unlocks |
|---|---|---|
| **Evaluated** | at least 100 held-out examples and ECE ≤ 0.10 | the *evaluated* badge |
| **Bulk** | Evaluated, plus at least 85% accuracy | **Apply all** for that question |

### The badge belongs to a model *and* a question

Reword the area question in `decisions/area.toml` and only area goes back to *unevaluated*.
Switch triage to another model and every card under it is *unevaluated* until you evaluate it.
The launch sheet warns "this routing has no evaluation" (§5.3).

### Drift

If the last 30 decisions you made on a question show accuracy more than 10 points below the
evaluated figure, its cards say *drifting* and Apply all stops for it until a new evaluation
passes.

### Trends

Per question: "priority: 41 of 44 accepted in the last 30 days, 2 edited, 1 dismissed",
alongside the evaluated accuracy and the drift flag (§4.4).

---

## 7. Apply all

When **type** passes the Bulk bar and isn't drifting, an **Apply all (12)** button appears over
the type cards for untriaged issues. It records one accept per card, so each one can be undone on
its own. A bulk accept is never used as truth for measuring the model, because
you didn't read those cards one by one. It never includes second opinions or closures (§4.3).

---

## 8. Closures

Unchanged from phase 1: "Check and close…", "Close as duplicate…", "Close as not planned…".
These are now part of the same triage session (scope `closures`), and the old *Recommend
closures* button still works. Your closes become truth for measuring them. A dismissal **with** a
reason ("not fixed") counts as *no*. A dismissal without one counts as nothing, because it might
just mean "not now".

A support question from someone outside opens your close sheet with a drafted reply. An outside
comment that tries to instruct an agent gets a warning on the item and in every brief.

---

## 9. "Needs a stronger look"

If a priority, type, duplicate or likely-fixed suggestion comes out below 60% after correction,
the Inbox lists it under **needs a stronger look**. Its button launches the strongest model at
high effort, on just those questions. The new answer replaces the card (§2.5).

---

## 10. Tagging a review you did in bulk

On 22–23 October you re-prioritised about 13 Store issues in one sitting. Those labels may make
the model look better or worse than it is, so you can tag them:

```bash
issue-dashboard truth tag --name oct-review --decision priority \
  --from 2026-10-22T00:00Z --to 2026-10-24T00:00Z
```

That lists the 13 label changes and records nothing. Add `--yes` to tag them. From then on,
`eval … --exclude bulk_review:oct-review` measures without them. To make that the default, add
it to `eval_exclude` in the project config.

---

## 11. What is not here yet

- **Ordering (WSJF).** It will read severity, reach, detectability and the estimates from each
  issue's `triage` record.
- **Writing labels to GitHub.** Accepted decisions wait, marked "not written to GitHub yet",
  until the write engine consumes them.
- **Replies to support questions** are phase 3.
