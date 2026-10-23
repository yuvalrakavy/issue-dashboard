# Retro log

Template:

```markdown
## YYYY-MM-DD <feature> (T<n>)
- time: <rough split: design / implement / test / review / friction>
- catches: probe=<n> self-review=<n> gate=<n> user=<n> post-merge=<n> — one line on the big ones
- DG=<weighted> IG=<weighted> post-merge=<n>
- friction: <items, with rough cost; mark >15-min items GUARDED/UNGUARDED>
- tier-call: <right / should-have-been-T<n>, why>
- delegation: <what went to which model + outcome, or "none">
- unconsolidated: <N>
- improve: <one candidate improvement, or "none">
```

## 2026-10-23 phase 1: schemas, ledger core, ledger CLI (T2)
- time: implement ~45%, tests ~25%, gate rounds and fixes ~30%; friction small.
- catches: self-review=4 (the 10.3 identity rule cannot make a post-commit retry a no-op; the
  missing withdrawn transition; a missing-ledger wipe suspected before the gate; two fix defects
  before round 2) gate=~35 user=0 post-merge=0. Biggest: a lost ledger made the next command move
  the healthy backup aside; a refused event committed through reserve_next's error swallowing.
- DG=n/a IG=220 post-merge=0 (5 gate rounds; the last three all on rebuild's replace step)
- friction: the shell hook refuses heredocs containing `!` (Rust negations): ~10 min of rerouted
  edits, GUARDED by the hook itself. The Codex rescue agent backgrounded its task and returned at
  once; reading it needed the companion's `status`/`result` (~5 min, UNGUARDED: brief it to wait).
- tier-call: right. The core was T3-like in places (data integrity); failing-test-first was used
  throughout, which the T2 call did not require.
- delegation: mutation finder Sonnet (worktree), 11 real survivors, 10 tests, one asserting a rule
  the fix round then changed; breadth Sonnet x2, accurate, one stray `git checkout` in the main
  tree (restored); depth Codex x3, every finding reproduced by probe.
- unconsolidated: 1
- improve: write the "what state does the recovery path leave?" row into the test table for any
  crash-safety mechanism: every round-1 S3 was a recovery path's own end state.
