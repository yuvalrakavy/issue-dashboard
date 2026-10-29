// node --test page/  — triage cards on the page (triage spec 4.1-4.3): which type, area and
// priority cards the Inbox shows, accept / edit / apply all, and the Details edit.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const D = require("./core.js");

const who = { project: "VillaRakavy", author: "user_1" };
const now = new Date("2026-10-24T08:00:00Z");
const STORE = "yuvalrakavy/Store";
const issue = (n, more) => Object.assign({ id: "I_" + n, alias: STORE + "#" + n, kind: "issue", state: "open", title: "t" + n }, more);
const tg = (kind, n, more) => Object.assign({
  id: "sg-" + kind + n, project: "VillaRakavy", run_id: "L1", kind, made_at: "2026-10-23T10:00:00Z", confidence: 0.8,
  item: "I_" + n, alias: STORE + "#" + n, decision: kind, answer: kind === "area" ? ["kernel"] : kind === "type" ? "bug" : "p1",
  probabilities: {}, backend: "triage", evaluated: true, reason: "r", current: null, badge: "evaluated", bulk_ok: true, card: "sg-" + kind + n,
}, more);
const combined = (n, more) => [
  tg("type", n, Object.assign({ card: "cd-" + n, action_label: "Set bug · p1" }, more)),
  tg("priority", n, Object.assign({ card: "cd-" + n, action_label: "Set bug · p1" }, more)),
];

test("a combined card is one card; its badge is the worst part's and apply all needs every part", () => {
  const parts = combined(1);
  parts[1].badge = "unevaluated";
  parts[1].bulk_ok = false;
  const r = D.triageCards(parts.concat([tg("area", 2)]), [issue(1), issue(2)], [], []);
  assert.deepEqual(r.cards.map((c) => [c.id, c.parts.map((s) => s.kind), c.badge, c.bulk_ok, c.label]), [
    ["cd-1", ["type", "priority"], "unevaluated", false, "Set bug · p1"],
    ["sg-area2", ["area"], "evaluated", true, "Accept"],
  ]);
  assert.deepEqual(r.applyAll.map((c) => c.id), ["sg-area2"]);
});

test("closing kinds and out-of-view or closed issues are no triage card", () => {
  const r = D.triageCards([tg("obsolete", 1), tg("support_question", 1), tg("type", 2), tg("type", 3)], [issue(1), issue(2, { state: "closed" })], [], []);
  assert.deepEqual(r.cards, []);
});

test("second opinions come after untriaged cards and apply all never takes one", () => {
  const so = tg("type", 1, { made_at: "2026-10-22T10:00:00Z", current: "feature", second_opinion: { current: "feature", p_current: 0.1, false_alarm_rate: 0.108 } });
  const r = D.triageCards([so, tg("area", 2)], [issue(1), issue(2)], [], []);
  assert.deepEqual(r.cards.map((c) => [c.id, c.second, c.bulk_ok]), [["sg-area2", false, true], ["sg-type1", true, false]]);
  assert.deepEqual(D.applyAllFields(r.cards).map((f) => f.target.suggestion), ["sg-area2"]);
  assert.equal(D.secondOpinionNote(so), "Second opinion: feature is 10% likely · 11% false alarms");
  assert.equal(D.secondOpinionNote(tg("area", 3, { second_opinion: { current: ["kernel", "ui"], flagged: ["ui"], p_current: 0.05, false_alarm_rate: null } })),
    "Second opinion: ui is 5% likely · false-alarm rate not measured");
  assert.equal(D.secondOpinionNote(tg("area", 3)), null);
  const forged = Object.assign({}, r.cards[1], { bulk_ok: true });
  assert.deepEqual(D.applyAllFields([forged]), [], "apply all refuses a second opinion even when a card claims Bulk");
});

test("second opinions show priority, then type, then area, each by disagreement, whatever their age", () => {
  const so = (kind, n, p, made) => tg(kind, n, { made_at: made, current: "x", second_opinion: { current: "x", p_current: p, false_alarm_rate: 0.1 } });
  const r = D.triageCards([
    so("area", 1, 0.01, "2026-10-21T10:00:00Z"),
    so("type", 2, 0.15, "2026-10-21T10:00:00Z"),
    so("type", 3, 0.02, "2026-10-22T10:00:00Z"),
    so("priority", 4, 0.19, "2026-10-23T10:00:00Z"),
  ], [issue(1), issue(2), issue(3), issue(4)], [], []);
  assert.deepEqual(r.cards.map((c) => c.id), ["sg-priority4", "sg-type3", "sg-type2", "sg-area1"]);
});

