// node --test page/  — telling the owner when the tracker does not know their page user id yet
// (10.5): every action they take is refused until the project config's owner_id is theirs.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const D = require("./core.js");

test("a pending owner_id asks the owner to set theirs, with the exact config line", () => {
  const n = D.ownerNotice({ project: "villa-rakavy", owner_id: null }, "u_abc", true);
  assert.equal(n.kind, "pending");
  assert.equal(n.id, "u_abc");
  assert.equal(n.line, 'owner_id = "u_abc"');
  assert.equal(n.file, "projects/villa-rakavy.toml");
});

test("an owner_id that is someone else's says so", () => {
  const n = D.ownerNotice({ project: "p", owner_id: "u_other" }, "u_abc", true);
  assert.equal(n.kind, "mismatch");
  assert.equal(n.configured, "u_other");
  assert.equal(n.line, 'owner_id = "u_abc"');
});

test("nothing is said when the id matches, to a viewer who is not the owner, or without an id", () => {
  assert.equal(D.ownerNotice({ project: "p", owner_id: "u_abc" }, "u_abc", true), null);
  assert.equal(D.ownerNotice({ project: "p", owner_id: null }, "u_abc", false), null);
  assert.equal(D.ownerNotice({ project: "p", owner_id: null }, null, true), null);
  assert.equal(D.ownerNotice(null, "u_abc", true), null);
});

test("a generation from before meta carried owner_id says nothing", () => {
  assert.equal(D.ownerNotice({ project: "p" }, "u_abc", true), null);
});
