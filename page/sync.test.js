// node --test page/  — sync requests from the page (7.2, 10.8): the Refresh of one item and the
// strip's Sync now, as `requests` documents the next run serves.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const D = require("./core.js");

const schema = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "schemas", "page-request.schema.json"), "utf8"));
const who = { project: "VillaRakavy", author: "user_1" };
const now = new Date("2026-10-23T18:12:05.123Z");
const issue = { id: "I_403", alias: "yuvalrakavy/Store#403" };

test("a Refresh names the one item, and Sync now names none", () => {
  const one = D.requestDoc(D.syncFields(issue), who, now, () => 0.5);
  assert.equal(one.kind, "sync");
  assert.deepEqual(one.items, ["I_403"]);
  const all = D.requestDoc(D.syncFields(null), who, now, () => 0.5);
  assert.equal(all.kind, "sync");
  assert.equal("items" in all, false, "a global request has no items, never an empty list");
  for (const d of [one, all]) {
    for (const k of Object.keys(d)) assert.ok(k in schema.properties, "page-request has no " + k);
    for (const k of schema.required) assert.ok(k in d, "missing " + k);
  }
  assert.ok(schema.properties.kind.enum.includes("sync"));
});

test("the note says the next run serves it, within the sync interval when known", () => {
  assert.equal(D.syncNote(issue, 5), "Refresh of yuvalrakavy/Store#403 requested: the next run reads it within 5 minutes.");
  assert.equal(D.syncNote(null, 1), "Sync requested: the next run reads it within 1 minute.");
  assert.equal(D.syncNote(null, null), "Sync requested: the next run reads it at the next sync.");
});
