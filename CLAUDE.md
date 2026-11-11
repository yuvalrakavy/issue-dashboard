# issue-dashboard

Spec: `docs/` (chapter 10 governs all state). Process: `docs/process/` — after each gated
feature append a micro-retro to `retro-log.md`; its `IG=` line is the gate record, scored as
`gate-kpi.md` defines.

## Before you write

- **Design:** a memo or spec answers every question in `docs/process/hazards.md` (one line each,
  or why it does not apply), and the depth reader's brief carries the file.
- **Negative controls:** derive the list with `python3 scripts/mutant-sites.py <base>`, write
  `MUTANTS.json`, run `scripts/mutate.py` on it, and `mutant-sites.py <base> --check` must pass
  (each site a mutant or a `skip` with its reason). A mutation-finder agent only for substantial
  new logic.
- cargo, git, gh and pushes in this tree run unsandboxed: the sandbox cannot write the worktree
  or the cargo lock. Never run `cargo fmt`.
