// node --test page/  — gate (mutation pass, slice 3): guards of core.js that the suite in
// core.test.js did not notice losing. Each test names the mutation it kills.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const D = require("./core.js");

function issue(n, more) {
  return Object.assign(
    {
      id: "I_" + n,
      alias: "yuvalrakavy/Store#" + n,
      kind: "issue",
      state: "open",
      status: "ready",
      outside: false,
      limit_breached: false,
      idle_days: 0,
      rank: null,
      priority: { value: "p2" },
      created_at: "2026-10-01T00:00:00Z",
      closed_at: null,
    },
    more,
  );
}

const who = { project: "VillaRakavy", author: "user_1" };
const rand = () => 0.5;
const jan = new Date("2026-01-05T03:04:05.678Z");

// --- ids and documents -------------------------------------------------------------------

// Kills: month not +1, date parts not zero padded, local time instead of UTC, the random part
// not four characters, "D"/"L"/"V"/"R" prefixes swapped.
test("an id is prefix_yyyymmdd_hhmmss_rand with every part zero padded, in UTC", () => {
  assert.equal(D.newId("D", jan, () => 0), "D_20260105_030405_00000000");
  assert.equal(D.newId("X", new Date("2026-12-31T23:59:59Z"), () => 0), "X_20261231_235959_00000000");
  const r = D.newId("D", jan, () => 0.123456789).split("_")[3];
  assert.equal(r.length, 8);
  assert.match(r, /^[a-z0-9]{8}$/);
  assert.equal(D.newId("D", jan, () => 0.5).slice(-8), D.newId("D", jan, () => 0.5).slice(-8));
  // A small random value is padded, not shortened.
  assert.equal(D.newId("D", jan, () => 1e-9).length, "D_20260105_030405_".length + 8);
});

// Kills: the random part not scaled to all 8 base-36 digits.
test("the random part of an id is the random number in 8 base-36 digits", () => {
  assert.equal(D.newId("D", jan, () => 0.5), "D_20260105_030405_i0000000");
  assert.equal(D.newId("D", jan, () => 1 / 36), "D_20260105_030405_10000000");
});

// Kills: the `exists` filter on the parts listing (a part deleted between the listing and its
// read comes back as a snapshot that does not exist).
test("a listed part that no longer exists is not joined", async () => {
  const text = JSON.stringify({ project: "home", sequence: 4, issues: [{ id: "I_1" }, { id: "I_2" }] });
  const half = Math.ceil(text.length / 2);
  const docs = {
    "meta/active": { generation: "gen-4", sequence: 4 },
    "gens/gen-4": { generation: "gen-4", sequence: 4, parts: 2, length: text.length },
  };
  const live = (index, t) => ({ exists: true, data: () => ({ index, text: t }) });
  const gone = { exists: false, data: () => ({ index: 2, text: "stale" }) };
  const db = {
    doc: (p) => ({ get: async () => ({ exists: p in docs, data: () => docs[p] }) }),
    collection: () => ({
      limit: () => ({ get: async () => ({ docs: [live(1, text.slice(0, half)), gone, live(2, text.slice(half))] }) }),
    }),
  };
  const r = await D.loadGeneration(db);
  assert.equal(r.status, "ok", r.why);
  assert.deepEqual(r.data.issues.map((i) => i.id), ["I_1", "I_2"]);
});

// Kills: the part count check (an extra empty part keeps the joined length, so only the count
// tells this generation is not the one its marker describes).
test("a generation with more parts than its marker says is partial", async () => {
  const text = JSON.stringify({ project: "home", sequence: 4, issues: [] });
  const docs = {
    "meta/active": { generation: "gen-4", sequence: 4 },
    "gens/gen-4": { generation: "gen-4", sequence: 4, parts: 1, length: text.length },
  };
  const part = (index, t) => ({ exists: true, data: () => ({ index, text: t }) });
  const db = {
    doc: (p) => ({ get: async () => ({ exists: p in docs, data: () => docs[p] }) }),
    collection: () => ({ limit: () => ({ get: async () => ({ docs: [part(1, text), part(2, "")] }) }) }),
  };
  const r = await D.loadGeneration(db, { attempts: 1 });
  assert.equal(r.status, "partial");
});

// Kills: waiting_on_you counting a review of an inside pull request.
test("only an outside pull request's review, or a decision, waits on the owner", () => {
  const t = D.tiles([
    issue(1, { status: "needs-review", outside: false }),
    issue(2, { status: "needs-review", outside: true }),
    issue(3, { status: "needs-decision" }),
    issue(4, { status: "needs-review" }),
  ]);
  assert.equal(t.waiting_on_you, 2);
});

