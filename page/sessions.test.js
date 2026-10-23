// node --test page/  — the sessions working on an item (2.3, 5.2a, 10.10): what the page says
// about each, in what order, and the "In progress" filter.
"use strict";
const test = require("node:test");
const assert = require("node:assert/strict");
const fs = require("node:fs");
const path = require("node:path");
const D = require("./core.js");

const now = new Date("2026-10-23T17:00:00Z");
const session = (more) => Object.assign({
  id: "local_1", title: "Fix gh #403", running: true, last_active_at: "2026-10-23T16:46:00Z",
  link: "claude://claude.ai/epitaxy/local_1", match: { kind: "exact", via: "launch" },
}, more);
const issue = (sessions, more) => Object.assign({ id: "I_1", alias: "yuvalrakavy/Store#403", kind: "issue", state: "open", status: "in-progress", sessions }, more);

test("each session says its title, running or idle, when it was last active, and how sure the match is", () => {
  const [a, b] = D.sessionLines(issue([
    session(),
    session({ id: "local_2", title: "Other", running: false, last_active_at: "2026-10-21T17:00:00Z", match: { kind: "probable", via: "title" } }),
  ]), now);
  assert.deepEqual(a, { id: "local_1", title: "Fix gh #403", state: "running", ago: "14 min ago", probable: false, link: "claude://claude.ai/epitaxy/local_1" });
  assert.equal(b.state, "idle");
  assert.equal(b.ago, "2 d ago");
  assert.equal(b.probable, true);
});

test("the ages read in minutes, hours and days, and a session active just now says so", () => {
  const ago = (at) => D.sessionLines(issue([session({ last_active_at: at })]), now)[0].ago;
  assert.equal(ago("2026-10-23T16:59:40Z"), "just now");
  assert.equal(ago("2026-10-23T17:00:30Z"), "just now", "a clock a little ahead is not a negative age");
  assert.equal(ago("2026-10-23T14:00:00Z"), "3 h ago");
  assert.equal(ago("2026-10-22T16:59:00Z"), "1 d ago");
});

test("an untitled session and one without a link still show", () => {
  const [s] = D.sessionLines(issue([session({ title: "", link: null })]), now);
  assert.equal(s.title, "Untitled session");
  assert.equal(s.link, null);
});

test("an item without sessions, from an older generation, shows none", () => {
  assert.deepEqual(D.sessionLines(issue(undefined), now), []);
  assert.deepEqual(D.sessionLines(issue([]), now), []);
});

test("the In progress filter keeps the items with a session or the in-progress status", () => {
  assert.equal(D.inProgress(issue([session()])), true);
  assert.equal(D.inProgress(issue([], { status: "in-progress" })), true, "the in_progress label without a session");
  assert.equal(D.inProgress(issue([], { status: "ready" })), false);
  assert.equal(D.inProgress(issue(undefined, { status: "ready" })), false);
  assert.equal(D.inProgress(issue([session()], { status: "checks-running", kind: "pull_request" })), true);
});

test("the schema's session fields are the ones the page reads", () => {
  const schema = JSON.parse(fs.readFileSync(path.join(__dirname, "..", "schemas", "doc-issue.schema.json"), "utf8"));
  const props = schema.properties.sessions.items.properties;
  for (const k of ["id", "title", "running", "last_active_at", "link", "match"]) assert.ok(k in props, "doc-issue sessions has no " + k);
  assert.deepEqual(props.match.properties.kind.enum.slice().sort(), ["exact", "probable"]);
});

test("the age changes unit exactly at an hour and at a day, and rounds down", () => {
  const ago = (mins) => D.sessionLines(issue([session({ last_active_at: new Date(now.getTime() - mins * 60000).toISOString() })]), now)[0].ago;
  assert.equal(ago(1), "1 min ago");
  assert.equal(ago(59), "59 min ago");
  assert.equal(ago(60), "1 h ago");
  assert.equal(ago(119), "1 h ago");
  assert.equal(ago(1439), "23 h ago");
  assert.equal(ago(1440), "1 d ago");
  assert.equal(ago(2159), "1 d ago");
  assert.equal(ago(2160), "1 d ago");
});

test("a session whose row has no link at all shows a null link", () => {
  const s = session();
  delete s.link;
  assert.equal(D.sessionLines(issue([s]), now)[0].link, null);
});
