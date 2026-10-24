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

## 2026-10-23 phase 1 slice 2: GitHub trait, snapshot, ingest, derive, outbox, cycle (T2)
- time: implement ~40%, fixtures and tests ~30%, gate and fixes ~30%; friction ~1 h.
- catches: self-review=3 (vacuous priority asserts comparing objects to strings; a control that
  stayed green because the fixture dropped the item itself; a hung lease control) gate=14
  user=0 post-merge=0. Biggest: details kept by configured repo name rather than node id; an
  entrant's change lost behind another project's newer watermark; an undo chain reviving a
  withdrawn launch.
- DG=n/a IG=76 post-merge=0 (2 rounds; round 2 found S1 only)
- friction: the mutation finder stalled with no process for ~30 min before the parent noticed
  (UNGUARDED: give the finder a wall-clock budget and a heartbeat file); the shell hook on `!`
  again pushed edits through Python scripts (GUARDED by habit); a patch script failed on
  rustfmt-reflowed SQL (~10 min).
- tier-call: right.
- delegation: mutation finder Sonnet (worktree), 58 pinning tests and no production bug, then
  stalled; depth Codex (codex-rescue), 10 findings, all real, and it returned properly this time;
  round-2 confirming Sonnet medium, 0 S3/S2 and 4 S1.
- unconsolidated: 2
- improve: for any state shared across readers (a watermark, a cache), the test table gets a
  row "two readers commit out of order": three of round 1's S3s were that interleaving.

## 2026-10-23 phase 1 slice 3: the dashboard page, sync skill, Close, session tracking (T2)
- time: implement ~45%, tests and harness ~25%, gates and fixes ~30%; friction ~1 h.
- catches: self-review=3 (three mutation controls that stayed green until lookalikes were added;
  a branch-ref lookalike that could not reach the digit-tail check; an empty owner_id the config
  schema already refuses) gate=Close 2 rounds (10 fix items over 4 commits), sessions 2 rounds
  (round 1: 1 S3 + 3 S2; round 2: 1 S1, filed as #6) user=0 post-merge=0. Biggest: one bad session
  row aborted the whole cycle, page ingest included; after a repo rename no session matched.
- DG=n/a IG=sessions 20 (Close not rescored here: severities were not recorded at its gate)
  post-merge=0
- friction: the shared target dir served a stale build after a worktree agent built from another
  tree (GUARDED: `touch crates/*/src/*.rs` before final runs); the `!` shell hook again (GUARDED by
  habit); creating the tracker root under ~/.claude on a peer-relayed approval was refused as
  persistence: the owner's yes has to be given in the session that acts (UNGUARDED: briefs that
  need persistent setup should say the owner answers there).
- tier-call: right.
- delegation: mutation finder Sonnet (worktree, heartbeat file, 22 min): 51 mutants, 20 pinned by
  18 tests, no production bug; depth Codex: 4 findings, all reproduced RED first; round-2
  confirming Sonnet medium: 0 S3/S2, 1 S1. Controls 22/22 RED on the original mutations.
- unconsolidated: 3
- improve: any ingest of an external list gets a test table row "one element malformed, the
  rest fine": the S3 was a whole-list validation where a per-element one was meant.

## 2026-10-24 phase 1 slice 4: the dispatcher and Recommend closures (T2)
- time: implement ~40%, tests and harness ~30%, gates and fixes ~30%; friction ~1 h (the
  preview launcher stuck at starting; the exclamation-mark shell hook).
- catches: self-review=2 (release refuses inside 15 min, so the routing check moved before
  reserve; reserve has no closed-issue check, so the skill re-reads issues) gate=round 1: 3 S3 +
  4 S2 (Codex), round 2: 2 S1 user=0 post-merge=n/a. Biggest: the page's optimistic dismissal never
  handed back to the tracker, so a lifted suggestion stayed hidden.
- DG=n/a IG=51 post-merge=n/a
- friction: page/controls.py printed RED for every mutant because its temp tree lacked a file one
  test reads (GUARDED: an identity copy must pass first); preview_start hung (UNGUARDED: use a
  background http.server plus Playwright for static pages).
- tier-call: right.
- delegation: mutation finder Sonnet high (90 min); Rust test author Sonnet high (30 min, 36 rows,
  2 production bugs); depth Codex (1 min of model time, 7 findings, all real); confirming Sonnet
  medium (0 S2+).
- unconsolidated: 4
- improve: a negative-control harness always runs an unmutated copy first and must see it GREEN;
  and any page-side optimistic state gets a test-table row "after the tracker consumed it, does
  the page defer?" (the S3 was optimism that never ended).
