// node --test page/  — the slice-3 gate's round-1 page findings, each pinned: a refused send
// stays not_sent, thread records are written once, a failed thread read records nothing, an
// ack past its window does not bring a decision back, a generation announced mid-load is
// loaded, and page ids carry enough randomness not to collide.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const D = require("./core.js");

const who = { project: "villa-rakavy", author: "user_1" };
const now = new Date("2026-10-23T08:14:05.123Z");
const fields = { kind: "fix", items: ["I_1"], aliases: ["yuvalrakavy/Store#1"], recipe: "default", routing: { model: "m", effort: "medium" } };

// A db over a map that logs every write's path; `failList`/`failSet(path)` make reads or writes reject.
function db(docs, opts) {
  const store = new Map(Object.entries(docs || {}));
  const writes = [];
  const o = opts || {};
  return {
    store,
    writes,
    doc: (p) => ({
      get: async () => ({ exists: store.has(p), data: () => store.get(p) }),
      set: async (d) => {
        if (o.failSet && o.failSet(p, d)) throw { code: "unavailable" };
        writes.push(p);
        store.set(p, JSON.parse(JSON.stringify(d)));
      },
    }),
    collection: (c) => {
      const q = { limit: () => q, get: async () => {
        if (o.failList) throw { code: "unavailable" };
        const docs = [...store.keys()].filter((k) => k.startsWith(c + "/")).sort().map((k) => ({ id: k.split("/").pop(), data: () => store.get(k) }));
        return { docs, size: docs.length };
      } };
      return q;
    },
  };
}

let t = 0;
let n = 0;
function deps(d, comments) {
  return { db: d, comments, who, now: () => new Date(now.getTime() + 1000 * t++), rand: () => ((++n * 0.1234567) % 1), anchor: async () => ({ kind: "element", id: "x" }) };
}

const sender = (lost) => {
  const sends = [];
  return {
    sends,
    canSendToClaude: async () => "available",
    sendToClaude: async (t) => {
      sends.push(t);
      if (t.threadId && lost.includes(t.threadId)) throw { code: "not_found" };
      return { threadId: t.threadId || "thr_" + sends.length };
    },
  };
};
const threads = (d) => [...d.store.values()].filter((v) => typeof v.thread === "string");

test("a send refused is not_sent even after a sync reports the launch requested", () => {
  const l = { id: "L_1", created_at: "2026-10-23T08:00:00Z", items: ["I_1"] };
  const ns = { id: "L_2", created_at: "2026-10-23T08:00:01Z", not_sent: { launch: "L_1", error: "no_session" } };
  assert.equal(D.launchStates([l, ns], [{ launch: "L_1", state: "requested" }]).I_1.state, "not_sent");
  assert.equal(D.launchStates([l, ns], [{ launch: "L_1", state: "started" }]).I_1.state, "started", "a real transition supersedes it");
  assert.equal(D.launchStates([l], [{ launch: "L_1", state: "requested" }]).I_1.state, "requested");
});

test("each thread is its own record, written once; the newest is the current thread", async () => {
  const c = sender(["thr_1"]);
  const d = db({ "launches/_thread": { id: "_thread", thread: "thr_0" } });
  const r1 = await D.launch(deps(d, c), fields);
  assert.ok(r1.ok);
  assert.equal(c.sends[0].threadId, "thr_0", "the legacy record is still read");
  const r2 = await D.launch(deps(d, c), fields);
  assert.deepEqual(c.sends[1], { threadId: "thr_0", text: r2.launch.id });
  // thr_0 is lost: the next launch starts a new thread and records it beside the old one.
  c.sends.length = 0;
  const lostOld = sender(["thr_0"]);
  const r3 = await D.launch(deps(d, lostOld), fields);
  assert.ok(r3.ok);
  assert.equal(threads(d).length, 2);
  const rec = threads(d).find((t) => t.id !== "_thread");
  assert.match(rec.id, /^T_/);
  assert.equal(rec.thread, r3.threadId);
  assert.equal(rec.created_at.length, 20);
  const r4 = await D.launch(deps(d, lostOld), fields);
  assert.equal(lostOld.sends.at(-1).threadId, r3.threadId, "the newest record is the thread");
  assert.ok(r4.ok);
  const counts = {};
  for (const p of d.writes) counts[p] = (counts[p] || 0) + 1;
  assert.deepEqual(Object.entries(counts).filter(([, n]) => n > 1), [], "no page document is written twice");
  assert.ok(!d.writes.includes("launches/_thread"));
});

test("a thread read that fails records nothing; a thread record that fails after a send is still a launch", async () => {
  const c = sender([]);
  const unreadable = db({}, { failList: true });
  const r = await D.launch(deps(unreadable, c), fields);
  assert.equal(r.ok, false);
  assert.equal(unreadable.writes.length, 0, "no launch is left without a send or a not_sent");
  assert.equal(c.sends.length, 0);

  const d = db({}, { failSet: (p, doc) => typeof doc.thread === "string" });
  const r2 = await D.launch(deps(d, sender([])), fields);
  assert.equal(r2.ok, true, "the comment went out, so the launch was heard");
  assert.ok(r2.threadNotRecorded);
  assert.equal([...d.store.values()].filter((v) => v.not_sent).length, 0);
});

test("a decision past the ack window with no ack is settled, not pending", () => {
  const meta = { taken_at: "2026-12-01T00:00:00Z" };
  const old = { id: "D_1", created_at: "2026-10-01T00:00:00Z", action: "set_priority", target: { item: "I_1" }, value: { priority: "p1" } };
  const fresh = { id: "D_2", created_at: "2026-11-30T00:00:00Z", action: "set_priority", target: { item: "I_2" }, value: { priority: "p1" } };
  const h = D.ackHorizon(meta);
  const s = D.decisionStates([old, fresh], [], h);
  assert.equal(s.latestFor("I_1"), null);
  assert.equal(s.latestFor("I_2").doc.id, "D_2");
  const close = { id: "D_3", created_at: "2026-10-01T00:00:00Z", action: "close", target: { item: "I_3" }, value: { close: { reason: "completed", comment: "" } } };
  assert.equal(D.pendingClose([close], [], "I_3", h), null);
  assert.equal(D.pendingClose([close], [], "I_3", null).id, "D_3", "with no snapshot there is no horizon");
  assert.equal(D.ackHorizon(null), null);
});

test("the page's ack window is derive.rs's", () => {
  const rs = fs.readFileSync(path.join(__dirname, "..", "crates", "core", "src", "derive.rs"), "utf8");
  assert.match(rs, new RegExp("pub const ACK_DAYS: i64 = " + D.ACK_DAYS + ";"));
});

test("a generation announced while one loads is loaded after it", async () => {
  const loads = [];
  let follow;
  follow = D.follower(async () => {
    loads.push(loads.length);
    if (loads.length === 1) { follow(); follow(); }
  });
  await follow();
  await new Promise((r) => setTimeout(r, 0));
  assert.equal(loads.length, 2, "two announcements mid-load are one more load");
  await follow();
  assert.equal(loads.length, 3);
});

test("page ids carry 8 random characters, so Codex's colliding pair no longer collides", () => {
  const a = D.newId("D", now, () => 0.6145960167267817);
  const b = D.newId("D", now, () => 0.7243957089540976);
  assert.notEqual(a, b);
  assert.match(a, /^D_20261023_081405_[a-z0-9]{8}$/);
  assert.match(D.newId("D", now, () => 0), /^D_20261023_081405_00000000$/);
  assert.match(D.newId("D", now, () => 0.9999999999), /^D_20261023_081405_[a-z0-9]{8}$/);
});