// Kills: author/project swapped; created_at not the stamp; optional value/reason always or
// never written; target/action dropped.
test("a decision document carries exactly the fields it was given", () => {
  const f = { action: "set_priority", target: { item: "I_1", alias: "a/b#1" }, value: { priority: "p1" }, reason: "why" };
  assert.deepEqual(D.decisionDoc(f, who, jan, rand), {
    id: "D_20260105_030405_" + D.newId("D", jan, rand).slice(-8),
    project: "VillaRakavy",
    author: "user_1",
    created_at: "2026-01-05T03:04:05Z",
    action: "set_priority",
    target: { item: "I_1", alias: "a/b#1" },
    value: { priority: "p1" },
    reason: "why",
  });
  const bare = D.decisionDoc({ action: "undo", target: { decision: "D_9" } }, who, jan, rand);
  assert.deepEqual(Object.keys(bare).sort(), ["action", "author", "created_at", "id", "project", "target"]);
});

// Kills: any field of a launch document dropped or swapped.
test("a launch document and its not_sent twin carry their fields", () => {
  const f = { kind: "fix", items: ["I_1"], aliases: ["a/b#1"], recipe: "default", routing: { model: "m", effort: "low" } };
  const l = D.launchDoc(f, who, jan, rand);
  assert.match(l.id, /^L_20260105_030405_/);
  assert.deepEqual(
    { ...l, id: null },
    { id: null, project: "VillaRakavy", author: "user_1", created_at: "2026-01-05T03:04:05Z", ...f },
  );
  const n = D.notSentDoc("L_1", "rate_limited", who, jan, rand);
  assert.match(n.id, /^L_20260105_030405_/);
  assert.deepEqual(n.not_sent, { launch: "L_1", error: "rate_limited" });
  assert.deepEqual([n.project, n.author, n.created_at], ["VillaRakavy", "user_1", "2026-01-05T03:04:05Z"]);
  assert.equal(D.notSentDoc("L_1", undefined, who, jan, rand).not_sent.error, "upstream_error");
  assert.equal(D.notSentDoc("L_1", "", who, jan, rand).not_sent.error, "upstream_error");
});

// Kills: view name/include/exclude dropped, defaults of [] lost.
test("a view document keeps its name and scope, an unset side being an empty list", () => {
  const v = D.viewDoc("Without bridges", { exclude: [{ group: "bridges" }] }, who, jan, rand);
  assert.match(v.id, /^V_20260105_030405_/);
  assert.deepEqual([v.name, v.include, v.exclude], ["Without bridges", [], [{ group: "bridges" }]]);
  const w = D.viewDoc("Only Store", { include: [{ repo: "yuvalrakavy/Store" }] }, who, jan, rand);
  assert.deepEqual([w.include, w.exclude], [[{ repo: "yuvalrakavy/Store" }], []]);
  assert.deepEqual([v.project, v.author, v.created_at], ["VillaRakavy", "user_1", "2026-01-05T03:04:05Z"]);
});

// Kills: request items/launch optional fields dropped or always written; kind lost.
test("a request document carries its kind and only the targets it was given", () => {
  const a = D.requestDoc({ kind: "triage", items: ["I_1"] }, who, jan, rand);
  assert.match(a.id, /^R_20260105_030405_/);
  assert.deepEqual([a.kind, a.items, "launch" in a], ["triage", ["I_1"], false]);
  const b = D.requestDoc({ kind: "relaunch", launch: "L_1" }, who, jan, rand);
  assert.deepEqual([b.kind, b.launch, "items" in b], ["relaunch", "L_1", false]);
  assert.deepEqual([a.project, a.author, a.created_at], ["VillaRakavy", "user_1", "2026-01-05T03:04:05Z"]);
});

// --- trends ------------------------------------------------------------------------------

