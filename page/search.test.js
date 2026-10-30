// node --test page/  — the search box: find an issue by number or title words.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const D = require("./core.js");

const repos = ["yuvalrakavy/Store", "rhaiscript/rhai", "yuvalrakavy/rhai"];
const issues = [
  { alias: "yuvalrakavy/Store#41", state: "open", title: "Timer drift on reload" },
  { alias: "yuvalrakavy/Store#410", state: "open", title: "Go back to rhai 1.27.0" },
  { alias: "yuvalrakavy/Store#416", state: "closed", title: "Bounded filesystem waits" },
  { alias: "rhaiscript/rhai#416", state: "open", title: "Closure capture panics" },
  { alias: "yuvalrakavy/Store#7", state: "open", title: "Scene 41 flickers" },
];
const aliases = (r) => r.items.map((i) => i.alias);

test("an empty or blank query is no search", () => {
  assert.deepEqual(D.search(issues, "", repos), { kind: "empty", items: [] });
  assert.deepEqual(D.search(issues, "   ", repos), { kind: "empty", items: [] });
});

test("a bare number matches the issue number exactly, never as a substring", () => {
  const r = D.search(issues, "41", repos);
  assert.equal(r.kind, "number");
  assert.equal(r.number, 41);
  assert.deepEqual(aliases(r), ["yuvalrakavy/Store#41"]);
});

test("#N is the same as N, and spaces around it are ignored", () => {
  assert.deepEqual(aliases(D.search(issues, " #41 ", repos)), ["yuvalrakavy/Store#41"]);
});

test("spaces around the # still make an exact number query", () => {
  assert.deepEqual(aliases(D.search(issues, "Store #41", repos)), ["yuvalrakavy/Store#41"]);
  assert.deepEqual(aliases(D.search(issues, "# 41", repos)), ["yuvalrakavy/Store#41"]);
  assert.deepEqual(aliases(D.search(issues, "store # 416", repos)), ["yuvalrakavy/Store#416"]);
});

test("a # with no number yet is no search", () => {
  assert.deepEqual(D.search(issues, "#", repos), { kind: "empty", items: [] });
  assert.deepEqual(D.search(issues, "Store#", repos), { kind: "empty", items: [] });
  assert.deepEqual(D.search(issues, "Store #", repos), { kind: "empty", items: [] });
});

test("a number with no repo finds it in every repo, open ones first", () => {
  assert.deepEqual(aliases(D.search(issues, "416", repos)), ["rhaiscript/rhai#416", "yuvalrakavy/Store#416"]);
});

test("a short repo prefix narrows to that repo, in any case", () => {
  assert.deepEqual(aliases(D.search(issues, "store#416", repos)), ["yuvalrakavy/Store#416"]);
  assert.deepEqual(aliases(D.search(issues, "Store#416", repos)), ["yuvalrakavy/Store#416"]);
});

test("a full owner/repo prefix matches only that repo", () => {
  assert.deepEqual(aliases(D.search(issues, "rhaiscript/rhai#416", repos)), ["rhaiscript/rhai#416"]);
  assert.deepEqual(aliases(D.search(issues, "yuvalrakavy/rhai#416", repos)), []);
});

test("a prefix that names one known repo resolves it for the GitHub link", () => {
  assert.equal(D.search(issues, "Store#999", repos).repo, "yuvalrakavy/Store");
  assert.deepEqual(aliases(D.search(issues, "Store#999", repos)), []);
});

test("a prefix shared by two repos, or no prefix, resolves no repo", () => {
  assert.equal(D.search(issues, "rhai#999", repos).repo, null);
  assert.equal(D.search(issues, "999", repos).repo, null);
  assert.equal(D.search(issues, "nosuch#1", repos).repo, null);
});

test("text matches when every word is in the title, in any order and case", () => {
  const r = D.search(issues, "WAITS bounded", repos);
  assert.equal(r.kind, "text");
  assert.deepEqual(aliases(r), ["yuvalrakavy/Store#416"]);
});

test("a word missing from the title excludes the issue", () => {
  assert.deepEqual(aliases(D.search(issues, "bounded timers", repos)), []);
});

test("text also matches the alias, so a repo name narrows a title search", () => {
  assert.deepEqual(aliases(D.search(issues, "rhai panics", repos)), ["rhaiscript/rhai#416"]);
});

test("a number with other words is a title search", () => {
  const r = D.search(issues, "scene 41", repos);
  assert.equal(r.kind, "text");
  assert.deepEqual(aliases(r), ["yuvalrakavy/Store#7"]);
});

test("text results put open issues before closed ones", () => {
  assert.deepEqual(aliases(D.search(issues, "store", repos)), ["yuvalrakavy/Store#41", "yuvalrakavy/Store#410", "yuvalrakavy/Store#7", "yuvalrakavy/Store#416"]);
});
