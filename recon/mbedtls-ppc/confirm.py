#!/usr/bin/env python3
"""Re-run every hit from a results file N times and report only reproducible ones.

usage: confirm.py <results.tsv> [--n 5] [--ca <ca.der>]
A hit is a row with signal != 0 or exit 124 (timeout).
"""
import os
import subprocess
import sys

QEMU = ["qemu-ppc", "-L", "/usr/powerpc-linux-gnu"]
HARNESS = "/w/out/harness"


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    results = sys.argv[1]
    n = int(sys.argv[sys.argv.index("--n") + 1]) if "--n" in sys.argv else 5
    ca = sys.argv[sys.argv.index("--ca") + 1] if "--ca" in sys.argv else None

    hits = []
    for line in open(results):
        f = line.rstrip("\n").split("\t")
        if len(f) < 6 or f[0] == "case":
            continue
        if int(f[3] or 0) != 0 or int(f[2] or 0) == 124:
            hits.append(f)

    print(f"{len(hits)} raw hits, {n} attempts each")
    stable = 0
    for case, mode, rc, sig, family, note in hits:
        path = os.path.join(os.path.dirname(results), "..", "corpus", case)
        if not os.path.exists(path):
            path = os.path.join("/w/corpus", case)
        argv = QEMU + [HARNESS, mode, path]
        if ca:
            argv += [ca, "-"]
        seen = []
        for _ in range(n):
            try:
                p = subprocess.run(argv, capture_output=True, timeout=5)
                r = p.returncode
            except subprocess.TimeoutExpired:
                r = 124
            seen.append(r)
        bad = sum(1 for r in seen if r < 0 or r == 124)
        tag = "STABLE" if bad == n else ("FLAKY" if bad else "GONE")
        if bad == n:
            stable += 1
        print(f"{tag} {case} {mode} {seen} {family} {note}")
    print(f"stable: {stable}/{len(hits)}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