test("accept names the card and component; apply all marks bulk; edit carries the value", () => {
  const card = D.triageCards(combined(1), [issue(1)], [], []).cards[0];
  assert.deepEqual(D.acceptCardFields(card, false), [
    { action: "accept_suggestion", target: { suggestion: "sg-type1", item: "I_1", alias: STORE + "#1", card: "cd-1", component: "type" } },
    { action: "accept_suggestion", target: { suggestion: "sg-priority1", item: "I_1", alias: STORE + "#1", card: "cd-1", component: "priority" } },
  ]);
  assert.deepEqual(D.applyAllFields([card]).map((f) => f.value), [{ bulk: true }, { bulk: true }]);
  assert.deepEqual(D.editFields(card, card.parts[0], "feature").value, { type: "feature" });
  assert.deepEqual(D.editFields(card, card.parts[1], "p3").value, { priority: "p3" });
  const area = D.triageCards([tg("area", 2)], [issue(2)], [], []).cards[0];
  assert.deepEqual(D.editFields(area, area.parts[0], ["ui", "kernel", "kernel"]).value, { areas: ["kernel", "ui"] }, "an area set is a set");
});

test("an accepted part hides at once; the combined card's other part shows alone, labelled for it", () => {
  const parts = combined(1);
  const card = D.triageCards(parts, [issue(1)], [], []).cards[0];
  const d = D.decisionDoc(D.acceptFields(card, parts[0], false), who, now, () => 0.5);
  let r = D.triageCards(parts, [issue(1)], [d], []);
  assert.deepEqual(r.cards.map((c) => [c.parts.map((s) => s.kind), c.label]), [[["priority"], "Set p1"]]);
  r = D.triageCards(parts, [issue(1)], [d], [{ decision: d.id, result: "refused" }]);
  assert.equal(r.cards[0].parts.length, 2, "once a cycle consumed it the tracker's documents are the word");
  const u = D.decisionDoc({ action: "undo", target: { decision: d.id, item: "I_1" } }, who, now, () => 0.6);
  assert.equal(D.triageCards(parts, [issue(1)], [d, u], []).cards[0].parts.length, 2, "an undone accept hides nothing");
});

test("a priority more than one level from the rubric adds a note", () => {
  assert.equal(D.rubricNote(tg("priority", 1, { answer: "p0", rubric_level: "p2" })), "The rubric gives p2.");
  assert.equal(D.rubricNote(tg("priority", 1, { answer: "p3", rubric_level: "p1" })), "The rubric gives p1.");
  assert.equal(D.rubricNote(tg("priority", 1, { answer: "p1", rubric_level: "p2" })), null);
  assert.equal(D.rubricNote(tg("priority", 1, { answer: "p1", rubric_level: null })), null);
  assert.equal(D.rubricNote(tg("type", 1, { rubric_level: "p3" })), null);
});

test("a Details edit names the issue's suggestion for that decision, or only the item", () => {
  const i = issue(1, { triage: { severity: { answer: "degraded", suggestion: "sg-sev1" }, reach: { answer: null, suggestion: null } } });
  assert.deepEqual(D.detailsEditFields(i, "severity", "wrong_result"), {
    action: "edit_suggestion", target: { suggestion: "sg-sev1", item: "I_1", alias: STORE + "#1" }, value: { severity: "wrong_result" },
  });
  assert.deepEqual(D.detailsEditFields(i, "reach", "most"), {
    action: "edit_suggestion", target: { item: "I_1", alias: STORE + "#1" }, value: { reach: "most" },
  });
  assert.deepEqual(D.detailsEditFields(issue(2), "severity", "degraded").target, { item: "I_2", alias: STORE + "#2" });
});

test("an estimates edit names only the item and carries the fields the owner set, cleaned", () => {
  const i = issue(1, { triage: { estimates: { tier: "T1", effort: "S", blast: "quick", sites: ["a.rs:1"], read_at: "abc1234", backend: "m@e", edited: null } } });
  assert.deepEqual(D.estimatesEditFields(i, { tier: "T2" }), {
    fields: { action: "edit_suggestion", target: { item: "I_1", alias: STORE + "#1" }, value: { estimates: { tier: "T2" } } },
  });
  assert.deepEqual(D.estimatesEditFields(i, { blast: "  the loader " }).fields.value, { estimates: { blast: "the loader" } });
  assert.deepEqual(D.estimatesEditFields(i, { sites: " src/a.rs:1, src/b.rs:2\nsrc/c.rs:3 ,", read_at: " DEF5678 " }).fields.value, {
    estimates: { sites: ["src/a.rs:1", "src/b.rs:2", "src/c.rs:3"], read_at: "def5678" },
  });
  assert.deepEqual(D.estimatesEditFields(issue(2), { effort: "L" }).fields.target, { item: "I_2", alias: STORE + "#2" });
  for (const [form, error] of [
    [{}, "Nothing to save."],
    [{ tier: "T4" }, "Tier is one of T0, T1, T2, T3."],
    [{ effort: "XL" }, "Effort is one of S, M, L."],
    [{ blast: "  " }, "Blast is empty."],
    [{ sites: "a.rs:1", read_at: "" }, "Sites need the commit they were read at."],
    [{ sites: " , ", read_at: "abc1234" }, "Sites need the commit they were read at."],
    [{ sites: "a.rs:1", read_at: "main" }, "The commit is 7 to 40 hex digits."],
  ]) assert.deepEqual(D.estimatesEditFields(i, form), { error }, JSON.stringify(form));
});

