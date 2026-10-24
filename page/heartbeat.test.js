// node --test page/  — the status strip's sync time (5.5, 10.6): a run that found nothing
// changed writes no generation, only `meta/heartbeat`, so the page shows the newer of the
// heartbeat and the active generation.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const D = require("./core.js");

const meta = {
  generation: "gen-40",
  sequence: 40,
  taken_at: "2026-10-23T08:00:00Z",
  sync_interval_minutes: 5,
  repos: [
    { repo: "o/a", read_at: "2026-10-23T08:00:00Z", read: "incremental", error: null, stale: false },
    { repo: "o/b", read_at: null, read: "full", error: "rate limited", stale: true },
  ],
};
const beat = (over) => Object.assign({
  schema: 1,
  project: "home",
  generation: "gen-40",
  sequence: 44,
  synced_at: "2026-10-23T08:20:00Z",
  repos: [{ repo: "o/a", read_at: "2026-10-23T08:20:00Z", read: "full" }],
}, over);

test("a heartbeat of the active generation moves the sync time and each repo's read", () => {
  const s = D.synced(meta, beat());
  assert.equal(s.at, "2026-10-23T08:20:00Z");
  assert.deepEqual(s.repos.map((r) => [r.repo, r.read_at, r.read]), [
    ["o/a", "2026-10-23T08:20:00Z", "full"],
    ["o/b", null, "full"],
  ]);
  // The generation's own fields stay: stale and error are part of what did not change.
  assert.equal(s.repos[1].error, "rate limited");
  assert.equal(s.repos[1].stale, true);
  assert.equal(meta.repos[0].read_at, "2026-10-23T08:00:00Z", "meta itself is not changed");
});

test("without a heartbeat, or with one for another generation or older, the generation's time stands", () => {
  for (const [what, hb] of [
    ["none", null],
    ["another generation", beat({ generation: "gen-39" })],
    ["older than the generation", beat({ synced_at: "2026-10-23T07:55:00Z" })],
    ["not a time", beat({ synced_at: "soon" })],
  ]) {
    const s = D.synced(meta, hb);
    assert.equal(s.at, "2026-10-23T08:00:00Z", what);
    assert.deepEqual(s.repos, meta.repos, what);
  }
  assert.deepEqual(D.synced(null, beat()), { at: null, repos: [] });
});

test("a heartbeat that is not a time never counts, even when the generation has no time", () => {
  const m = Object.assign({}, meta, { taken_at: null });
  assert.equal(D.synced(m, beat({ synced_at: "soon" })).at, null);
  assert.deepEqual(D.synced(m, beat({ synced_at: "soon" })).repos, meta.repos);
});

test("a heartbeat at the generation's own time is not newer: the generation's reads stand", () => {
  const s = D.synced(meta, beat({ synced_at: meta.taken_at }));
  assert.deepEqual(s.repos, meta.repos);
});

test("the next run is counted from the newer sync time", () => {
  const now = new Date("2026-10-23T08:21:00Z");
  assert.equal(D.nextRunMinutes(meta, now), null, "from the generation alone the run is overdue");
  assert.equal(D.nextRunMinutes(meta, now, beat()), 4);
  assert.equal(D.nextRunMinutes(meta, now, beat({ generation: "gen-39" })), null);
});

test("the heartbeat example is what the synced line reads", () => {
  const ex = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "schemas", "examples", "doc-heartbeat", "heartbeat.json"), "utf8"));
  const m = Object.assign({}, meta, { generation: ex.generation, taken_at: "2026-10-23T08:00:00Z" });
  assert.equal(D.synced(m, ex).at, ex.synced_at);
});
