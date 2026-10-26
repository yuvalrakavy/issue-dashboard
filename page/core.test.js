// node --test page/  — the page's logic against a fake db and comments.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const D = require("./core.js");

// A db holding documents by path; `onList(path)` runs before each collection read, so a test
// can change the store mid-load the way a sync run's switch and cleanup would.
function fakeDb(docs, onList) {
  const store = new Map(Object.entries(docs));
  const lists = [];
  const snap = (p) => ({
    id: p.split("/").pop(),
    exists: store.has(p),
    data: () => (store.has(p) ? JSON.parse(JSON.stringify(store.get(p))) : undefined),
  });
  const db = {
    store,
    lists,
    doc: (p) => ({
      get: async () => snap(p),
      set: async (data) => {
        store.set(p, JSON.parse(JSON.stringify(data)));
      },
    }),
    collection: (c) => {
      const q = (n) => ({
        limit: (m) => q(m),
        get: async () => {
          lists.push(c);
          if (onList) onList(c, store);
          const ids = [...store.keys()].filter((k) => k.startsWith(c + "/") && !k.slice(c.length + 1).includes("/")).sort();
          const docs = ids.slice(0, n).map(snap);
          return { docs, size: docs.length, empty: docs.length === 0 };
        },
      });
      return q(1000);
    },
  };
  return db;
}

// A packed generation (publish.rs): the outbox's JSON text cut into `parts` documents
// `{index, text}` under `gens/<g>/parts/`, and a marker naming how many and the text's length.
function generation(seq, issues, extra, parts) {
  const g = "gen-" + seq;
  const outbox = Object.assign({ project: "home", sequence: seq, meta: meta(seq), issues, acks: [], launch_status: [] }, extra);
  const text = JSON.stringify(outbox);
  const n = parts || 3;
  const size = Math.ceil(text.length / n);
  const docs = {};
  for (let k = 0; k < n; k++) {
    docs["gens/" + g + "/parts/p" + String(k + 1).padStart(4, "0")] = { index: k + 1, text: text.slice(k * size, (k + 1) * size) };
  }
  docs["gens/" + g] = { generation: g, sequence: seq, parts: n, length: text.length };
  return docs;
}

const meta = (seq, more) => Object.assign({ generation: "gen-" + seq, sequence: seq, taken_at: "2026-10-23T08:00:00Z" }, more);

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
      priority: { value: "p2", source: "label", label: "p2", project: null, override: null },
      created_at: "2026-10-0" + ((n % 9) + 1) + "T00:00:00Z",
      closed_at: null,
    },
    more,
  );
}

test("a generation loads whole from the meta, the marker and its joined parts", async () => {
  const acks = [{ id: "D_1", cycle: 4 }];
  const db = fakeDb(Object.assign({ "meta/active": meta(4) }, generation(4, [issue(1), issue(2)], { acks })));
  const r = await D.loadGeneration(db);
  assert.equal(r.status, "ok");
  assert.equal(r.generation, "gen-4");
  assert.deepEqual(r.data.issues, [issue(1), issue(2)]);
  assert.deepEqual(r.data.acks, acks);
  // A collection the outbox does not carry loads as empty.
  assert.deepEqual(r.data.metrics, []);
  assert.deepEqual(db.lists, ["gens/gen-4/parts"], "one listing reads the whole generation");
});

test("parts are joined in index order, whatever order the listing returns", async () => {
  const docs = Object.assign({ "meta/active": meta(4) }, generation(4, [issue(1), issue(2)]));
  // Swap the first and last parts' ids: the listing (sorted by id) now returns them reversed.
  const a = docs["gens/gen-4/parts/p0001"];
  docs["gens/gen-4/parts/p0001"] = docs["gens/gen-4/parts/p0003"];
  docs["gens/gen-4/parts/p0003"] = a;
  const r = await D.loadGeneration(fakeDb(docs));
  assert.equal(r.status, "ok", r.why);
  assert.deepEqual(r.data.issues.map((i) => i.id), ["I_1", "I_2"]);
});

test("a joined text of another length than the marker's, or one that does not parse, is partial", async () => {
  const long = Object.assign({ "meta/active": meta(4) }, generation(4, [issue(1)]));
  long["gens/gen-4"].length += 1;
  const r = await D.loadGeneration(fakeDb(long), { attempts: 2 });
  assert.equal(r.status, "partial");
  assert.equal(r.attempts, 2, "retried: a switch may be under way");
  assert.match(r.why, /gen-4 is \d+ of \d+ characters/);
  assert.equal(r.data, undefined, "a partial load leaves what the page shows");
  const bad = Object.assign({ "meta/active": meta(4) }, generation(4, [issue(1)]));
  const p = bad["gens/gen-4/parts/p0002"];
  p.text = p.text.slice(0, -1) + "\u0000";
  const b = await D.loadGeneration(fakeDb(bad), { attempts: 1 });
  assert.equal(b.status, "partial");
  assert.match(b.why, /gen-4 does not parse/);
});