test("the estimates shown are the owner's edit over the recorded ones", () => {
  assert.equal(D.shownEstimates({}), null);
  assert.deepEqual(D.shownEstimates({ estimates: { tier: "T1", effort: "S", blast: "b", sites: ["a"], read_at: "abc1234", backend: "m", edited: null } }),
    { tier: "T1", effort: "S", blast: "b", sites: ["a"], read_at: "abc1234", edited: false });
  assert.deepEqual(D.shownEstimates({ estimates: { tier: "T1", effort: "S", blast: "b", sites: ["a"], read_at: "abc1234", backend: "m", edited: { effort: "L" } } }),
    { tier: "T1", effort: "L", blast: "b", sites: ["a"], read_at: "abc1234", edited: true });
  assert.deepEqual(D.shownEstimates({ estimates: { tier: null, effort: null, blast: null, sites: null, read_at: null, backend: null, edited: { tier: "T0" } } }),
    { tier: "T0", effort: null, blast: null, sites: null, read_at: null, edited: true });
});

test("the estimates editor starts from the full commit, so saving the sites keeps it", () => {
  const full = "def5678901234567890123456789012345678901";
  const f = D.estimatesFormValues({ tier: "T1", effort: "S", blast: "quick", sites: ["src/a.rs:1", "src/b.rs:2"], read_at: full, edited: false });
  assert.deepEqual(f, { blast: "quick", sites: "src/a.rs:1, src/b.rs:2", read_at: full });
  assert.equal(D.estimatesEditFields(issue(1), { sites: f.sites, read_at: f.read_at }).fields.value.estimates.read_at, full);
  assert.deepEqual(D.estimatesFormValues(null), { blast: "", sites: "", read_at: "" });
});

test("a support question is a closing card that closes as not planned", () => {
  const s = tg("support_question", 1);
  assert.deepEqual(D.suggestionCards([s], [issue(1)], [], []).cards.map((c) => c.s.kind), ["support_question"]);
  assert.deepEqual(D.closePrefill(s), { reason: "not_planned" });
});

test("every decision the triage cards write fits the page-decision schema's keys and enums", () => {
  const sc = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "schemas", "page-decision.schema.json"), "utf8"));
  const card = D.triageCards(combined(1), [issue(1)], [], []).cards[0];
  const i = issue(1, { triage: { severity: { suggestion: "sg-sev1" } } });
  const estimates = D.estimatesEditFields(i, { tier: "T2", effort: "M" }).fields;
  for (const f of [...D.applyAllFields([card]), D.editFields(card, card.parts[1], "p2"), D.detailsEditFields(i, "severity", "degraded"), estimates]) {
    const d = D.decisionDoc(f, who, now, () => 0.5);
    assert.ok(sc.properties.action.enum.includes(d.action), d.action);
    for (const k of Object.keys(d.target)) assert.ok(k in sc.properties.target.properties, "target has no " + k);
    for (const [k, v] of Object.entries(d.value)) {
      const p = sc.properties.value.properties[k];
      assert.ok(p, "value has no " + k);
      if (p.enum) assert.ok(p.enum.includes(v), k + " " + v);
    }
    if (d.target.component) assert.ok(sc.properties.target.properties.component.enum.includes(d.target.component));
  }
  // The estimates edit's own fields fit the shared definition the value refers to.
  const common = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "schemas", "common.schema.json"), "utf8"));
  assert.equal(sc.properties.value.properties.estimates.$ref, "common.schema.json#/$defs/estimatesEdit");
  const def = common.$defs.estimatesEdit.properties;
  for (const [k, v] of Object.entries(estimates.value.estimates)) {
    assert.ok(def[k], "estimates has no " + k);
    if (def[k].enum) assert.ok(def[k].enum.includes(v), k + " " + v);
  }
});

test("the Details estimates editor fills its inputs from estimatesFormValues, so the commit field keeps the full commit", () => {
  const html = fs.readFileSync(path.join(__dirname, "dashboard.html"), "utf8");
  assert.match(html, /const f = D\.estimatesFormValues\(v\);/);
  assert.match(html, /"Estimate blast", placeholder: "blast", value: f\.blast \}/);
  assert.match(html, /"Estimate sites", placeholder: "file:line, file:line", value: f\.sites \}/);
  assert.match(html, /"Estimate commit", class: "mono", placeholder: "commit", size: "9", value: f\.read_at \}/);
});
