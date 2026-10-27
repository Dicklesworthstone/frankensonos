#!/usr/bin/env python3
"""Run a DER corpus through the device libmbedx509 under qemu-ppc.

Runs INSIDE the fsonos-mbedtls-ppc container (needs qemu-ppc + /w/out/harness).

usage: run_corpus.py <corpusdir> <results.tsv> [--modes parse,nocopy,dn,info,verify]
Each case gets every mode; a mode that dies by signal is a hit. Timeout 5 s per run.
"""
import os
import subprocess
import sys

TIMEOUT = 5
QEMU = ["qemu-ppc", "-L", "/usr/powerpc-linux-gnu"]
HARNESS = "/w/out/harness"


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    corpus, results = sys.argv[1], sys.argv[2]
    modes = ["parse", "nocopy", "dn", "info", "verify"]
    ca = None
    if "--ca" in sys.argv:
        ca = sys.argv[sys.argv.index("--ca") + 1]
    if "--modes" in sys.argv:
        modes = sys.argv[sys.argv.index("--modes") + 1].split(",")

    cases = sorted(f for f in os.listdir(corpus) if f.endswith(".der"))
    notes = {}
    man = os.path.join(corpus, "manifest.tsv")
    if os.path.exists(man):
        for line in open(man):
            parts = line.rstrip("\n").split("\t")
            if len(parts) >= 3:
                notes[parts[0]] = (parts[1], parts[2])

    hits = 0
    with open(results, "w") as out:
        out.write("case\tmode\texit\tsignal\tfamily\tnote\n")
        for i, case in enumerate(cases):
            path = os.path.join(corpus, case)
            family, note = notes.get(case[:-4], ("?", ""))
            for mode in modes:
                try:
                    argv = QEMU + [HARNESS, mode, path]
                    if ca:
                        argv += [ca, "-"]
                    p = subprocess.run(argv,
                                       capture_output=True, timeout=TIMEOUT)
                    rc, sig = p.returncode, 0
                    err = p.stderr.decode("utf-8", "replace")
                    if "uncaught target signal" in err:
                        for tok in err.split():
                            if tok.isdigit():
                                sig = int(tok)
                                break
                except subprocess.TimeoutExpired:
                    rc, sig = 124, 0
                out.write(f"{case}\t{mode}\t{rc}\t{sig}\t{family}\t{note}\n")
                if sig or rc == 124:
                    hits += 1
                    print(f"HIT {case} {mode} rc={rc} sig={sig} {family} {note}", flush=True)
            if i % 200 == 0:
                print(f"... {i}/{len(cases)}", flush=True)
    print(f"done: {len(cases)} cases x {len(modes)} modes, {hits} hits")
    return 0


if __name__ == "__main__":
    sys.exit(main())