test("a marker without parts is a generation this page cannot read, and is not retried", async () => {
  const docs = { "meta/active": meta(4), "gens/gen-4": { generation: "gen-4", sequence: 4, counts: { issues: 1 } } };
  const db = fakeDb(docs);
  const r = await D.loadGeneration(db, { attempts: 3 });
  assert.equal(r.status, "partial");
  assert.equal(r.attempts, 1);
  assert.match(r.why, /gen-4 is not packed/);
  assert.deepEqual(db.lists, []);
});

test("no meta/active is an empty database, not an error", async () => {
  assert.equal((await D.loadGeneration(fakeDb({}))).status, "empty");
});

test("a generation cleaned up mid-read sends the read back to meta/active", async () => {
  const docs = Object.assign({ "meta/active": meta(4) }, generation(4, [issue(1), issue(2)]), generation(5, [issue(1), issue(2), issue(3)]));
  let first = true;
  const db = fakeDb(docs, (c, store) => {
    if (first && c === "gens/gen-4/parts") {
      first = false;
      // The sync run switched to gen-5 and deleted one of gen-4's parts.
      store.set("meta/active", meta(5));
      store.delete("gens/gen-4/parts/p0002");
    }
  });
  const r = await D.loadGeneration(db);
  assert.equal(r.status, "ok");
  assert.equal(r.generation, "gen-5");
  assert.equal(r.attempts, 2);
  assert.equal(r.data.issues.length, 3);
});

test("a marker already deleted sends the read back to meta/active", async () => {
  const docs = Object.assign({ "meta/active": meta(4) }, generation(5, [issue(1)]));
  let reads = 0;
  const db = fakeDb(docs);
  const get = db.doc;
  db.doc = (p) => {
    const ref = get(p);
    if (p === "meta/active") {
      return { get: async () => (reads++ === 0 ? ref.get() : (db.store.set(p, meta(5)), ref.get())) };
    }
    return ref;
  };
  const r = await D.loadGeneration(db);
  assert.equal(r.status, "ok");
  assert.equal(r.generation, "gen-5");
});

test("a generation that stays short is partial and says what is missing", async () => {
  const docs = Object.assign({ "meta/active": meta(4) }, generation(4, [issue(1), issue(2)]));
  delete docs["gens/gen-4/parts/p0002"];
  const r = await D.loadGeneration(fakeDb(docs), { attempts: 3 });
  assert.equal(r.status, "partial");
  assert.equal(r.attempts, 3);
  assert.match(r.why, /gen-4 has 2 of 3 parts/);
  assert.equal(r.data, undefined);
});

test("a part out of sequence is missing, even when the count matches", async () => {
  const docs = Object.assign({ "meta/active": meta(4) }, generation(4, [issue(1), issue(2)]));
  docs["gens/gen-4/parts/p0002"].index = 3;
  const r = await D.loadGeneration(fakeDb(docs), { attempts: 1 });
  assert.equal(r.status, "partial");
  assert.match(r.why, /gen-4 has 2 of 3 parts/);
});

test("a meta naming no generation is partial without listing anything", async () => {
  const db = fakeDb({ "meta/active": { generation: "../x" } });
  const r = await D.loadGeneration(db);
  assert.equal(r.status, "partial");
  assert.equal(r.why, "meta/active names no generation");
  assert.equal(r.attempts, 1, "a bad name is not retried");
  assert.deepEqual(db.lists, []);
});

test("the tiles use derive.rs's predicates and count only open items", () => {
  const list = [
    issue(1, { priority: { value: "p0" } }),
    issue(2, { status: "needs-decision" }),
    issue(3, { status: "needs-review", outside: true, kind: "pull_request" }),
    issue(4, { status: "needs-review", outside: false, kind: "pull_request" }),
    issue(5, { status: "untriaged" }),
    issue(6, { status: "new", limit_breached: true }),
    issue(7, { state: "closed", priority: { value: "p0" }, status: "untriaged", limit_breached: true }),
  ];
  assert.deepEqual(D.tiles(list), { p0_open: 1, waiting_on_you: 2, untriaged: 2, over_time_limit: 1, open: 6 });
});

