// node --test page/  — Run triage… and Evaluate… on the page (triage spec 5.1, 5.3): the items
// a triage launch names, the eval launch, the "no evaluation" warning, and when the button is off.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const D = require("./core.js");

const who = { project: "VillaRakavy", author: "user_1" };
const now = new Date("2026-10-29T08:00:00Z");
const STORE = "yuvalrakavy/Store";
const issue = (n, more) => Object.assign({ id: "I_" + n, alias: STORE + "#" + n, kind: "issue", state: "open", title: "t" + n }, more);
const routing = { model: "claude-opus-5-5", effort: "medium", reason: "chosen on the launch sheet" };
const meta = {
  triage_queue: { full: ["I_3", "I_1", "I_9", "I_2"], closures: ["I_2"] },
  evaluated_backends: ["claude-opus-5-5@medium"],
};

test("a triage launch names the due items in view, in the tracker's order, at scope full", () => {
  const issues = [issue(1), issue(2, { state: "closed" }), issue(3), issue(4)];
  const r = D.triageLaunchFields(issues, meta, "full", routing);
  assert.deepEqual(r.fields, {
    kind: "triage",
    scope: "full",
    items: ["I_3", "I_1"],
    aliases: ["yuvalrakavy/Store#3", "yuvalrakavy/Store#1"],
    recipe: "read-only",
    routing,
  });
});

test("scope closures takes the closures queue", () => {
  const r = D.triageLaunchFields([issue(1), issue(2)], meta, "closures", routing);
  assert.deepEqual([r.fields.scope, r.fields.items], ["closures", ["I_2"]]);
});

test("nothing due in view is an error, per scope, and no fields", () => {
  assert.deepEqual(D.triageLaunchFields([issue(5)], meta, "full", routing), { error: "No open issue in view is due for triage." });
  assert.deepEqual(D.triageLaunchFields([issue(5)], meta, "closures", routing), { error: "No open issue in view is due for a closure check." });
  assert.deepEqual(D.triageLaunchFields([issue(1)], {}, "full", routing), { error: "No open issue in view is due for triage." });
});

test("the launch document carries the scope, and only when the fields name one", () => {
  const f = D.triageLaunchFields([issue(1)], meta, "full", routing).fields;
  const doc = D.launchDoc(f, who, now, () => 0);
  assert.equal(doc.scope, "full");
  assert.equal(doc.kind, "triage");
  const fix = D.launchDoc({ kind: "fix", items: ["I_1"], aliases: [STORE + "#1"], recipe: "lanes", routing }, who, now, () => 0);
  assert.equal("scope" in fix, false);
});

test("an eval launch names no items and runs read-only at the backend measured", () => {
  const r = { model: "claude-sonnet-5-5", effort: "high" };
  assert.deepEqual(D.evalLaunchFields(r), {
    fields: { kind: "eval", items: [], aliases: [], recipe: "read-only", routing: r },
  });
  const doc = D.launchDoc(D.evalLaunchFields(r).fields, who, now, () => 0);
  assert.deepEqual([doc.kind, doc.items, doc.aliases, "scope" in doc], ["eval", [], [], false]);
});

test("a routing no evaluation measured warns; a measured one does not", () => {
  assert.equal(D.noEvaluation({ model: "claude-opus-5-5", effort: "medium" }, meta), false);
  assert.equal(D.noEvaluation({ model: "claude-opus-5-5", effort: "high" }, meta), true);
  assert.equal(D.noEvaluation({ model: "claude-sonnet-5-5", effort: "medium" }, meta), true);
  assert.equal(D.noEvaluation({ model: "claude-opus-5-5", effort: "medium" }, {}), true);
});

test("Run triage is off while a triage launch is live, the daily run's included", () => {
  // The tracker reports the daily run's launch, which the page never wrote.
  const scheduled = [{ launch: "L_daily", kind: "triage", scope: "full", author: "schedule", state: "reserved" }];
  assert.equal(D.triageLive([], scheduled), true);
  for (const state of ["requested", "queued", "reserved", "unconfirmed", "started"]) {
    assert.equal(D.triageLive([], [{ launch: "L1", kind: "triage", state }]), true, state);
  }
  for (const state of ["ended", "released", "withdrawn"]) {
    assert.equal(D.triageLive([], [{ launch: "L1", kind: "triage", state }]), false, state);
  }
  // The alias counts; another kind does not.
  assert.equal(D.triageLive([], [{ launch: "L1", kind: "recommend_closures", state: "started" }]), true);
  assert.equal(D.triageLive([], [{ launch: "L1", kind: "fix", state: "started" }]), false);
  assert.equal(D.triageLive([], [{ launch: "L1", kind: "eval", state: "started" }]), false);
});

test("a triage launch the page wrote counts as live until the tracker reports it, unless it was not sent", () => {
  const mine = { id: "L1", kind: "triage", items: ["I_1"] };
  assert.equal(D.triageLive([mine], []), true);
  assert.equal(D.triageLive([mine], [{ launch: "L1", kind: "triage", state: "ended" }]), false);
  assert.equal(D.triageLive([mine, { id: "L2", not_sent: { launch: "L1", error: "x" } }], []), false);
  assert.equal(D.triageLive([{ id: "L3", kind: "fix", items: ["I_1"] }], []), false);
});

