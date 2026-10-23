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
issue-dashboard --root ~/.claude/issue-dashboard/root sync-now --project villa-rakavy --issue yuvalrakavy/Store#403
```

It records the request and prints the project's sync task (`task`). Run that task now with
the app's scheduled-tasks "run now"; the next run reads the issue in full whatever its last
read saw. Leave out `--issue` to ask for a run of the whole project. Requests coalesce: any
number of them before a run cost one read, and one made while a run is going waits for the
next. An issue the project does not track is refused.

From the page, "Sync now" in the status strip and "Refresh" in an item's `⋯` menu and Details
ask the same; the page cannot wake the task, so the next scheduled run serves them.