test("an item appears once: the inbox takes what needs the owner, the queue the rest", () => {
  const list = [
    issue(1, { status: "needs-decision", created_at: "2026-10-05T00:00:00Z" }),
    issue(2, { status: "untriaged", priority: { value: null }, created_at: "2026-10-01T00:00:00Z" }),
    issue(3, { status: "needs-review", outside: true, kind: "pull_request", created_at: "2026-10-03T00:00:00Z" }),
    issue(4, { priority: { value: "p1" }, idle_days: 2 }),
    issue(5, { priority: { value: "p1" }, idle_days: 9 }),
    issue(6, { priority: { value: null } }),
    issue(7, { kind: "pull_request", status: "needs-review" }),
    issue(8, { state: "closed" }),
  ];
  const cards = D.inbox(list);
  assert.deepEqual(cards.map((c) => [c.issue.id, c.why]), [["I_2", "untriaged"], ["I_3", "outside_pr"], ["I_1", "decision"]]);
  const q = D.queue(list, cards);
  const level = (l) => q.levels.find((x) => x.level === l).items.map((i) => i.id);
  assert.deepEqual(level("p1"), ["I_5", "I_4"], "longest idle first within a level");
  assert.deepEqual(level("none"), ["I_6"]);
  assert.deepEqual(q.pulls.map((i) => i.id), ["I_7"]);
  const everywhere = cards.map((c) => c.issue.id).concat(...q.levels.map((l) => l.items.map((i) => i.id)), q.pulls.map((i) => i.id));
  assert.equal(new Set(everywhere).size, everywhere.length);
  assert.ok(!everywhere.includes("I_8"));
});

test("a view's exclude wins, and an empty include means every repo", () => {
  const groups = { bridges: ["yuvalrakavy/mqtt_dali", "yuvalrakavy/mqtt_ac"] };
  assert.ok(D.inScope("yuvalrakavy/Store", { include: [], exclude: [{ group: "bridges" }] }, groups));
  assert.ok(!D.inScope("yuvalrakavy/mqtt_ac", { include: [], exclude: [{ group: "bridges" }] }, groups));
  const v = { include: [{ group: "bridges" }], exclude: [{ repo: "yuvalrakavy/mqtt_dali" }] };
  assert.ok(D.inScope("yuvalrakavy/mqtt_ac", v, groups));
  assert.ok(!D.inScope("yuvalrakavy/mqtt_dali", v, groups));
  assert.ok(!D.inScope("yuvalrakavy/Store", v, groups));
  assert.ok(D.inScope("anything/at-all", null, groups));
});

test("a decision is pending until acked, and an undo or a refusal takes it off the item", () => {
  const dec = (id, at, more) => Object.assign({ id, created_at: at, action: "set_priority", target: { item: "I_1" }, value: { priority: "p1" } }, more);
  const d1 = dec("D_1", "2026-10-23T08:00:00Z");
  const d2 = dec("D_2", "2026-10-23T08:05:00Z", { value: { priority: "p0" } });
  let s = D.decisionStates([d1, d2], []);
  assert.equal(s.latestFor("I_1").doc.id, "D_2");
  assert.equal(s.latestFor("I_1").ack, null);
  const undo = { id: "D_3", created_at: "2026-10-23T08:06:00Z", action: "undo", target: { decision: "D_2" } };
  s = D.decisionStates([d1, d2, undo], [{ decision: "D_1", result: "applied", cycle: 3 }]);
  assert.equal(s.latestFor("I_1").doc.id, "D_1");
  assert.equal(s.latestFor("I_1").ack.result, "applied");
  assert.equal(s.byId.D_2.undoneBy.id, "D_3");
  s = D.decisionStates([d1], [{ decision: "D_1", result: "refused", cycle: 3, reason: "read-only repo" }]);
  assert.equal(s.latestFor("I_1"), null);
  assert.equal(s.byId.D_1.ack.reason, "read-only repo");
  assert.equal(D.decisionStates([], []).latestFor("I_9"), null);
});

test("the next run is the interval less the generation's age, never below a minute", () => {
  const m = { taken_at: "2026-10-23T08:00:00Z", sync_interval_minutes: 30 };
  assert.equal(D.nextRunMinutes(m, new Date("2026-10-23T08:10:00Z")), 20);
  assert.equal(D.nextRunMinutes(m, new Date("2026-10-23T08:29:50Z")), 1);
  assert.equal(D.nextRunMinutes(m, new Date("2026-10-23T08:31:00Z")), null);
  assert.equal(D.nextRunMinutes({ taken_at: "2026-10-23T08:00:00Z" }, new Date("2026-10-23T08:01:00Z")), 4);
  assert.equal(D.nextRunMinutes({}, new Date()), null);
});

