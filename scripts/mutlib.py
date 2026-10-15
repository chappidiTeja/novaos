#!/usr/bin/env python3
"""Mutation testing: put a deliberate bug into a scratch copy of the project and require
the tests to notice. A test suite that stays green with a bug in the code proves nothing.

Each mutation is a list of edits (file, old text, new text; the old text must occur exactly
once), the make target that should fail, and a text the failing output must contain, so a
test that fails for an unrelated reason (a typo in the mutation, a timeout) is not counted.

Verdicts:
  CAUGHT      the target failed and showed the expected text
  CAUGHT?     the target failed, but not in the way expected (look at it)
  SURVIVED    the target still passed: the tests have a hole
  INVALID     the mutated code did not compile (the mutation itself is wrong)

Usage from a spec file:  mutlib.run(__file__-relative project root, work dir, [Mutation, ...])
"""

import os
import re
import signal
import shutil
import subprocess
import sys
import time

IGNORE = shutil.ignore_patterns(".git", "iso_root", "serial.log", "*.iso")


class Mutation:
    def __init__(self, name, edits, target, expect=None, timeout=900):
        self.name = name
        self.edits = edits  # [(relative path, old, new)]
        self.target = target
        self.expect = expect
        self.timeout = timeout


def make(work, target, timeout):
    """Run `make target`; on a timeout kill the whole process group (make starts others)."""
    p = subprocess.Popen(["make", target], cwd=work, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
                         text=True, errors="replace", start_new_session=True)
    try:
        out, _ = p.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        os.killpg(p.pid, signal.SIGKILL)
        p.communicate()
        raise
    return p.returncode, out


def compile_failed(log):
    return re.search(r"^error(\[E\d+\])?:", log, re.M) is not None or "could not compile" in log


def first_failures(log, n=3):
    keys = ("FAIL", "panicked", "FAILED", "failed")
    lines = [l.strip() for l in log.splitlines() if any(k in l for k in keys)]
    return lines[:n]


def check(src, mutations):
    """Only verify that every edit's old text occurs exactly once (no building)."""
    bad = 0
    for m in mutations:
        for rel, old, new in m.edits:
            n = open(os.path.join(src, rel), newline="").read().count(old)
            if n != 1 or old == new:
                print(f"BAD EDIT in '{m.name}': '{old[:70]}' occurs {n} times in {rel}")
                bad += 1
    print(f"{len(mutations)} mutations, {bad} bad edit(s)")
    return 1 if bad else 0


def main(src, work, mutations):
    """Command line for a spec: [--check] [--target MAKE_TARGET] [name-substring ...]."""
    args = sys.argv[1:]
    if "--check" in args:
        return check(src, mutations)
    target = None
    if "--target" in args:
        i = args.index("--target")
        target = args[i + 1]
        del args[i:i + 2]
    names = [a for a in args if not a.startswith("-")]
    chosen = [m for m in mutations if (not names or any(n in m.name for n in names)) and (target is None or m.target == target)]
    return run(src, work, chosen)


def run(src, work, mutations, baseline_targets=None):
    src, work = os.path.abspath(src), os.path.abspath(work)
    if os.path.exists(work):
        shutil.rmtree(work)
    shutil.copytree(src, work, ignore=IGNORE, symlinks=True)

    targets = baseline_targets or sorted({m.target for m in mutations})
    for t in targets:
        status, log = make(work, t, 1800)
        if status != 0:
            print(f"BASELINE FAILED: make {t} does not pass without any mutation; fix that first")
            for l in first_failures(log, 5):
                print("   ", l)
            return 2
        print(f"baseline ok: make {t}", flush=True)

    results = []
    for m in mutations:
        originals = {}
        started = time.time()
        try:
            for rel, old, new in m.edits:
                path = os.path.join(work, rel)
                text = originals.setdefault(rel, open(path, newline="").read())
                current = open(path, newline="").read()
                if current.count(old) != 1:
                    raise SystemExit(f"{m.name}: '{old[:60]}' occurs {current.count(old)} times in {rel}, wanted exactly 1")
                open(path, "w", newline="").write(current.replace(old, new))
            status, log = make(work, m.target, m.timeout)
        except subprocess.TimeoutExpired:
            status, log = 1, "TIMEOUT"
        finally:
            for rel, text in originals.items():
                open(os.path.join(work, rel), "w", newline="").write(text)
        secs = int(time.time() - started)
        if compile_failed(log):
            verdict = "INVALID"
        elif status == 0:
            verdict = "SURVIVED"
        elif m.expect is None or m.expect in log:
            verdict = "CAUGHT"
        else:
            verdict = "CAUGHT?"
        results.append((verdict, m.name))
        print(f"{verdict:9} {m.name}   ({secs} s)", flush=True)
        if verdict in ("SURVIVED", "INVALID", "CAUGHT?"):
            for l in (first_failures(log) if verdict != "INVALID" else [l for l in log.splitlines() if l.startswith("error")][:3]):
                print("          ", l[:160])

    bad = [r for r in results if r[0] != "CAUGHT"]
    print(f"\n{len(results) - len(bad)} of {len(results)} injected bugs caught")
    for v, n in bad:
        print(f"   {v}: {n}")
    return 1 if bad else 0


if __name__ == "__main__":
    sys.exit("mutlib is a library: run a spec such as scripts/mutations_net.py")
