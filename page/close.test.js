// node --test page/  — closing from the page (5.2a, 10.10): who may close, the sheet's fields,
// the decision it writes, and the recent closes the Inbox offers to reopen.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const D = require("./core.js");

const schema = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "schemas", "page-decision.schema.json"), "utf8"));
const who = { project: "villa-rakavy", author: "user_1" };
const now = new Date("2026-10-23T08:14:05.123Z");
const issue = (more) => Object.assign({ id: "I_7", alias: "yuvalrakavy/Store#7", kind: "issue", state: "open" }, more);
const repo = (more) => Object.assign({ repo: "yuvalrakavy/Store", read_only: { value: false, why: null }, visibility: "public" }, more);

test("only an open issue on a writable repo can be closed", () => {
  assert.equal(D.closable(issue(), repo()), true);
  assert.equal(D.closable(issue({ kind: "pull_request" }), repo()), false);
  assert.equal(D.closable(issue({ state: "closed" }), repo()), false);
  assert.equal(D.closable(issue(), repo({ read_only: { value: true, why: "declared" } })), false);
  assert.equal(D.closable(issue(), undefined), false, "a repo the meta does not name is not known writable");
});

test("each reason prefills its own comment, and a duplicate's names the other issue", () => {
  assert.equal(D.closeComment("not_planned"), "Closed from the issue dashboard: no longer relevant.");
  assert.match(D.closeComment("completed"), /^Closed from the issue dashboard/);
  assert.equal(D.closeComment("duplicate", 12), "Closed from the issue dashboard as a duplicate of #12.");
  assert.notEqual(D.closeComment("completed"), D.closeComment("not_planned"));
});

test("the sheet's fields become a close decision that fits the schema", () => {
  const r = D.closeFields(issue(), "not_planned", "", "  Closed from the issue dashboard: no longer relevant. ");
  assert.equal(r.error, undefined);
  const d = D.decisionDoc(r.fields, who, now, () => 0.5);
  assert.equal(d.action, "close");
  assert.deepEqual(d.target, { item: "I_7", alias: "yuvalrakavy/Store#7" });
  assert.deepEqual(d.value, { close: { reason: "not_planned", comment: "Closed from the issue dashboard: no longer relevant." } });
  for (const k of Object.keys(d)) assert.ok(k in schema.properties, "page-decision has no " + k);
  assert.ok(schema.properties.action.enum.includes("close"));
  const closeKeys = schema.properties.value.properties.close.properties;
  for (const k of Object.keys(d.value.close)) assert.ok(k in closeKeys, "value.close has no " + k);
});

test("a duplicate needs a whole number other than the issue's own", () => {
  assert.match(D.closeFields(issue(), "duplicate", "", "x").error, /number/);
  assert.match(D.closeFields(issue(), "duplicate", "abc", "x").error, /number/);
  assert.match(D.closeFields(issue(), "duplicate", "0", "x").error, /number/);
  assert.match(D.closeFields(issue(), "duplicate", "7", "x").error, /itself/);
  assert.equal(D.closeFields(issue(), "duplicate", "#12", "x").fields.value.close.duplicate_of, 12);
  assert.equal(D.closeFields(issue(), "completed", "12", "x").fields.value.close.duplicate_of, undefined, "only a duplicate carries the number");
  assert.match(D.closeFields(issue(), "completed", "", "x".repeat(65537)).error, /long/);
  assert.match(D.closeFields(issue(), "wontfix", "", "x").error, /reason/);
});

test("a close is pending until acked, and the line says so", () => {
  const c = { id: "D_c", created_at: "2026-10-23T08:00:00Z", action: "close", target: { item: "I_7", alias: "a/b#7" }, value: { close: { reason: "completed", duplicate_of: null, comment: "" } } };
  assert.equal(D.pendingClose([c], [], "I_7").id, "D_c");
  assert.equal(D.pendingClose([c], [{ decision: "D_c", result: "applied" }], "I_7"), null);
  const u = { id: "D_u", created_at: "2026-10-23T08:01:00Z", action: "undo", target: { decision: "D_c", item: "I_7" } };
  assert.equal(D.pendingClose([c, u], [], "I_7"), null, "an undo takes it back");
  assert.equal(D.pendingClose([c], [], "I_8"), null);
  assert.equal(D.describeClose(c.value.close), "close as completed");
  assert.equal(D.describeClose({ reason: "duplicate", duplicate_of: 3 }), "close as a duplicate of #3");
  assert.equal(D.describeClose({ reason: "not_planned" }), "close as not planned");
});

test("the Inbox lists the last 7 days' dashboard closes, newest first, with any pending reopen", () => {
  const dc = (decision, at, undo_allowed) => ({ decision, reason: "completed", at, result: "closed", undo_allowed });
  const issues = [
    issue({ id: "I_1", state: "closed", dashboard_close: dc("D_1", "2026-10-22T08:00:00Z", true) }),
    issue({ id: "I_2", state: "closed", dashboard_close: dc("D_2", "2026-10-15T08:00:00Z", true) }),
    issue({ id: "I_3", state: "closed", dashboard_close: dc("D_3", "2026-10-23T07:00:00Z", false) }),
    issue({ id: "I_4", state: "closed", dashboard_close: null }),
    issue({ id: "I_5" }),
  ];
  const undo = { id: "D_u", created_at: "2026-10-23T08:10:00Z", action: "undo", target: { decision: "D_1", item: "I_1" } };
  const r = D.recentCloses(issues, [undo], [], now, 7);
  assert.deepEqual(r.map((x) => x.issue.id), ["I_3", "I_1"]);
  assert.equal(r[0].reopening, null);
  assert.equal(r[1].reopening.id, "D_u");
  assert.equal(D.recentCloses(issues, [undo], [{ decision: "D_u", result: "refused" }], now, 7)[1].reopening, null, "an acked undo is no longer pending");
});