test("trends count opens and closes per week and give over-limit as N of M", () => {
  const now = new Date("2026-10-23T00:00:00Z");
  const list = [
    issue(1, { created_at: "2026-10-20T00:00:00Z" }),
    issue(2, { created_at: "2026-10-10T00:00:00Z", state: "closed", closed_at: "2026-10-21T00:00:00Z" }),
    issue(3, { created_at: "2026-10-12T00:00:00Z", limit_breached: true, alias: "yuvalrakavy/mqtt_ac#3" }),
    issue(4, { created_at: "2025-01-01T00:00:00Z" }),
  ];
  const t = D.trends(list, now, 3, { bridges: ["yuvalrakavy/mqtt_ac"] });
  assert.deepEqual(t.weeks.map((w) => [w.opened, w.closed]), [[0, 0], [2, 0], [1, 1]]);
  assert.deepEqual(t.over, { n: 1, of: 3 });
  assert.equal(t.byGroup.bridges.n, 1);
  assert.equal(t.byGroup.bridges.of, 1);
  assert.equal(t.byRepo["yuvalrakavy/Store"].of, 2);
});

test("a launch row shows the tracker's state, else not_sent, else requested", () => {
  const l1 = { id: "L_1", created_at: "2026-10-23T08:00:00Z", items: ["I_1"] };
  const l2 = { id: "L_2", created_at: "2026-10-23T08:01:00Z", items: ["I_2"] };
  const l3 = { id: "L_3", created_at: "2026-10-23T08:02:00Z", items: ["I_3"] };
  const ns = { id: "L_4", created_at: "2026-10-23T08:02:01Z", not_sent: { launch: "L_3", error: "claude_unavailable" } };
  const s = D.launchStates([l1, l2, l3, ns, { id: "_thread", thread: "t" }], [{ launch: "L_1", state: "started" }]);
  assert.equal(s.I_1.state, "started");
  assert.equal(s.I_2.state, "requested");
  assert.equal(s.I_3.state, "not_sent");
  assert.equal(s.I_3.error, "claude_unavailable");
});

// --- the documents the page writes match their schemas' shape --------------------------

const schemaDir = path.join(__dirname, "..", "schemas");
const DOC_ID = /^[A-Za-z0-9_-]+$/;

function shapeOf(file, doc, branch) {
  let s = JSON.parse(fs.readFileSync(path.join(schemaDir, file), "utf8"));
  if (s.oneOf) s = s.oneOf[branch];
  for (const k of s.required) assert.ok(k in doc, file + " needs " + k);
  for (const k of Object.keys(doc)) assert.ok(k in s.properties, file + " has no " + k);
  assert.match(doc.id, DOC_ID);
  assert.ok(doc.id.length <= 128);
}

const who = { project: "VillaRakavy", author: "user_1" };
const now = new Date("2026-10-23T08:14:05.123Z");

test("page ids are prefix_date_time_rand and every written document fits its schema", () => {
  assert.match(D.newId("D", now, () => 0.5), /^D_20261023_081405_[a-z0-9]{8}$/);
  assert.match(D.newId("D", now, () => 0), /^D_20261023_081405_00000000$/);
  const d = D.decisionDoc({ action: "set_priority", target: { item: "I_1", alias: "a/b#1" }, value: { priority: "p1" } }, who, now);
  assert.equal(d.created_at, "2026-10-23T08:14:05Z");
  shapeOf("page-decision.schema.json", d);
  shapeOf("page-decision.schema.json", D.decisionDoc({ action: "set_override", target: { item: "I_1" }, value: { priority: "p0" }, reason: "blocks" }, who, now));
  const l = D.launchDoc({ kind: "fix", items: ["I_1"], aliases: ["a/b#1"], recipe: "default", routing: { model: "m", effort: "medium" } }, who, now);
  shapeOf("page-launch.schema.json", l, 0);
  shapeOf("page-launch.schema.json", D.notSentDoc(l.id, "no_session", who, now), 1);
  shapeOf("page-launch.schema.json", D.threadDoc("thr_1", who, now), 2);
  shapeOf("page-view.schema.json", D.viewDoc("Without bridges", { exclude: [{ group: "bridges" }] }, who, now));
  shapeOf("page-request.schema.json", D.requestDoc({ kind: "triage", items: ["I_1"] }, who, now));
});

