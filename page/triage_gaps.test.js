// node --test page/  — the triage spec's gaps on the page: the support question's drafted
// reply in the close sheet (1.4), "Needs a stronger look" (2.5) and the Trends rows of the
// metrics documents (4.4).
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const D = require("./core.js");

const STORE = "yuvalrakavy/Store";
const issue = (n, more) => Object.assign({ id: "I_" + n, alias: STORE + "#" + n, kind: "issue", state: "open", title: "t" + n }, more);
const look = (decision, confidence) => ({ decision, answer: decision === "type" ? "bug" : false, confidence, backend: "claude-opus-5-5@medium", suggestion: "sg-" + decision });
const flagged = (n, more) => issue(n, Object.assign({ triage: { stronger_look: [look("type", 0.5)] } }, more));

test("a support question's close sheet opens with its drafted reply", () => {
  assert.deepEqual(D.closePrefill({ kind: "support_question", reply: "Use the settings page." }), { reason: "not_planned", comment: "Use the settings page." });
  assert.deepEqual(D.closePrefill({ kind: "support_question" }), { reason: "not_planned" });
  assert.deepEqual(D.closePrefill({ kind: "obsolete", reply: "ignored" }), { reason: "not_planned" });
});

test("the stronger-look list holds the open issues with answers that need one, by alias", () => {
  const list = D.strongerLookList([
    flagged(12),
    issue(3),
    flagged(10),
    flagged(11, { state: "closed" }),
    issue(4, { triage: { stronger_look: [] } }),
    issue(5, { triage: {} }),
  ]);
  assert.deepEqual(list.map((x) => x.issue.id), ["I_10", "I_12"]);
  assert.deepEqual(list[0].answers, [look("type", 0.5)]);
  assert.deepEqual(D.strongerLookList(undefined), []);
});

test("Take a stronger look writes a read-only stronger_look launch over the listed issues, capped", () => {
  const routing = { model: "claude-fable-5-1", effort: "high" };
  const r = D.strongerLookFields([flagged(2), flagged(1), issue(3)], { max_triage_items: 40 }, routing);
  assert.deepEqual(r, {
    fields: { kind: "stronger_look", items: ["I_1", "I_2"], aliases: [STORE + "#1", STORE + "#2"], routing, recipe: "read-only" },
  });
  const capped = D.strongerLookFields([flagged(1), flagged(2), flagged(3)], { max_triage_items: 2 }, routing);
  assert.deepEqual(capped.fields.items, ["I_1", "I_2"]);
  assert.deepEqual(D.strongerLookFields([issue(1)], {}, routing), { error: "No open issue in view needs a stronger look." });
});

test("a live stronger look is told apart from a live triage run", () => {
  const live = [{ launch: "S1", kind: "stronger_look", state: "started" }];
  assert.equal(D.triageLive([], live, new Date(), ["stronger_look"]), true);
  assert.equal(D.triageLive([], live), false);
  assert.equal(D.triageLive([], [{ launch: "T1", kind: "triage", state: "started" }], new Date(), ["stronger_look"]), false);
});

test("Trends shows each triage key's metrics as accepted of acted on, with its accuracy", () => {
  const key = (decision, backend, more) => Object.assign({
    id: "mt-" + decision, project: "home", kind: "triage_key", period_days: 30, backend, decision, question_hash: "h",
    accepted: 41, bulk: 3, edited: 2, dismissed: 1, of: 44, evaluated_accuracy: 0.861, drifting: false,
  }, more);
  const rows = D.metricsRows([
    key("type", "b2"),
    { id: "m-trends", kind: "trends" },
    key("priority", "b1", { accepted: 0, bulk: 0, edited: 0, dismissed: 0, of: 0, evaluated_accuracy: null, drifting: true }),
    key("type", "b1"),
  ]);
  assert.deepEqual(rows, [
    { decision: "priority", backend: "b1", ratio: "priority: 0 of 0", edited: 0, dismissed: 0, bulk: 0, accuracy: "not evaluated", drifting: true, days: 30 },
    { decision: "type", backend: "b1", ratio: "type: 41 of 44", edited: 2, dismissed: 1, bulk: 3, accuracy: "86%", drifting: false, days: 30 },
    { decision: "type", backend: "b2", ratio: "type: 41 of 44", edited: 2, dismissed: 1, bulk: 3, accuracy: "86%", drifting: false, days: 30 },
  ]);
  assert.deepEqual(D.metricsRows(undefined), []);
});
