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

test("Recently closed lists every issue closed in the window, newest first, Reopen for the dashboard's own close and Dismiss for the rest", () => {
  const dc = (decision, at, undo_allowed) => ({ decision, reason: "completed", at, result: "closed", undo_allowed });
  const closed = (id, at, more) => issue(Object.assign({ id, alias: "yuvalrakavy/Store#" + id.slice(2), state: "closed", closed_at: at }, more));
  const issues = [
    closed("I_1", "2026-10-22T08:00:00Z", { dashboard_close: dc("D_1", "2026-10-22T08:00:00Z", true) }),
    closed("I_2", "2026-10-15T08:00:00Z", { dashboard_close: dc("D_2", "2026-10-15T08:00:00Z", true) }),
    closed("I_3", "2026-10-23T07:00:00Z", { dashboard_close: dc("D_3", "2026-10-23T07:00:00Z", false) }),
    closed("I_4", "2026-10-21T09:00:00Z", { state_reason: "not_planned", closed_by: ["yuvalrakavy/Store#40"] }),
    closed("I_6", "2026-10-16T08:14:06Z"),
    closed("I_9", "2026-10-16T08:14:04Z"),
    issue({ id: "I_5" }),
    closed("I_8", "2026-10-22T10:00:00Z", { kind: "pull_request", state: "merged" }),
  ];
  const undo = { id: "D_u", created_at: "2026-10-23T08:10:00Z", action: "undo", target: { decision: "D_1", item: "I_1" } };
  const r = D.recentlyClosed(issues, [undo], [], now, 7);
  assert.deepEqual(r.map((x) => x.issue.id), ["I_3", "I_1", "I_4", "I_6"], "inside 7 days only, issues only, newest first");
  assert.equal(r[1].reopening.id, "D_u");
  assert.equal(r[1].dismissible, false, "a dashboard close being reopened is not dismissed");
  assert.equal(r[0].dismissible, true, "a dashboard close past its Undo can be dismissed");
  assert.equal(r[2].dismissible, true);
  assert.equal(r[2].reason, "not planned");
  assert.deepEqual(r[2].prs, ["yuvalrakavy/Store#40"]);
  assert.equal(r[3].reason, null, "no reason when GitHub gave none");
  const fresh = D.recentlyClosed(issues, [undo], [{ decision: "D_u", result: "refused" }], now, 7);
  assert.equal(fresh[1].reopening, null, "an acked undo is no longer pending");
  assert.equal(fresh[1].dismissible, false, "Undo still allowed: Reopen, not Dismiss");
  assert.deepEqual(D.recentlyClosed(issues, [], [], now, 3).map((x) => x.issue.id), ["I_3", "I_1", "I_4"], "the window is the meta's days");
});

test("no Dismiss on an open issue, even one the dashboard is closing", () => {
  const c = { decision: "D_c", reason: "completed", at: "2026-10-23T08:00:00Z", result: "closed", undo_allowed: false };
  const r = D.recentlyClosed([issue({ dashboard_close: c })], [], [], now, 7);
  assert.equal(r.length, 1);
  assert.equal(r[0].dismissible, false);
  assert.equal(D.dismissed(issue({ dismissed_at: "2026-10-23T08:00:00Z" }), [], []), false, "an open item is never dismissed");
});

test("a dismissed issue leaves the list at once, comes back on Undo or refusal, and a later close shows anew", () => {
  const i = issue({ state: "closed", closed_at: "2026-10-22T08:00:00Z" });
  const f = D.dismissFields(i);
  const d = D.decisionDoc(f, who, now, () => 0.5);
  assert.deepEqual(d.target, { item: "I_7", alias: "yuvalrakavy/Store#7" });
  assert.deepEqual(d.value, { closed_at: "2026-10-22T08:00:00Z" });
  assert.ok(schema.properties.action.enum.includes("dismiss_closed"));
  for (const k of Object.keys(d.value)) assert.ok(k in schema.properties.value.properties, "value has no " + k);
  assert.equal(D.dismissed(i, [], []), false);
  assert.equal(D.dismissed(i, [d], []), true, "hidden before any cycle acks it");
  assert.equal(D.recentlyClosed([i], [d], [], now, 7).length, 0);
  assert.equal(D.dismissed(i, [d], [{ decision: d.id, result: "refused" }]), false, "a refused dismiss shows it again");
  const u = { id: "D_undo", action: "undo", target: { decision: d.id, item: "I_7" } };
  assert.equal(D.dismissed(Object.assign({}, i, { dismissed_at: "2026-10-23T08:00:00Z" }), [d, u], []), false, "Undo shows it at once");
  assert.equal(D.dismissed(i, [d, u], [{ decision: "D_undo", result: "refused" }]), true, "a refused undo leaves it dismissed");
  const u2 = { id: "D_redo", action: "undo", target: { decision: "D_undo", item: "I_7" } };
  assert.equal(D.dismissed(i, [d, u, u2], []), true, "an undo of the undo dismisses it again, as decision.rs resolves the chain");
  assert.equal(D.dismissed(i, [d, u, u2], [{ decision: d.id, result: "applied" }, { decision: "D_undo", result: "cancelled" }, { decision: "D_redo", result: "applied" }]), true);
  assert.equal(D.dismissed(i, [d], [{ decision: d.id, result: "cancelled" }]), false, "cancelled by an undo past the page's list: the tracker's word");
  assert.equal(D.dismissed(i, [d, u2], [{ decision: "D_undo", result: "cancelled" }]), true, "an undo cancelled by the tracker, itself past the list, takes nothing back");
  assert.equal(D.dismissed(Object.assign({}, i, { dismissed_at: "2026-10-23T08:00:00Z" }), [], []), true, "the tracker's word when the page holds none");
  const again = Object.assign({}, i, { closed_at: "2026-10-23T07:00:00Z", dismissed_at: null });
  assert.equal(D.dismissed(again, [d], []), false, "a dismissal of an earlier close does not hide a later one");
  assert.equal(D.dismissed(Object.assign({}, i, { closed_at: "2026-10-22T08:00:00.000Z" }), [d], []), true, "the same instant in another spelling");
});

test("Trends still counts a dismissed issue's close", () => {
  const i = issue({ state: "closed", created_at: "2026-10-01T00:00:00Z", closed_at: "2026-10-22T08:00:00Z", dismissed_at: "2026-10-23T08:00:00Z" });
  const t = D.trends([i], now, 4, {});
  assert.equal(t.weeks.reduce((n, w) => n + w.closed, 0), 1);
});