// --- launch (5.4) -----------------------------------------------------------------------

function launchDeps(db, comments) {
  let t = 0;
  return {
    db,
    comments,
    who,
    now: () => new Date(now.getTime() + 1000 * t++),
    rand: Math.random,
    anchor: async () => ({ kind: "element", id: "launch-sheet" }),
  };
}
const fields = { kind: "fix", items: ["I_1"], aliases: ["yuvalrakavy/Store#1"], recipe: "default", routing: { model: "claude-opus-5-5", effort: "medium" } };
const launches = (db) => [...db.store.entries()].filter(([k]) => k.startsWith("launches/")).map(([, v]) => v);

test("a launch that is not available writes nothing", async () => {
  for (const gate of ["no_session", "writers_only", "off", "something-new"]) {
    const db = fakeDb({});
    const r = await D.launch(launchDeps(db, { canSendToClaude: async () => gate }), fields);
    assert.deepEqual([r.ok, r.gate], [false, gate]);
    assert.equal(db.store.size, 0);
  }
  const db = fakeDb({});
  const r = await D.launch(launchDeps(db, { canSendToClaude: async () => Promise.reject({ code: "capability_removed" }) }), fields);
  assert.equal(r.ok, false);
  assert.equal(db.store.size, 0);
});

test("the first launch starts the thread and records it; the next replies into it", async () => {
  const sends = [];
  const comments = {
    canSendToClaude: async () => "available",
    sendToClaude: async (t) => {
      sends.push(t);
      return { threadId: t.threadId || "thr_1", commentId: "c" + sends.length };
    },
  };
  const db = fakeDb({});
  const r1 = await D.launch(launchDeps(db, comments), fields);
  assert.ok(r1.ok);
  assert.deepEqual(launches(db).filter((l) => l.thread).map((l) => l.thread), ["thr_1"]);
  assert.deepEqual(sends[0].text, r1.launch.id, "the comment holds only the launch id");
  assert.ok(sends[0].anchor);
  const r2 = await D.launch(launchDeps(db, comments), fields);
  assert.deepEqual(sends[1], { threadId: "thr_1", text: r2.launch.id });
  assert.equal(launches(db).filter((l) => l.kind).length, 2);
});

test("a thread gone or full starts a new one, recorded beside the old", async () => {
  for (const code of ["not_found", "upstream_error"]) {
    const comments = {
      canSendToClaude: async () => "available",
      sendToClaude: async (t) => {
        if (t.threadId === "thr_old") throw { code };
        return { threadId: "thr_new", commentId: "c" };
      },
    };
    const db = fakeDb({ "launches/_thread": { id: "_thread", thread: "thr_old" } });
    const r = await D.launch(launchDeps(db, comments), fields);
    assert.ok(r.ok, code);
    assert.equal(db.store.get("launches/_thread").thread, "thr_old");
    const rec = launches(db).filter((l) => l.thread && l.id !== "_thread");
    assert.deepEqual(rec.map((l) => l.thread), ["thr_new"]);
    assert.ok(rec[0].created_at);
  }
});

test("a refused send leaves a not_sent document naming the launch", async () => {
  for (const code of ["claude_unavailable", "rate_limited", "consent_required"]) {
    let sends = 0;
    const comments = { canSendToClaude: async () => "available", sendToClaude: async () => { sends++; return Promise.reject({ code }); } };
    const db = fakeDb({ "launches/_thread": { id: "_thread", thread: "thr_1" } });
    const r = await D.launch(launchDeps(db, comments), fields);
    assert.deepEqual([r.ok, r.error], [false, code]);
    assert.equal(sends, 1, code + " is not a lost thread, so no new thread is tried");
    const ns = launches(db).filter((l) => l.not_sent);
    assert.equal(ns.length, 1);
    assert.deepEqual(ns[0].not_sent, { launch: r.launch.id, error: code });
    assert.notEqual(ns[0].id, r.launch.id);
    assert.equal(launches(db).filter((l) => l.thread).length, 1, "a refusal other than a lost thread starts no thread");
  }
  // The retry into a new thread failing too is one not_sent, not two.
  const comments = { canSendToClaude: async () => "available", sendToClaude: async () => Promise.reject({ code: "upstream_error" }) };
  const db = fakeDb({ "launches/_thread": { id: "_thread", thread: "thr_1" } });
  const r = await D.launch(launchDeps(db, comments), fields);
  assert.equal(r.error, "upstream_error");
  assert.equal(launches(db).filter((l) => l.not_sent).length, 1);
});
