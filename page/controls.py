"""Negative controls for page/core.js: remove each guard, expect the suite to go RED.

Run: python3 page/controls.py (exit 0 when every mutant is RED)."""
import os, shutil, subprocess, sys, tempfile

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
MUTANTS = [
    ("generation name check", "!GENERATION.test(generation)", "false"),
    ("short collection retries", "data[c].length < Math.min(want, LIST_LIMIT)", "false"),
    ("missing marker retries", "if (!marker.exists) {", "if (false) {"),
    ("over-limit collection is partial", 'if (want !== null && want > LIST_LIMIT) why', "if (false) why"),
    ("exclude wins", "if (hits(view.exclude)) return false;", ""),
    ("empty include is every repo", "!(view.include && view.include.length) || ", ""),
    ("inbox item shown once", "!shown.has(i.id)", "true"),
    ("outside-only review waits", '(i.status === "needs-review" && i.outside === true) why', '(i.status === "needs-review") why'),
    ("undone decision skipped", "d.target.item !== item || s.undoneBy || s.settled) continue;", "d.target.item !== item || s.settled) continue;"),
    ("refused ack skipped", 'if (s.ack && s.ack.result !== "applied") continue;', ""),
    ("overdue run is null", "left <= 0 ? null :", ""),
    ("launch gate", 'if (gate !== "available") return { ok: false, gate };', ""),
    ("thread retry only for gone/full", 'if (!e || (e.code !== "not_found" && e.code !== "upstream_error")) throw e;', ""),
    ("page dismissal hides a suggestion", "if (gone.has(s.id)) continue;", ""),
    ("consumed dismissal defers to the tracker", "if (s.ack || s.undoneBy || s.settled) continue;", "if (s.undoneBy || s.settled) continue;"),
    ("undone dismissal not in force", "if (s.ack || s.undoneBy || s.settled) continue;", "if (s.ack || s.settled) continue;"),
    ("settled dismissal defers to the tracker", "if (s.ack || s.undoneBy || s.settled) continue;", "if (s.ack || s.undoneBy) continue;"),
    ("batch below two leaves", "if (members.length < 2) continue;", ""),
    ("suggestion out of view leaves", "if (!issue) continue;", "if (!issue) { cards.push({ s, issue: { alias: s.alias, id: s.item }, members: [] }); continue; }"),
    ("cross-repo duplicate not by number", "if (other && repoOf(other) === repoOf(s.alias)) return", "if (other) return"),
    ("launch needs a recipe", "if (!repo || !repo.recipe) return", "if (false) return"),
    ("batch is one repo", "if (new Set(issues.map((i) => repoOf(i.alias))).size !== 1) return", "if (false) return"),
    ("recommend only closable", "const items = issues.filter((i) => closable(i, repoFor(repoOf(i.alias))));", "const items = issues;"),
    ("dismissal hides a batch", "if (gone.has(s.id)) continue;", 'if (gone.has(s.id) && s.kind !== "batch") continue;'),
    ("dismissal hides a left-open entry", "if (gone.has(s.id)) continue;", 'if (gone.has(s.id) && s.kind !== "left_out") continue;'),
    ("dismissal names item and alias", "target: { suggestion: s.id, item: s.item, alias: s.alias }", "target: { suggestion: s.id }"),
    ("duplicate with no other says so", '(other || "another issue")', "other"),
    ("recommend carries routing", "recipe: READ_ONLY_RECIPE,\n        routing,", "recipe: READ_ONLY_RECIPE,\n        routing: null,"),
    ("heartbeat of another generation ignored", "heartbeat.generation === meta.generation && ", ""),
    ("heartbeat older than the generation ignored", "|| at > Date.parse(meta.taken_at));", "|| true);"),
    ("heartbeat moves each repo's read", "(reads.has(r.repo) ?", "(false ?"),
    ("show counts only open issues", 'if (i.state === "open") open[repoOf(i.alias)]', 'if (true) open[repoOf(i.alias)]'),
    ("show tells shared short names apart", "(shared.has(short(r)) ? r : short(r))", "short(r)"),
    ("show selects the isolated entry", "if (isolated && options.some((o) => o.value === isolated)) selected = isolated;", "if (false) selected = isolated;"),
    ("show never selects a missing entry", "if (isolated && options.some((o) => o.value === isolated)) selected", "if (isolated) selected"),
    ("show sorts groups ignoring case", "Object.keys(groups).sort((a, b) => a.toLowerCase().localeCompare(b.toLowerCase()))", "Object.keys(groups).sort()"),
    ("show marks a saved view", 'scopeName ? "View: " + scopeName : "Custom view"', '"Custom view"'),
    ("show ignores a scope with excludes", "scope && !(scope.exclude && scope.exclude.length) && ", "scope && "),
    ("next run counts from the heartbeat", "Date.parse(synced(meta, heartbeat).at)", "Date.parse(meta && meta.taken_at)"),
    ("triage: acted part hides at once", "if (!LABEL_KINDS.includes(s.kind) || gone.has(s.id)) continue;", "if (!LABEL_KINDS.includes(s.kind)) continue;"),
    ("triage: consumed accept defers to the tracker", "if (s.ack || s.settled || s.undoneBy) continue;", "if (s.settled || s.undoneBy) continue;"),
    ("triage: undone accept hides nothing", "if (s.ack || s.settled || s.undoneBy) continue;", "if (s.ack || s.settled) continue;"),
    ("triage: closing kinds are no triage card", "if (!LABEL_KINDS.includes(s.kind) || gone", "if (gone"),
    ("triage: closed issue has no card", "if (!shownIssue) continue;", "if (false) continue;"),
    ("triage: apply all needs every part", "c.parts.every((s) => s.bulk_ok === true)", "c.parts.some((s) => s.bulk_ok === true)"),
    ("triage: apply all never a second opinion (card)", "c.bulk_ok = !c.second && ", "c.bulk_ok = "),
    ("triage: apply all never a second opinion (fields)", "cards.filter((c) => c.bulk_ok && !c.second)", "cards.filter((c) => c.bulk_ok)"),
    ("triage: worst badge", "sort((a, b) => BADGES.indexOf(b) - BADGES.indexOf(a))[0]", "sort((a, b) => BADGES.indexOf(a) - BADGES.indexOf(b))[0]"),
    ("triage: lone part relabelled", 'c.label = lone ? "Set " + answerText(c.parts[0].answer) :', "c.label ="),
    ("triage: opinions after untriaged", "cards.sort((a, b) => (a.second - b.second)\n", "cards.sort((a, b) => 0\n"),
    ("triage: apply all marks bulk", "if (bulk) f.value = { bulk: true };", ""),
    ("triage: accept names card and component", "card: card.id, component: s.kind } };", "} };"),
    ("triage: rubric only past one level", "Math.abs(a - r) <= 1", "Math.abs(a - r) < 1"),
    ("triage: details edit names its suggestion", "const target = t && t.suggestion ?", "const target = false ?"),
    ("triage: opinions by rank", "(rank(a) - rank(b)) || ", ""),
    ("triage: opinions by disagreement", "|| (pCurrent(a) - pCurrent(b))", ""),
    ("triage: area edit is a set", "[...new Set([].concat(answer))].sort()", "[].concat(answer)"),
    ("triage: unmeasured false alarms said", 'o.false_alarm_rate == null ? "false-alarm rate not measured" :', ""),
    ("support question closes as not planned", 'if (s.kind === "support_question") return s.reply ? { reason: "not_planned", comment: s.reply } : { reason: "not_planned" };', ""),
    ("launch: triage live from the tracker's report", 'if (TRIAGE_KINDS.includes(s.kind) && LIVE_STATES.includes(s.state) && !(s.state === "requested" && notSent.has(s.launch)) && !unheard(s)) return true;', ""),
    ("launch: an ended triage is not live", 'LIVE_STATES.includes(s.state) && !(s.state', 'true && !(s.state'),
    ("launch: a refused send outranks the tracker's requested", ' && !(s.state === "requested" && notSent.has(s.launch)) && !unheard(s)', ""),
    ("launch: a read-only launch leaves the fix launch", "launchStates((launches || []).filter((l) => !READ_ONLY_KINDS.includes(l.kind)), statuses);", "launchStates(launches, statuses);"),
    ("launch: a page launch unheard for a day is not live", " && !unheard(s)) return true;", ") return true;"),
    ("launch: the cap follows the view", ".filter((id) => byId.has(id)).slice(0, max).map(", ".filter((id) => byId.has(id)).map("),
    ("launch: the alias counts as triage", 'const TRIAGE_KIND_LIST = ["triage", "recommend_closures"];', 'const TRIAGE_KIND_LIST = ["triage"];'),
    ("launch: a reported page launch defers to the tracker", "!status.has(l.id) && !notSent", "!notSent"),
    ("launch: a not-sent page launch is not live", "&& !status.has(l.id) && !notSent.has(l.id));", "&& !status.has(l.id));"),
    ("launch: triage names only open issues", '.filter((i) => i.state === "open")', ""),
    ("launch: triage document carries the scope", "...(fields.scope ? { scope: fields.scope } : {}),", ""),
    ("launch: no evaluation reads the measured backends", 'return !measured.includes(routing.model + "@" + routing.effort);', "return false;"),
    ("estimates: sites go with their commit", "if (!sites.length || !at) return", "if (false) return"),
    ("estimates: the commit is hex", "if (!/^[0-9a-f]{7,40}$/.test(at)) return", "if (false) return"),
    ("estimates: an empty edit saves nothing", 'if (!Object.keys(e).length) return { error: "Nothing to save." };', ""),
    ("estimates: the edit shows over the recorded value", "e.edited && e.edited[k] !== undefined ? e.edited[k] :", ""),
]

