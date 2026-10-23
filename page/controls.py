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
    ("undone decision skipped", "|| s.undoneBy || s.settled) continue;", "|| s.settled) continue;"),
    ("refused ack skipped", 'if (s.ack && s.ack.result !== "applied") continue;', ""),
    ("overdue run is null", "left <= 0 ? null :", ""),
    ("launch gate", 'if (gate !== "available") return { ok: false, gate };', ""),
    ("thread retry only for gone/full", 'if (!e || (e.code !== "not_found" && e.code !== "upstream_error")) throw e;', ""),
    ("page dismissal hides a suggestion", "if (gone.has(s.id)) continue;", ""),
    ("refused dismissal not in force", 'if (r === "refused" || r === "cancelled" || undone.has(d.id)) continue;', 'if (r === "cancelled" || undone.has(d.id)) continue;'),
    ("undone dismissal not in force", 'if (r === "refused" || r === "cancelled" || undone.has(d.id)) continue;', 'if (r === "refused" || r === "cancelled") continue;'),
    ("batch below two leaves", "if (members.length < 2) continue;", ""),
    ("suggestion out of view leaves", "if (!issue) continue;", "if (!issue) { cards.push({ s, issue: { alias: s.alias, id: s.item }, members: [] }); continue; }"),
    ("cross-repo duplicate not by number", "if (other && repoOf(other) === repoOf(s.alias)) return", "if (other) return"),
    ("launch needs a recipe", "if (!repo || !repo.recipe) return", "if (false) return"),
    ("batch is one repo", "if (new Set(issues.map((i) => repoOf(i.alias))).size !== 1) return", "if (false) return"),
    ("recommend only closable", "const items = issues.filter((i) => closable(i, repoFor(repoOf(i.alias))));", "const items = issues;"),
]

src = open(os.path.join(REPO, "page/core.js")).read()
bad = 0
for name, old, new in MUTANTS:
    n = src.count(old)
    if n != 1:
        print(f"SITE?  {name}: found {n} times"); bad += 1; continue
    root = tempfile.mkdtemp()
    d = os.path.join(root, "page"); os.mkdir(d)
    os.symlink(os.path.join(REPO, "schemas"), os.path.join(root, "schemas"))
    for t in os.listdir(os.path.join(REPO, "page")):
        if t.endswith(".test.js"):
            shutil.copy(os.path.join(REPO, "page", t), d)
    open(os.path.join(d, "core.js"), "w").write(src.replace(old, new))
    r = subprocess.run(["node", "--test"], cwd=d, capture_output=True, text=True)
    fails = [l for l in r.stdout.splitlines() if l.startswith("ℹ fail")]
    red = r.returncode != 0
    print(("RED    " if red else "GREEN  ") + name + "  " + (fails[0] if fails else r.stderr[-200:]))
    bad += 0 if red else 1
sys.exit(1 if bad else 0)
