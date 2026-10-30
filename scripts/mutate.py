#!/usr/bin/env python3
"""Negative controls: break each guard, expect its tests to go RED.

Usage: python3 scripts/mutate.py MUTANTS.json [--jobs N] [--budget SECONDS] [--rev REV]

MUTANTS.json:
  {
    "timeout": 300,                       # per test run, seconds (default 300)
    "test": ["node", "--test", "page/"],  # default test command for every mutant
    "mutants": [
      {"name": "exact number", "file": "page/core.js",
       "find": "=== number", "replace": "!== number",
       "test": ["node", "--test", "page/search.test.js"]}   # optional, overrides "test"
    ]
  }

Each mutant replaces `find` (which must occur exactly once) with `replace` and runs its test
command with a timeout. RED (the command fails) means the tests catch the mutant. The tests
run in throwaway git worktrees cut from REV (default HEAD), so only committed code is
mutated. Before any mutant, every distinct test command must pass on the unmutated tree: a
harness that is red already reports every mutant RED. A Rust mutant should name a targeted
command, for example ["cargo", "nextest", "run", "-E", "test(ledger::)"], never the whole
suite, and share the build: CARGO_TARGET_DIR=$PWD/target with --jobs 1 rebuilds only the
mutated crate (about 100 s for the identity run plus one mutant), where a fresh worktree's own
target dir builds every dependency first. Mutants not started when --budget runs out are
reported UNRUN.

Exit 0 only when the identity runs pass and every mutant is RED.
"""
import argparse, json, os, queue, shutil, subprocess, sys, tempfile, threading, time

REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))


def run(cmd, cwd, timeout):
    try:
        p = subprocess.run(cmd, cwd=cwd, capture_output=True, text=True, timeout=timeout)
        return "pass" if p.returncode == 0 else "fail", p.stdout[-2000:] + p.stderr[-2000:]
    except subprocess.TimeoutExpired:
        return "timeout", ""


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("mutants")
    ap.add_argument("--jobs", type=int, default=1)
    ap.add_argument("--budget", type=int, default=600, help="seconds; default 600")
    ap.add_argument("--rev", default="HEAD")
    a = ap.parse_args()
    spec = json.load(open(a.mutants))
    timeout = spec.get("timeout", 300)
    mutants = spec["mutants"]
    for m in mutants:
        m.setdefault("test", spec.get("test"))
        if not m["test"]:
            sys.exit(f"mutant {m['name']!r} has no test command")
    deadline = time.time() + a.budget

    base = tempfile.mkdtemp(prefix="mutate-")
    trees = []
    try:
        for k in range(max(1, a.jobs)):
            t = os.path.join(base, f"w{k}")
            subprocess.run(["git", "worktree", "add", "-q", "--detach", t, a.rev], cwd=REPO, check=True)
            trees.append(t)

        # Each mutant's find text must occur exactly once at REV.
        for m in mutants:
            n = open(os.path.join(trees[0], m["file"])).read().count(m["find"])
            if n != 1:
                sys.exit(f"mutant {m['name']!r}: find text occurs {n} times in {m['file']}, need 1")

        # Identity: every distinct test command passes unmutated.
        for cmd in {tuple(m["test"]) for m in mutants}:
            status, out = run(list(cmd), trees[0], timeout)
            print(f"identity {status.upper():7} {' '.join(cmd)}")
            if status != "pass":
                print(out)
                print("identity run is not green: the harness cannot tell a mutant from the baseline")
                return 2

        work = queue.Queue()
        for m in mutants:
            work.put(m)
        results = {}

        def worker(tree):
            while True:
                try:
                    m = work.get_nowait()
                except queue.Empty:
                    return
                if time.time() > deadline:
                    results[m["name"]] = "unrun"
                    continue
                path = os.path.join(tree, m["file"])
                src = open(path).read()
                open(path, "w").write(src.replace(m["find"], m["replace"]))
                try:
                    status, _ = run(m["test"], tree, timeout)
                finally:
                    open(path, "w").write(src)
                results[m["name"]] = {"fail": "red", "pass": "green", "timeout": "timeout"}[status]

        threads = [threading.Thread(target=worker, args=(t,)) for t in trees]
        for t in threads:
            t.start()
        for t in threads:
            t.join()
    finally:
        for t in trees:
            subprocess.run(["git", "worktree", "remove", "--force", t], cwd=REPO, capture_output=True)
        shutil.rmtree(base, ignore_errors=True)

    for m in mutants:
        print(f"{results[m['name']].upper():7} {m['name']}")
    counts = {s: list(results.values()).count(s) for s in ("red", "green", "timeout", "unrun")}
    print(", ".join(f"{v} {k}" for k, v in counts.items() if v))
    return 0 if counts["red"] == len(mutants) else 1


if __name__ == "__main__":
    sys.exit(main())
