// node --test page/  — the status strip's Show drop-down (5.6).
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const D = require("./core.js");

const meta = {
  repos: [{ repo: "yuvalrakavy/Store" }, { repo: "yuvalrakavy/mqtt_dali" }, { repo: "rhaiscript/rhai" }, { repo: "yuvalrakavy/airzoned" }],
  groups: { bridges: ["yuvalrakavy/mqtt_dali", "yuvalrakavy/airzoned"], core: ["yuvalrakavy/Store"] },
};
const issues = [
  { alias: "yuvalrakavy/Store#1", state: "open" },
  { alias: "yuvalrakavy/Store#2", state: "open" },
  { alias: "yuvalrakavy/Store#3", state: "closed" },
  { alias: "yuvalrakavy/mqtt_dali#4", state: "open" },
];

test("Show lists All, then groups, then repos, each with its open count", () => {
  const r = D.showOptions(meta, issues, null, null);
  assert.equal(r.selected, "all");
  assert.deepEqual(r.options, [
    { value: "all", label: "All repos" },
    { value: "group:bridges", label: "bridges (1)", heading: "Groups" },
    { value: "group:core", label: "core (2)", heading: "Groups" },
    { value: "repo:yuvalrakavy/airzoned", label: "airzoned (0)", heading: "Repos" },
    { value: "repo:yuvalrakavy/mqtt_dali", label: "mqtt_dali (1)", heading: "Repos" },
    { value: "repo:rhaiscript/rhai", label: "rhai (0)", heading: "Repos" },
    { value: "repo:yuvalrakavy/Store", label: "Store (2)", heading: "Repos" },
  ]);
});

test("Show selects the isolated repo or group", () => {
  assert.equal(D.showOptions(meta, issues, { include: [{ repo: "yuvalrakavy/Store" }], exclude: [] }, null).selected, "repo:yuvalrakavy/Store");
  assert.equal(D.showOptions(meta, issues, { include: [{ group: "bridges" }], exclude: [] }, null).selected, "group:bridges");
});

test("a saved view in force is shown as its own first entry", () => {
  const r = D.showOptions(meta, issues, { include: [{ repo: "yuvalrakavy/Store" }], exclude: [] }, "Store only");
  assert.equal(r.selected, "view");
  assert.deepEqual(r.options[0], { value: "view", label: "View: Store only" });
  assert.equal(r.options[1].value, "all");
});

test("a scope that is not one repo or group is shown as a view", () => {
  const r = D.showOptions(meta, issues, { include: [], exclude: [{ group: "bridges" }] }, null);
  assert.equal(r.selected, "view");
  assert.deepEqual(r.options[0], { value: "view", label: "Custom view" });
  const both = D.showOptions(meta, issues, { include: [{ group: "bridges" }], exclude: [{ repo: "yuvalrakavy/airzoned" }] }, null);
  assert.equal(both.selected, "view");
});

test("an isolated repo with no entry of its own is shown as a view, never as a blank", () => {
  const r = D.showOptions(meta, issues, { include: [{ repo: "yuvalrakavy/gone" }], exclude: [] }, null);
  assert.equal(r.selected, "view");
  assert.deepEqual(r.options[0], { value: "view", label: "Custom view" });
});

test("groups sort like repos, ignoring case", () => {
  const m = { repos: [], groups: { Zeta: [], alpha: [] } };
  assert.deepEqual(D.showOptions(m, [], null, null).options.map((o) => o.label), ["All repos", "alpha (0)", "Zeta (0)"]);
});

test("repos that share a short name are told apart by owner", () => {
  const m = { repos: [{ repo: "yuvalrakavy/rhai" }, { repo: "rhaiscript/rhai" }], groups: {} };
  const labels = D.showOptions(m, [], null, null).options.map((o) => o.label);
  assert.deepEqual(labels, ["All repos", "rhaiscript/rhai (0)", "yuvalrakavy/rhai (0)"]);
});

test("Show parses its values back into an isolate request", () => {
  assert.deepEqual(D.showChoice("repo:yuvalrakavy/Store"), { kind: "repo", name: "yuvalrakavy/Store" });
  assert.deepEqual(D.showChoice("group:bridges"), { kind: "group", name: "bridges" });
  assert.deepEqual(D.showChoice("all"), { kind: "all" });
  assert.equal(D.showChoice("view"), null);
});
