#!/usr/bin/env python3
"""Is the published dashboard page the build on `main`?

Usage: python3 -I scripts/page-current.py SAVED_DIR [REV]

SAVED_DIR holds `index.html` and `core.js` as `Artifact` `read` saved them (`paths:
["index.html", "core.js"]`, `out_dir: SAVED_DIR`). Current when the saved `core.js` is
byte-identical to REV's `page/core.js` and REV's `page/dashboard.html` appears verbatim inside
the saved `index.html` (the app wraps the page in its own skeleton). REV defaults to `main`, so
a branch checked out in this tree does not change the answer.

Prints `current` (exit 0) or `stale: <which file differs>` (exit 1); exit 2 on a missing file.
"""
import os, subprocess, sys

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def at_rev(rev, path):
    return subprocess.run(["git", "show", f"{rev}:{path}"], cwd=REPO, capture_output=True,
                          text=True, check=True).stdout


def main():
    if len(sys.argv) not in (2, 3):
        sys.exit(__doc__)
    saved, rev = sys.argv[1], sys.argv[2] if len(sys.argv) == 3 else "main"
    try:
        live_core = open(os.path.join(saved, "core.js")).read()
        live_index = open(os.path.join(saved, "index.html")).read()
    except OSError as e:
        print(f"missing: {e}")
        return 2
    stale = []
    if live_core != at_rev(rev, "page/core.js"):
        stale.append("core.js")
    if at_rev(rev, "page/dashboard.html") not in live_index:
        stale.append("dashboard.html")
    print("stale: " + ", ".join(stale) if stale else "current")
    return 1 if stale else 0


if __name__ == "__main__":
    sys.exit(main())