src = open(os.path.join(REPO, "page/core.js")).read()


def run(code):
    """Run every page test against `code` as core.js in a copy of the tree's layout."""
    root = tempfile.mkdtemp()
    d = os.path.join(root, "page"); os.mkdir(d)
    for shared in ("schemas", "crates"):
        os.symlink(os.path.join(REPO, shared), os.path.join(root, shared))
    for t in os.listdir(os.path.join(REPO, "page")):
        if t.endswith(".test.js") or t == "dashboard.html":
            shutil.copy(os.path.join(REPO, "page", t), d)
    open(os.path.join(d, "core.js"), "w").write(code)
    r = subprocess.run(["node", "--test"], cwd=d, capture_output=True, text=True)
    fails = [l for l in r.stdout.splitlines() if l.startswith("\u2139 fail")]
    return r.returncode != 0, (fails[0] if fails else r.stderr[-200:])


# The unmutated copy must pass, or every RED below proves nothing.
red, why = run(src)
if red:
    print("IDENTITY RED  " + why); sys.exit(1)
print("GREEN  identity")
bad = 0
for name, old, new in MUTANTS:
    n = src.count(old)
    if n != 1:
        print(f"SITE?  {name}: found {n} times"); bad += 1; continue
    red, why = run(src.replace(old, new))
    print(("RED    " if red else "GREEN  ") + name + "  " + why)
    bad += 0 if red else 1
sys.exit(1 if bad else 0)
