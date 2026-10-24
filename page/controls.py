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
]

src = open(os.path.join(REPO, "page/core.js")).read()


def run(code):
    """Run every page test against `code` as core.js in a copy of the tree's layout."""
    root = tempfile.mkdtemp()
    d = os.path.join(root, "page"); os.mkdir(d)
    for shared in ("schemas", "crates"):
        os.symlink(os.path.join(REPO, shared), os.path.join(root, shared))
    for t in os.listdir(os.path.join(REPO, "page")):
        if t.endswith(".test.js"):
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