// Kills: bucket edges (from exclusive, to inclusive), the closed_at guard, week order, the
// `from` stamp, the number of weeks, closed counted by created_at.
test("a week bucket is (from, to], oldest first, and an issue counts where it opened or closed", () => {
  const now = new Date("2026-10-23T00:00:00Z");
  const list = [
    issue(1, { created_at: "2026-10-23T00:00:00Z" }), // on `now`: in the last week
    issue(2, { created_at: "2026-10-16T00:00:00Z" }), // on the last week's `from`: in the week before
    issue(3, { created_at: "2026-10-09T00:00:00Z", state: "closed", closed_at: "2026-10-16T00:00:01Z" }),
    issue(4, { created_at: "2026-10-02T00:00:00Z", state: "closed", closed_at: "2026-10-02T00:00:00Z" }), // before every bucket's closed edge
    issue(5, { created_at: "2026-10-22T00:00:00Z", closed_at: null }),
  ];
  const t = D.trends(list, now, 3, {});
  assert.equal(t.weeks.length, 3);
  assert.deepEqual(t.weeks.map((w) => w.from), ["2026-10-02T00:00:00.000Z", "2026-10-09T00:00:00.000Z", "2026-10-16T00:00:00.000Z"]);
  // Week 1 (Oct 02, Oct 09]: opened 2 (Oct 09 is in, Oct 02 is out), closed 0.
  // Week 2 (Oct 09, Oct 16]: opened 1 (I_2), closed 0.
  // Week 3 (Oct 16, Oct 23]: opened 2 (I_1, I_5), closed 1 (I_3).
  assert.deepEqual(t.weeks.map((w) => [w.opened, w.closed]), [[1, 0], [1, 0], [2, 1]]);
});

// Kills: over-limit counting closed issues; N counting non-breached; byRepo/byGroup taking
// closed issues; byGroup ignoring the group's repos; the tiles inside each repo.
test("over-limit is N of the open M overall and per repo and group, closed issues left out", () => {
  const now = new Date("2026-10-23T00:00:00Z");
  const a = (n, more) => issue(n, Object.assign({ alias: "yuvalrakavy/mqtt_ac#" + n }, more));
  const list = [
    a(1, { limit_breached: true }),
    a(2),
    a(3, { limit_breached: true, state: "closed" }),
    issue(4, { limit_breached: true, status: "untriaged" }),
    issue(5),
    issue(6, { alias: "other/repo#6", priority: { value: "p0" } }),
  ];
  const t = D.trends(list, now, 1, { bridges: ["yuvalrakavy/mqtt_ac"], none: [] });
  assert.deepEqual(t.over, { n: 2, of: 5 });
  assert.deepEqual([t.byRepo["yuvalrakavy/mqtt_ac"].n, t.byRepo["yuvalrakavy/mqtt_ac"].of], [1, 2]);
  assert.deepEqual([t.byRepo["yuvalrakavy/Store"].n, t.byRepo["yuvalrakavy/Store"].of], [1, 2]);
  assert.deepEqual([t.byGroup.bridges.n, t.byGroup.bridges.of], [1, 2]);
  assert.deepEqual([t.byGroup.none.n, t.byGroup.none.of], [0, 0]);
  assert.equal(t.byRepo["other/repo"].p0_open, 1);
  assert.equal(t.byRepo["yuvalrakavy/Store"].untriaged, 1);
  assert.equal(t.byGroup.bridges.open, 2);
  assert.deepEqual(Object.keys(t.byRepo).sort(), ["other/repo", "yuvalrakavy/Store", "yuvalrakavy/mqtt_ac"]);
});

// --- launch states -----------------------------------------------------------------------

// Kills: the tracker's state not winning over a not_sent record; the newest launch per item
// not winning; thread / not_sent documents counted as launches; the error text.
test("a launch row takes the tracker's state first, and the newest launch of an item wins", () => {
  const old = { id: "L_1", created_at: "2026-10-23T08:00:00Z", items: ["I_1", "I_2"] };
  const newer = { id: "L_2", created_at: "2026-10-23T09:00:00Z", items: ["I_1"] };
  const sent = { id: "L_3", created_at: "2026-10-23T07:00:00Z", items: ["I_3"] };
  const ns = { id: "L_4", created_at: "2026-10-23T07:30:00Z", not_sent: { launch: "L_3", error: "rate_limited" } };
  const s = D.launchStates([newer, old, sent, ns, { id: "_thread", thread: "t" }], [
    { launch: "L_2", state: "running" },
    { launch: "L_3", state: "finished" },
  ]);
  assert.equal(s.I_1.launch.id, "L_2", "newest wins whatever the order");
  assert.equal(s.I_1.state, "running");
  assert.equal(s.I_2.launch.id, "L_1");
  assert.equal(s.I_2.state, "requested");
  assert.equal(s.I_3.state, "finished", "a tracker state beats a not_sent record");
  assert.equal(s.I_3.error, "rate_limited", "the record's error rides along");
  assert.equal(s.I_2.error, null);
  assert.deepEqual(Object.keys(s).sort(), ["I_1", "I_2", "I_3"]);
  const reversed = D.launchStates([old, newer], []);
  assert.equal(reversed.I_1.launch.id, "L_2");
});

