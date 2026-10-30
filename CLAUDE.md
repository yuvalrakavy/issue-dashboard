# issue-dashboard

Spec: `docs/` (chapter 10 governs all state). Process: `docs/process/` — append a micro-retro to
`retro-log.md` and a row to `gate-kpi.md` after each gated feature.

## Before you write

- **Design:** a memo or spec answers every question in `docs/process/hazards.md` (one line each,
  or why it does not apply), and the depth reader's brief carries the file.
- **Negative controls:** `python3 scripts/mutate.py MUTANTS.json` (targeted test command per
  mutant, identity run first, time budget). Write the mutants yourself; an agent only turns
  survivors into tests.

## Harness traps (each cost 10+ minutes more than once)

- Write source with the Edit or Write tool, never from Bash: a heredoc or one-liner that writes
  a file containing `!` is refused (the harness escapes it and would corrupt the file).
- `cmd | filter && next` is refused: split it, or prefix `set -o pipefail;`.
- Temporary files go in the session's scratchpad by absolute path, not `$TMPDIR` (it differs
  between sandboxed and unsandboxed shells).
- cargo, git, gh and pushes in this tree run unsandboxed: the sandbox cannot write the
  worktree or the cargo lock. Never run `cargo fmt`.
- A merged `page/` change is not live until the page is republished: run `/issues-dashboard`
  right after the merge (its step 4 finds a stale page and asks the dispatcher to republish;
  the 06:00 triage task runs the same check daily).
