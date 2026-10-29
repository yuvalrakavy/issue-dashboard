# issue-dashboard

One dashboard for every GitHub issue and pull request across a set of repositories: a
tracker CLI (`issue-dashboard`) keeps an event ledger of what it read and what was decided,
and a scheduled Claude task carries each cycle's result to a claude.ai artifact page. The
design is `docs/spec.md`; setup notes come with phase 1's release.

## After you change an issue

The dashboard reads GitHub every few minutes (`sync_interval_minutes`). When a session has
just closed, relabelled or landed a fix for an issue the dashboard tracks, ask for the change
to show now instead:

```bash
issue-dashboard --root ~/.claude/issue-dashboard/root sync-now --project VillaRakavy --issue yuvalrakavy/Store#403
```

It records the request and prints the project's sync task (`task`). Run that task now with
the app's scheduled-tasks "run now"; the next run reads the issue in full whatever its last
read saw. Leave out `--issue` to ask for a run of the whole project. Requests coalesce: any
number of them before a run cost one read, and one made while a run is going waits for the
next. An issue the project does not track is refused.

From the page, "Sync now" in the status strip and "Refresh" in an item's `⋯` menu and Details
ask the same; the page cannot wake the task, so the next scheduled run serves them.

A run that finds nothing changed (no issue moved, no decision or launch arrived) does not
republish the page's data: `publish-plan` says `unchanged`, and the run writes only a small
heartbeat, from which the page's "All repos read" time comes. Any real change publishes in full.

## The dispatcher

Launch…, Launch batch…, Run triage…, Evaluate… and Recommend closures… on the page post a launch to whatever Claude
session watches the page. That session is the dispatcher (`skills/issue-dashboard-dispatcher`):
start it on the strongest model at high effort, in auto mode, ask it to follow that skill, and
paste the dashboard's link in your message so it can arm its watch. It reserves a slot, builds
the brief and starts the session; when the app will not let it start one from a page comment,
it shows the session as a card in its own transcript for you to start.

Every session watching the page with auto-replies armed is woken by every launch, so the
dispatcher should be the only one: a session that republishes the page stops its watch
afterwards (`ArtifactComments` `watch` with `on: false`).

Recommend closures runs a read-only session over the issues in view. It judges each repo's
issues by the repo's own campaign skill (`campaign_skill` in the registry, a path in the
repo's `local_path` checkout), or by the generic procedure in `docs/closing-recommendations.md`
when the repo has none, and its recommendations reach the Inbox after the next sync. It is the
closures scope of triage (below), and keeps this brief.

## Triage

A triage session reads the open issues that are due and suggests a type, areas, a priority and
the closures (likely fixed, duplicate, obsolete, support question), each with a probability. A
suggestion changes nothing until you accept it on the page, and nothing is written to GitHub.
Every probability is measured on the project's own history first: a card says whether its model
and question passed that measurement (evaluated), have not been measured, have no correction
map, or have drifted since.

```bash
issue-dashboard --root ~/.claude/issue-dashboard/root triage-request --project VillaRakavy
issue-dashboard --root ~/.claude/issue-dashboard/root eval status --project VillaRakavy
```

`triage-request` records a triage launch for the items that are due, at the routing in the
project config (default Opus 5.5 at medium effort), and prints `requested`, `pending` (an earlier
daily request no dispatcher has taken) or `nothing_to_triage`. It is what the scheduled task
`issue-dashboard-triage` (`skills/issue-dashboard-triage/`) runs each morning before it wakes the
dispatcher. **Run triage…** on the Inbox does the same on demand, with a choice of scope, and
**Evaluate…** on Trends measures a model. `eval status` lists each key's latest evaluation and
drift, and Trends shows each key's last 30 days (accepted of acted on, edited, dismissed). Answers a
run was unsure of are listed under **Needs a stronger look** in the Inbox, whose button launches a
`stronger_look` session at a stronger routing. Triage, stronger-look and eval launches run in the
`read-only` pool (one slot by default) and hold no issue.

The session's method is `skills/issue-dashboard-triage-session`. The design is
`docs/triage-spec.md`, a walk through it from the owner's side is `docs/triage-tutorial.md`, and
what was built differently is the "As built" table at the end of the spec.