// --- next run ----------------------------------------------------------------------------

// Kills: the default interval, ceil, the one-minute floor, an unparsable time.
test("the next run rounds up, floors at one minute, and defaults to five", () => {
  const m = { taken_at: "2026-10-23T08:00:00Z", sync_interval_minutes: 10 };
  assert.equal(D.nextRunMinutes(m, new Date("2026-10-23T08:02:30Z")), 8, "7.5 left rounds up");
  assert.equal(D.nextRunMinutes(m, new Date("2026-10-23T08:09:59Z")), 1);
  assert.equal(D.nextRunMinutes(m, new Date("2026-10-23T08:10:00Z")), null, "exactly due is overdue");
  assert.equal(D.nextRunMinutes({ taken_at: "2026-10-23T08:00:00Z" }, new Date("2026-10-23T08:00:00Z")), 5);
  assert.equal(D.nextRunMinutes({ taken_at: "2026-10-23T08:00:00Z" }, new Date("2026-10-23T08:05:00Z")), null);
  assert.equal(D.nextRunMinutes({ taken_at: "not a time", sync_interval_minutes: 5 }, new Date()), null);
  assert.equal(D.nextRunMinutes(null, new Date()), null);
});

// --- queue and inbox ordering --------------------------------------------------------------

const ids = (l) => l.map((i) => i.id);
const level = (q, l) => ids(q.levels.find((x) => x.level === l).items);

// Kills: rank not first; null rank not last; idle not descending; alias tie-break; pulls
// sorted differently from the levels; a pull request listed in a level.
test("a level orders by rank, then longest idle, then alias; unranked go last", () => {
  const list = [
    issue(1, { rank: null, idle_days: 50 }),
    issue(2, { rank: 2, idle_days: 0 }),
    issue(3, { rank: 1, idle_days: 0 }),
    issue(4, { rank: 2, idle_days: 5 }),
    issue(5, { rank: 2, idle_days: 5, alias: "a/a#5" }),
    issue(6, { rank: null, idle_days: 50, alias: "a/a#6" }),
    issue(9, { rank: null, idle_days: 9, kind: "pull_request", alias: "z/z#9" }),
    issue(8, { rank: 1, idle_days: 0, kind: "pull_request", alias: "z/z#8" }),
    issue(7, { rank: 0, idle_days: 0, kind: "pull_request", alias: "z/z#7" }),
  ];
  const q = D.queue(list, []);
  assert.deepEqual(level(q, "p2"), ["I_3", "I_5", "I_4", "I_2", "I_6", "I_1"]);
  assert.deepEqual(ids(q.pulls), ["I_7", "I_8", "I_9"]);
  assert.deepEqual(q.levels.map((l) => l.level), ["p0", "p1", "p2", "p3", "none"]);
});

// Kills: the level of an issue with no priority object or a null value; p0..p3 routing.
test("an issue lands in its priority's level, and one without a priority in none", () => {
  const list = [
    issue(1, { priority: { value: "p0" } }),
    issue(2, { priority: { value: "p1" } }),
    issue(3, { priority: { value: "p3" } }),
    issue(4, { priority: { value: null } }),
    issue(5, { priority: null }),
  ];
  const q = D.queue(list, []);
  assert.deepEqual(["p0", "p1", "p2", "p3", "none"].map((l) => level(q, l)), [["I_1"], ["I_2"], [], ["I_3"], ["I_4", "I_5"]]);
});

// Kills: the "new" status missing from the inbox; closed issues in the inbox; oldest-first
// order and its alias tie-break.
test("the inbox takes new issues too, oldest first, ties by alias, closed never", () => {
  const list = [
    issue(1, { status: "new", created_at: "2026-10-02T00:00:00Z", alias: "b/b#1" }),
    issue(2, { status: "needs-decision", created_at: "2026-10-02T00:00:00Z", alias: "a/a#2" }),
    issue(3, { status: "untriaged", created_at: "2026-10-01T00:00:00Z" }),
    issue(4, { status: "untriaged", state: "closed" }),
    issue(5, { status: "needs-review", outside: false }),
    issue(6, { status: "ready" }),
  ];
  const cards = D.inbox(list);
  assert.deepEqual(cards.map((c) => [c.issue.id, c.why]), [["I_3", "untriaged"], ["I_2", "decision"], ["I_1", "untriaged"]]);
});

// --- decisions ---------------------------------------------------------------------------

