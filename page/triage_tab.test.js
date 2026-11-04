// node --test page/  — the Triage tab (spec 5.2): one row per issue over its triage cards,
// second opinions apart, "Apply eligible" under a confidence threshold that never widens Bulk
// (triage spec 4.3), and the Inbox keeping an untriaged issue only when it has no row.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const D = require("./core.js");

const STORE = "yuvalrakavy/Store";
const issue = (n, more) => Object.assign({ id: "I_" + n, alias: STORE + "#" + n, kind: "issue", state: "open", title: "t" + n, created_at: "2026-10-0" + (n % 9 + 1) + "T00:00:00Z" }, more);
const tg = (kind, n, more) => Object.assign({
  id: "sg-" + kind + n, project: "VillaRakavy", run_id: "L1", kind, made_at: "2026-10-23T10:00:00Z", confidence: 0.9,
  item: "I_" + n, alias: STORE + "#" + n, decision: kind, answer: kind === "area" ? ["kernel"] : kind === "type" ? "bug" : "p1",
  probabilities: {}, backend: "triage", evaluated: true, reason: "r", current: null, badge: "evaluated", bulk_ok: true, card: "sg-" + kind + n,
}, more);
const so = (kind, n, more) => tg(kind, n, Object.assign({ id: "so-" + kind + n, card: "so-" + kind + n, current: "x", second_opinion: { current: "x", p_current: 0.1, false_alarm_rate: 0.1 } }, more));

test("an issue's cards make one row: answers in type, area, priority order, the lowest confidence, the worst badge", () => {
  const parts = [
    tg("type", 1, { card: "cd-1", confidence: 0.85, action_label: "Set bug · p2" }),
    tg("priority", 1, { card: "cd-1", answer: "p2", confidence: 0.48, action_label: "Set bug · p2" }),
    tg("area", 1, { answer: ["harness", "kernel"], confidence: 0.5, badge: "uncalibrated" }),
    tg("area", 2),
  ];
  const r = D.triageRows(parts, [issue(1), issue(2)], [], []);
  assert.deepEqual(r.rows.map((x) => [x.issue.alias, x.cards.length, x.label, x.confidence, x.badge]), [
    [STORE + "#1", 2, "Set bug · harness, kernel · p2", 0.48, "uncalibrated"],
    [STORE + "#2", 1, "Set kernel", 0.9, "evaluated"],
  ]);
  assert.deepEqual(r.rows[0].parts.map((s) => s.kind), ["type", "area", "priority"]);
  assert.deepEqual(r.seconds, []);
  assert.equal(D.triageCount(r), 2);
});

test("a second opinion is its own row, apart from a first-time row on the same issue, and the count is by issue", () => {
  const r = D.triageRows([tg("area", 1), so("type", 1), so("priority", 2)], [issue(1), issue(2)], [], []);
  assert.deepEqual(r.rows.map((x) => [x.issue.alias, x.second]), [[STORE + "#1", false]]);
  assert.deepEqual(r.seconds.map((x) => [x.issue.alias, x.second]), [[STORE + "#2", true], [STORE + "#1", true]]);
  assert.equal(D.triageCount(r), 2, "issue 1 has two rows but is one issue to triage");
});

test("Apply eligible takes only Bulk cards at or above the threshold, never a second opinion", () => {
  const r = D.triageRows([
    tg("type", 1, { confidence: 0.99, bulk_ok: false }),
    tg("type", 2, { confidence: 0.86 }),
    tg("type", 3, { confidence: 0.84 }),
    so("type", 4, { confidence: 0.99, bulk_ok: true }),
  ], [issue(1), issue(2), issue(3), issue(4)], [], []);
  assert.deepEqual(D.eligibleCards(r, 0.85).map((c) => c.issue.alias), [STORE + "#2"]);
  assert.deepEqual(D.eligibleCards(r, 0).map((c) => c.issue.alias), [STORE + "#2", STORE + "#3"], "a threshold of 0 is still bounded by Bulk");
  assert.deepEqual(D.applyAllFields(D.eligibleCards(r, 0)).map((f) => f.target.suggestion), ["sg-type2", "sg-type3"]);
});

test("with nothing eligible the bar says why", () => {
  const none = D.triageRows([tg("type", 1, { bulk_ok: false, badge: "uncalibrated" }), tg("area", 1, { bulk_ok: false, badge: "uncalibrated" }), tg("type", 2, { bulk_ok: false, badge: "evaluated" })], [issue(1), issue(2)], [], []);
  assert.equal(D.eligibleNote(none, 0.85), "No triage question has passed the Bulk bar yet (2 of 3 suggestions uncalibrated).");
  const low = D.triageRows([tg("type", 1, { confidence: 0.7 })], [issue(1)], [], []);
  assert.equal(D.eligibleNote(low, 0.85), "1 suggestion passes Bulk but is below 85%.");
  const some = D.triageRows([tg("type", 1)], [issue(1)], [], []);
  assert.equal(D.eligibleNote(some, 0.85), null);
  assert.equal(D.eligibleNote(D.triageRows([], [], [], []), 0.85), null, "no rows, no note");
});

test("the Inbox keeps an untriaged issue only when it has no first-time triage row", () => {
  const issues = [issue(1, { status: "untriaged" }), issue(2, { status: "untriaged" }), issue(3, { status: "needs-decision" }), issue(4, { status: "untriaged", type: "bug" })];
  const r = D.triageRows([tg("type", 1), tg("priority", 3), so("type", 4)], issues, [], []);
  const absorbed = new Set(r.rows.map((x) => x.issue.id));
  assert.deepEqual(D.inbox(issues, absorbed).map((c) => [c.issue.alias, c.why]), [
    [STORE + "#2", "untriaged"], [STORE + "#3", "decision"], [STORE + "#4", "untriaged"],
  ], "#1 lives in Triage; #3's decision stays; #4's second opinion does not absorb it");
  assert.equal(D.inbox(issues).length, 4, "without a set the Inbox is as before");
});

test("the page has a Triage tab and the Inbox no longer renders the triage sections", () => {
  const html = fs.readFileSync(path.join(__dirname, "dashboard.html"), "utf8");
  assert.match(html, /data-tab="triage"/);
  const inbox = html.slice(html.indexOf("function renderInbox()"), html.indexOf("function renderTriage()"));
  assert.ok(inbox.length > 0, "renderInbox precedes renderTriage");
  assert.doesNotMatch(inbox, /triageSection|strongerLookSection|openTriage/);
});

// The tile counts first-time and second-opinion issues together, so it must open a list
// holding both (spec 5.2: every tile links to the filter it counts).
test("the to-triage tile opens the All filter, which lists rows and second opinions", () => {
  const html = fs.readFileSync(path.join(__dirname, "dashboard.html"), "utf8");
  const strip = html.slice(html.indexOf("function renderStrip"), html.indexOf("function renderStrip") + 4000);
  assert.match(strip, /"to triage · "[^\n]*go\("triage", "all"\)/);
  assert.match(html, /triageFilter: "all"/);
  assert.match(html, /f === "all" \? t\.rows\.concat\(t\.seconds\)/);
});
