// node --test page/  — closing recommendations on the page (5.8): which suggestion cards the
// Inbox shows, Dismiss, the close sheet each one opens, and the launches the page writes.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const D = require("./core.js");

const schema = (n) => JSON.parse(fs.readFileSync(path.join(__dirname, "..", "schemas", n + ".schema.json"), "utf8"));
const who = { project: "VillaRakavy", author: "user_1" };
const now = new Date("2026-10-24T08:00:00Z");
const STORE = "yuvalrakavy/Store";
const issue = (n, more) => Object.assign({ id: "I_" + n, alias: STORE + "#" + n, kind: "issue", state: "open", title: "t" + n }, more);
const repo = (more) => Object.assign({ repo: STORE, read_only: { value: false, why: null }, visibility: "public", recipe: "worktree-pr", campaign_skill: true }, more);
const repoFor = (r) => (r === STORE ? repo() : r === "yuvalrakavy/ro" ? repo({ repo: r, read_only: { value: true, why: "declared" } }) : undefined);
const sg = (kind, n, more) => Object.assign({
  id: "sg-" + kind + n, project: "VillaRakavy", run_id: "L1", kind, made_at: "2026-10-23T10:00:00Z", confidence: 0.8,
  item: "I_" + n, alias: STORE + "#" + n, decision: kind, answer: true, probabilities: { true: 0.8, false: 0.2 },
  backend: "recommend_closures", evaluated: false, reason: "r",
}, more);

test("a suggestion fixture has every field the tracker's document requires", () => {
  const s = schema("doc-suggestion");
  const body = schema("common").$defs.suggestionBody;
  for (const k of s.required.concat(body.required)) assert.ok(k in sg("obsolete", 1), "fixture lacks " + k);
});

test("each kind's card, oldest first; left_out goes to its own list", () => {
  const issues = [issue(1), issue(2), issue(3), issue(4)];
  const r = D.suggestionCards([
    sg("obsolete", 1, { made_at: "2026-10-23T11:00:00Z" }),
    sg("likely_fixed", 2, { commit: "abc1234" }),
    sg("left_out", 3, { why: "needs_design" }),
    sg("type", 4),
  ], issues, [], []);
  assert.deepEqual(r.cards.map((c) => c.s.kind), ["likely_fixed", "obsolete"]);
  assert.deepEqual(r.leftOut.map((c) => c.issue.id), ["I_3"]);
  assert.ok(!r.cards.some((c) => c.s.kind === "type"), "triage kinds are not closing cards");
});

test("an issue out of view or closed since has no card", () => {
  const r = D.suggestionCards([sg("obsolete", 1), sg("obsolete", 2), sg("left_out", 3)], [issue(1, { state: "closed" }), issue(3, { state: "closed" })], [], []);
  assert.deepEqual(r.cards, []);
  assert.deepEqual(r.leftOut, []);
});

test("a batch keeps its members in view and open, and leaves below two", () => {
  const b = sg("batch", 1, { members: ["I_1", "I_2", "I_3"], member_aliases: [STORE + "#1", STORE + "#2", STORE + "#3"], surface: "mqtt" });
  let r = D.suggestionCards([b], [issue(1), issue(2, { state: "closed" }), issue(3)], [], []);
  assert.deepEqual(r.cards[0].members.map((i) => i.id), ["I_1", "I_3"]);
  r = D.suggestionCards([b], [issue(1), issue(2, { state: "closed" })], [], []);
  assert.deepEqual(r.cards, [], "one open member is not a batch");
  r = D.suggestionCards([b], [issue(2), issue(3)], [], []);
  assert.equal(r.cards[0].issue.id, "I_2", "the card's issue is the first member still shown");
});

test("Dismiss hides the card at once; a refused, cancelled or undone dismissal does not", () => {
  const s = sg("obsolete", 1);
  const d = D.decisionDoc(D.dismissSuggestionFields(s), who, now, () => 0.5);
  const issues = [issue(1)];
  assert.deepEqual(D.suggestionCards([s], issues, [d], []).cards, []);
  for (const result of ["refused", "cancelled"]) {
    assert.equal(D.suggestionCards([s], issues, [d], [{ decision: d.id, result }]).cards.length, 1, result);
  }
  // Once a cycle consumed it, the tracker's documents decide: a suggestion it still publishes
  // was lifted (the issue changed since), so it shows again under the same id.
  assert.equal(D.suggestionCards([s], issues, [d], [{ decision: d.id, result: "applied" }]).cards.length, 1);
  assert.equal(D.suggestionCards([s], issues, [d], [], "2026-10-25T00:00:00Z").cards.length, 1, "a dismissal past the ack horizon is the tracker's");
  assert.equal(D.suggestionCards([s], issues, [d], [], "2026-10-01T00:00:00Z").cards.length, 0, "a pending one inside the horizon hides");
  const undo = D.decisionDoc({ action: "undo", target: { decision: d.id, item: "I_1" } }, who, now, () => 0.6);
  assert.equal(D.suggestionCards([s], issues, [d, undo], []).cards.length, 1);
  const other = D.decisionDoc(D.dismissSuggestionFields(sg("obsolete", 2)), who, now, () => 0.7);
  assert.equal(D.suggestionCards([s], issues, [other], []).cards.length, 1, "a dismissal is of one suggestion");
});

