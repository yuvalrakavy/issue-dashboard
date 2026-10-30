# Design hazards

Questions a memo or spec answers before it is approved, and the depth reader is handed with
the frozen diff. Each comes from a gate finding in `retro-log.md`; the entry date is beside it.
Three families. Answer a question in the spec, in one line, or say why it does not apply.

## 1. End states: what is left when the path finishes, fails or nobody acts

Most first-round S3s were here.

- A recovery or retry path: what state does it leave, and is that state healthy for the next
  command? (2026-10-23 phase 1: a lost ledger made the next command move the good backup aside.)
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
  (2026-10-24 slice 4; `scripts/mutate.py` enforces it.)
