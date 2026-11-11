#!/usr/bin/env python3
"""List the guard sites a diff adds, so a mutant list is derived, not recalled.

Usage: python3 scripts/mutant-sites.py BASE [--check MUTANTS.json]

A site is an added production line (Rust under crates/*/src, the page's core.js and
dashboard.html; never tests) holding a condition, a comparison, a fallback or a refusal:
`if`, `&&`, `||`, a spaced comparison, `.or(`, `.find(`, `.filter(`, `.then_with(`, `Err(`,
`bail!`. Each is a place a mutant can flip. Several sites on one line are one mutant's worth:
cover the line.

--check: also read MUTANTS.json (the `scripts/mutate.py` format) and report every site no
mutant covers. A site is covered when a mutant of the same file has a `find` that occurs in
the site's line (or the line occurs in a multi-line `find`), or when the file's optional
top-level "skip" list names it: [{"file": ..., "find": ..., "why": ...}]. Exit 1 when any
site is uncovered, so the gaps are decided, not missed.

Why (retro 2026-11-11): the author's own 8 mutants of harness phase 1 were all RED, yet a
mutation finder then found 10 untested guards, mostly refusals added late in the build.
"""
import argparse, json, os, re, subprocess, sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
PROD = re.compile(r"^(crates/[^/]+/src/.*\.rs|page/core\.js|page/dashboard\.html)$")
# Comparisons only when spaced (` < `, ` >= `): unspaced `<`/`>` are generics and arrows.
GUARD = re.compile(
    r"\bif\b|&&|\|\||\s(<|>|<=|>=|==|!=|===|!==)\s|\.or\(|\.or_else\(|\.find\(|\.filter\("
    r"|\.then_with\(|\bErr\(|\bbail!"
)
COMMENT = re.compile(r"^\s*(//|#|\*|/\*)")


def sites(base):
    diff = subprocess.run(
        ["git", "diff", "-U0", "--no-color", base, "--"], cwd=REPO, capture_output=True, text=True, check=True
    ).stdout
    out, path, line = [], None, 0
    for d in diff.splitlines():
        if d.startswith("+++ "):
            p = d[6:] if d.startswith("+++ b/") else None
            path = p if p and PROD.match(p) else None
        elif d.startswith("@@") and path:
            line = int(re.search(r"\+(\d+)", d).group(1))
        elif path and d.startswith("+") and not d.startswith("+++"):
            text = d[1:]
            if not COMMENT.match(text) and GUARD.search(text):
                out.append((path, line, text.strip()))
            line += 1
    return out


def covered(site, entries):
    path, _, text = site
    for e in entries:
        f = e.get("find", "")
        if e.get("file") == path and f and (f in text or text in f):
            return True
    return False


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("base")
    ap.add_argument("--check")
    a = ap.parse_args()
    found = sites(a.base)
    if not a.check:
        for p, n, t in found:
            print(f"{p}:{n}: {t}")
        print(f"{len(found)} sites", file=sys.stderr)
        return 0
    spec = json.load(open(a.check))
    entries = spec.get("mutants", []) + spec.get("skip", [])
    missing = [s for s in found if not covered(s, entries)]
    for p, n, t in missing:
        print(f"UNCOVERED {p}:{n}: {t}")
    print(f"{len(found) - len(missing)} of {len(found)} sites covered", file=sys.stderr)
    return 1 if missing else 0


if __name__ == "__main__":
    sys.exit(main())