// Kills: the earliest undo not being the one recorded; the id tie-break at equal time; an
// undo of an unknown decision breaking the read; an undo counting as a pending decision.
test("the latest live decision wins, ties by id, and the earliest undo is the one kept", () => {
  const dec = (id, at, more) => Object.assign({ id, created_at: at, action: "set_priority", target: { item: "I_1" } }, more);
  const same = "2026-10-23T08:00:00Z";
  let s = D.decisionStates([dec("D_1", same), dec("D_2", same)], []);
  assert.equal(s.latestFor("I_1").doc.id, "D_2", "equal times: the greater id");
  s = D.decisionStates([dec("D_2", same), dec("D_1", same)], []);
  assert.equal(s.latestFor("I_1").doc.id, "D_2", "whatever the order given");
  const u1 = { id: "D_8", created_at: "2026-10-23T09:00:00Z", action: "undo", target: { decision: "D_2" } };
  const u2 = { id: "D_7", created_at: "2026-10-23T08:30:00Z", action: "undo", target: { decision: "D_2" } };
  s = D.decisionStates([dec("D_1", same), dec("D_2", same), u1, u2], []);
  assert.equal(s.byId.D_2.undoneBy.id, "D_7", "the earliest undo");
  assert.equal(s.latestFor("I_1").doc.id, "D_1");
  const stray = { id: "D_9", created_at: "2026-10-23T10:00:00Z", action: "undo", target: { decision: "D_404" } };
  s = D.decisionStates([dec("D_1", same), stray], []);
  assert.equal(s.latestFor("I_1").doc.id, "D_1", "an undo is never the pending decision");
  assert.equal(s.latestFor("I_2"), null);
  s = D.decisionStates([dec("D_1", same)], [{ decision: "D_1", result: "applied" }, { decision: "D_x", result: "refused" }]);
  assert.equal(s.latestFor("I_1").ack.result, "applied");
});

// --- the launch thread record -----------------------------------------------------------

function fakeDb(docs) {
  const store = new Map(Object.entries(docs));
  return {
    store,
    doc: (p) => ({
      get: async () => ({ exists: store.has(p), data: () => (store.has(p) ? JSON.parse(JSON.stringify(store.get(p))) : undefined) }),
      set: async (d) => void store.set(p, JSON.parse(JSON.stringify(d))),
    }),
    collection: (c) => {
      const q = { limit: () => q, get: async () => ({ docs: [...store.keys()].filter((k) => k.startsWith(c + "/")).map((k) => ({ data: () => store.get(k) })) }) };
      return q;
    },
  };
}

// Kills: a thread record not written for a new thread, or written for a reply; its time not
// the stamp of when the thread started; the new thread id not recorded.
test("a thread record is written when a thread starts, and only then", async () => {
  const t0 = new Date("2026-10-23T08:00:00Z");
  let tick = 0;
  const deps = (db, comments) => ({
    db,
    comments,
    who,
    now: () => new Date(t0.getTime() + 60000 * tick++),
    rand,
    anchor: async () => ({ kind: "element", id: "x" }),
  });
  const fields = { kind: "fix", items: ["I_1"], aliases: ["a#1"], recipe: "default", routing: {} };
  const comments = (lost) => ({
    canSendToClaude: async () => "available",
    sendToClaude: async (t) => {
      if (t.threadId && t.threadId === lost) throw { code: "not_found" };
      return { threadId: "thr_new" };
    },
  });
  const records = (db) => [...db.store.entries()].filter(([, v]) => typeof v.thread === "string");
  const first = fakeDb({});
  await D.launch(deps(first, comments(null)), fields);
  // now() is read for the launch document (0), then for the thread record (1 minute later).
  assert.deepEqual(records(first), [["launches/T_20261023_080100_i0000000",
    { id: "T_20261023_080100_i0000000", project: "VillaRakavy", author: "user_1", created_at: "2026-10-23T08:01:00Z", thread: "thr_new" }]]);
  tick = 0;
  const replaced = fakeDb({ "launches/_thread": { id: "_thread", thread: "thr_old" } });
  await D.launch(deps(replaced, comments("thr_old")), fields);
  assert.deepEqual(replaced.store.get("launches/_thread"), { id: "_thread", thread: "thr_old" }, "the old record is never rewritten");
  assert.deepEqual(records(replaced).map(([, v]) => v.thread), ["thr_old", "thr_new"]);
  tick = 0;
  const same = fakeDb({ "launches/_thread": { id: "_thread", thread: "thr_new" } });
  await D.launch(deps(same, comments("thr_old")), fields);
  assert.equal(records(same).length, 1, "a reply into the thread writes no record");
});