test("a dismissal fits page-decision and names the suggestion", () => {
  const d = D.decisionDoc(D.dismissSuggestionFields(sg("obsolete", 1)), who, now, () => 0.5);
  const sc = schema("page-decision");
  assert.ok(sc.properties.action.enum.includes(d.action));
  for (const k of Object.keys(d.target)) assert.ok(k in sc.properties.target.properties, "target has no " + k);
  assert.equal(d.target.suggestion, "sg-obsolete1");
});

test("each close sheet opens as its kind says, and the prefill makes a valid close", () => {
  const i = issue(7);
  const cases = [
    [sg("likely_fixed", 7, { commit: "abc1234" }), "completed", /fixed by abc1234/],
    [sg("obsolete", 7), "not_planned", null],
    [sg("duplicate", 7, { related: "I_3", related_alias: STORE + "#3" }), "duplicate", /duplicate of #3/],
    [sg("duplicate", 7, { related: "I_9", related_alias: "yuvalrakavy/other#3" }), "not_planned", /duplicate of yuvalrakavy\/other#3/],
  ];
  for (const [s, reason, comment] of cases) {
    const p = D.closePrefill(s);
    assert.equal(p.reason, reason, s.kind);
    const r = D.closeFields(i, p.reason, p.duplicateOf, p.comment || D.closeComment(p.reason, p.duplicateOf));
    assert.equal(r.error, undefined, s.kind + ": " + r.error);
    if (comment) assert.match(r.fields.value.close.comment, comment);
    if (reason === "duplicate") assert.equal(r.fields.value.close.duplicate_of, 3);
  }
  assert.equal(D.closePrefill(sg("batch", 7)), null);
});

test("a fix or batch launch goes through the repo's recipe", () => {
  const routing = { model: "claude-opus-5-5", effort: "high" };
  let r = D.fixLaunchFields([issue(1)], repo(), routing);
  assert.deepEqual(r.fields, { kind: "fix", items: ["I_1"], aliases: [STORE + "#1"], recipe: "worktree-pr", routing });
  r = D.fixLaunchFields([issue(1), issue(2)], repo(), routing);
  assert.equal(r.fields.kind, "batch");
  assert.match(D.fixLaunchFields([issue(1)], repo({ recipe: null }), routing).error, /no launch recipe/);
  assert.match(D.fixLaunchFields([issue(1)], undefined, routing).error, /no launch recipe/);
  assert.match(D.fixLaunchFields([issue(1), issue(2, { alias: "yuvalrakavy/other#2" })], repo(), routing).error, /one repo/);
  const doc = D.launchDoc(D.fixLaunchFields([issue(1), issue(2)], repo(), routing).fields, who, now, () => 0.5);
  const sc = schema("page-launch").oneOf[0];
  for (const k of Object.keys(doc)) assert.ok(k in sc.properties, "page-launch has no " + k);
  assert.ok(sc.properties.kind.enum.includes(doc.kind));
});

test("Recommend closures covers every closable issue in view, read-only", () => {
  const routing = { model: "claude-opus-5-5", effort: "medium" };
  const issues = [issue(1), issue(2, { state: "closed" }), issue(3, { kind: "pull_request" }), issue(4, { alias: "yuvalrakavy/ro#4" }), issue(5, { alias: "yuvalrakavy/unknown#5" }), issue(6)];
  const r = D.recommendFields(issues, repoFor, routing);
  assert.deepEqual(r.fields.items, ["I_1", "I_6"]);
  assert.deepEqual(r.fields.aliases, [STORE + "#1", STORE + "#6"]);
  assert.equal(r.fields.kind, "recommend_closures");
  assert.equal(r.fields.recipe, "read-only");
  assert.ok(schema("page-launch").oneOf[0].properties.kind.enum.includes("recommend_closures"));
  assert.match(D.recommendFields([issue(2, { state: "closed" })], repoFor, routing).error, /No open issue/);
  assert.deepEqual(D.recommendFields([issue(1)], repoFor, routing).fields.routing, routing);
});

test("Dismiss hides a batch and a left-open entry too, and names the item and alias", () => {
  const b = sg("batch", 1, { members: ["I_1", "I_2"], member_aliases: [STORE + "#1", STORE + "#2"], surface: "mqtt" });
  const l = sg("left_out", 3, { why: "keep_open" });
  const issues = [issue(1), issue(2), issue(3)];
  const db = D.decisionDoc(D.dismissSuggestionFields(b), who, now, () => 0.5);
  const dl = D.decisionDoc(D.dismissSuggestionFields(l), who, now, () => 0.6);
  assert.equal(D.suggestionCards([b, l], issues, [], []).cards.length, 1);
  assert.deepEqual(D.suggestionCards([b, l], issues, [db], []).cards, []);
  assert.deepEqual(D.suggestionCards([b, l], issues, [dl], []).leftOut, []);
  assert.deepEqual(D.dismissSuggestionFields(l).target, { suggestion: "sg-left_out3", item: "I_3", alias: STORE + "#3" });
});

test("a duplicate with no other issue named closes as not planned, saying so", () => {
  assert.deepEqual(D.closePrefill(sg("duplicate", 7)), { reason: "not_planned", comment: "Closed from the issue dashboard as a duplicate of another issue." });
});