test("the Inbox offers Run triage… to the owner, off while live, and the sheet warns with Evaluate…", () => {
  const html = fs.readFileSync(path.join(__dirname, "dashboard.html"), "utf8");
  assert.match(html, /"Run triage…"/);
  assert.match(html, /disabled: live/);
  assert.match(html, /This routing has no evaluation/);
  assert.match(html, /"Evaluate…"/);
});

test("a send the page recorded as refused is not live, even once the tracker reports it requested", () => {
  const mine = { id: "L1", kind: "triage", items: ["I_1"] };
  const notSent = { id: "L2", not_sent: { launch: "L1", error: "upstream_error" } };
  const requested = [{ launch: "L1", kind: "triage", state: "requested" }];
  assert.equal(D.triageLive([mine, notSent], requested), false);
  // A later transition outranks the page's record: a dispatcher has it.
  assert.equal(D.triageLive([mine, notSent], [{ launch: "L1", kind: "triage", state: "reserved" }]), true);
  // Without the record, requested is live.
  assert.equal(D.triageLive([mine], requested), true);
});

test("a page launch no dispatcher took for a day stops disabling Run triage, as the tracker stops counting it", () => {
  const mine = { id: "L1", kind: "triage", items: ["I_1"], created_at: "2026-10-28T08:00:00Z" };
  const requested = [{ launch: "L1", kind: "triage", author: "user_1", state: "requested" }];
  assert.equal(D.triageLive([mine], requested, new Date("2026-10-29T07:59:59Z")), true);
  assert.equal(D.triageLive([mine], requested, new Date("2026-10-29T08:00:00Z")), false);
  // Taken by a dispatcher, it is live whatever its age; and the schedule's request never ages.
  assert.equal(D.triageLive([mine], [{ launch: "L1", kind: "triage", author: "user_1", state: "reserved" }], new Date("2026-10-30T08:00:00Z")), true);
  const daily = { id: "L2", kind: "triage", items: ["I_1"], created_at: "2026-10-28T08:00:00Z" };
  assert.equal(D.triageLive([daily], [{ launch: "L2", kind: "triage", author: "schedule", state: "requested" }], new Date("2026-10-30T08:00:00Z")), true);
});

test("a triage, closure check or eval over an item leaves its fix launch available", () => {
  const launches = [
    { id: "T1", kind: "triage", items: ["I_1"] },
    { id: "R1", kind: "recommend_closures", items: ["I_2"] },
    { id: "S1", kind: "stronger_look", items: ["I_3"] },
    { id: "F1", kind: "fix", items: ["I_4"] },
  ];
  const statuses = ["T1", "R1", "S1", "F1"].map((l) => ({ launch: l, state: "released" }));
  assert.deepEqual(Object.keys(D.fixLaunchStates(launches, statuses)), ["I_4"]);
  assert.equal(D.fixLaunchStates(launches, statuses).I_4.state, "released");
  // The rows still show every launch's state.
  assert.deepEqual(Object.keys(D.launchStates(launches, statuses)).sort(), ["I_1", "I_2", "I_3", "I_4"]);
});

test("the cap applies after the repos in view are kept, so one repo cannot hide another's", () => {
  const OTHER = "yuvalrakavy/airzoned";
  const capped = { triage_queue: { full: ["I_1", "I_2", "I_3", "I_7", "I_8"] }, max_triage_items: 2 };
  const inView = [issue(7, { alias: OTHER + "#7" }), issue(8, { alias: OTHER + "#8" })];
  assert.deepEqual(D.triageLaunchFields(inView, capped, "full", routing).fields.items, ["I_7", "I_8"]);
  const all = [issue(1), issue(2), issue(3), ...inView];
  assert.deepEqual(D.triageLaunchFields(all, capped, "full", routing).fields.items, ["I_1", "I_2"]);
  // With no cap in meta, the tracker's default of 40.
  const many = Array.from({ length: 45 }, (_, k) => issue(100 + k));
  const m = { triage_queue: { full: many.map((i) => i.id) } };
  assert.equal(D.triageLaunchFields(many, m, "full", routing).fields.items.length, 40);
});

test("Trends offers the owner Evaluate…, and a refused Launch on the sheet can be pressed again", () => {
  const html = fs.readFileSync(path.join(__dirname, "dashboard.html"), "utf8");
  assert.match(html, /S\.owner \? h\("div", \{ class: "pills" \}, h\("button", \{ class: "btn", onclick: \(\) => openEvaluate\(/);
  assert.match(html, /if \(f\.error\) \{ status\.textContent = f\.error; go\.disabled = false; return; \}/);
  assert.match(html, /const st = fixStates\[i\.id\];/);
  assert.match(html, /const live = D\.triageLive\(S\.launches, S\.statuses, new Date\(\)\);/);
});
