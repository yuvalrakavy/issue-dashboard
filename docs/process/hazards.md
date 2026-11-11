# Design hazards

Questions a memo or spec answers before it is approved, and the depth reader is handed with
the frozen diff. Each comes from a gate finding in `retro-log.md`; the entry date is beside it.
Three families. Answer a question in the spec, in one line, or say why it does not apply.

## 1. End states: what is left when the path finishes, fails or nobody acts

Most first-round S3s were here.

- A recovery or retry path: what state does it leave, and is that state healthy for the next
  command? (2026-10-23 phase 1: a lost ledger made the next command move the good backup aside.)
- A retry or repeat of a write: does it reproduce the IDENTICAL payload? Name each field
  computed from live state at write time (a staleness, a time, a current read) and say where
  the retry reads it from. (2026-11-11 harness: a stale estimate's retry was refused.)
- A state the far side reports: can two different states arrive looking the same?
  (2026-11-03 runner: an idle worker and a dead one were both `unknown`.)
- Every file a path writes: what removes it? (2026-11-04: 3,031 outboxes, 2.1 GB, never pruned.)
- A state that blocks something while it is live ("refused while X is requested"): what ends X
  if nobody acts? (2026-10-29 launch: a request no dispatcher would take stopped triage for good.)
- Optimistic state on the page: once the tracker has consumed it, does the page defer to the
  tracker? (2026-10-24 slice 4: a dismissal that never handed back.)
- An optimisation that skips a write: what did that write guarantee as a side effect (a version
  bump, a sequence, a timestamp)? (2026-10-24 skip-unchanged.)
- A format change: what happens to data already written in the old format when nothing changes
  again? (2026-10-29 packed generations: the skip froze the old format in place.)
- Deploy: a merged `page/` change is not live until the page is republished. (2026-10-30.)

## 2. Freshness and keys: which observation is this about?

- State shared across readers (a watermark, a cache): what if two readers commit out of order?
  (2026-10-23 slice 2.)
- A fact derived from another recorded answer: about which observation was that answer, and is
  it older than the question? (2026-10-29 seam.)
- A stored artifact that is reused (history file, manifest, tag): which request parameters must
  match to reuse it? (2026-10-29 eval: four findings were reuse under a different key.)
- A fold over an ordered log: is each decision kind "the latest one" or "a set"? (2026-10-29 eval.)
- A lookup against an external resolver: does it resolve more than the identity named (a short
  sha, a prefix)? (2026-10-29 seam.)
- Identity: is a thing keyed by its stable id (node id), not a configured or display name?
  (2026-10-23 slice 2.)

## 3. One source, one scenario: tests and checks that cannot drift

- An ingest of an external list: one element malformed, the rest fine. Validate per element.
  (2026-10-23 slice 3.)
- A list and the action that consumes it share ONE eligibility function; the spec names it.
  (2026-10-29 spec gaps.)
- A numbered procedure in a brief: assert the ORDER of each step that consumes an earlier
  step's output, not only that it is present. (2026-10-29 rulings.)
- A test never branches on whether its own scenario happened; it forces the scenario and
  asserts it did. (2026-10-29 spec gaps.)
- A fixture used for "every X" populates every X, and a control that stays green is a finding
  about the fixture. (2026-10-24 skip-unchanged, 2026-10-29 eval.)
- A negative-control harness runs the unmutated copy first and must see it GREEN.
  (2026-10-24 slice 4; `scripts/mutate.py` enforces it.) A mutant restored by hand (`mv`,
  `git stash`) keeps its old mtime and cargo may not rebuild: `touch` the sources after.
  (2026-11-11.)
- Every refusal the spec adds has a test that drives it through ingest, not only a schema
  test. (2026-11-11 harness: four refusals were never reached.)
- A summary tile and the list it opens are one claim: a test opens the tile's filter and
  counts it. (2026-11-04 Triage tab: the tile counted a union, opened a subset.)
