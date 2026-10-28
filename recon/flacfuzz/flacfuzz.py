#!/usr/bin/env python3
"""Deterministic mutation sweep over crafted FLAC seeds, bead frankensonos-81v.
Runs /tmp/dec134 (1.3.4, ASan+UBSan, -fno-sanitize-recover) per case.
Class: OK / RC<nonzero> (sanitizer or crash) / TIMEOUT / STATE:<final>.
"""
import os
import subprocess
import sys

DEC = os.environ.get("DEC","/tmp/dec134")
OUT = os.environ.get("OUT","/tmp/flac81v_results.jsonl")


def mutate(data, rng):
    b = bytearray(data)
    if not b:
        return b"\x00"
    n = rng.randint(1, 8)
    for _ in range(n):
        op = rng.randint(0, 4)
        pos = rng.randrange(len(b))
        if op == 0:      # bit flip
            b[pos] ^= 1 << rng.randint(0, 7)
        elif op == 1:    # interesting byte
            b[pos] = rng.choice([0, 0xFF, 0x7F, 0x80, 1, 0xFE])
        elif op == 2:    # random byte
            b[pos] = rng.randint(0, 255)
        elif op == 3 and len(b) > 64:  # truncate (keep at least 1 byte)
            del b[pos + 1:]
        else:            # duplicate a run
            end = min(len(b), pos + rng.randint(1, 64))
            b[pos:pos] = b[pos:end]
    return bytes(b)


def main():
    import random
    seeds = [open(p, "rb").read() for p in sys.argv[1:]]
    rng = random.Random(1337)
    counts = {}
    with open(OUT, "w") as sink:
        for case in range(500):
            src = seeds[case % len(seeds)]
            data = mutate(src, rng)
            path = "/tmp/fc_%d.flac" % case
            with open(path, "wb") as f:
                f.write(data)
            try:
                r = subprocess.run([DEC, path], capture_output=True, timeout=10)
                rc, err = r.returncode, r.stderr.decode("latin-1")
            except subprocess.TimeoutExpired:
                rc, err = 124, ""
            if rc == 124:
                cls = "TIMEOUT"
            elif rc != 0 or "runtime error" in err or "AddressSanitizer" in err:
                cls = "BAD_rc%d" % rc
            else:
                cls = "OK"
            counts[cls] = counts.get(cls, 0) + 1
            interesting = cls != "OK" or "error cb" in err
            rec = {
                "case": case, "seed": case % len(seeds), "class": cls,
                "len": len(data),
                "err": err[-300:] if interesting else "",
            }
            sink.write(__import__("json").dumps(rec) + "\n")
            sink.flush()
            if interesting:
                if 'Sanitizer' in err or 'runtime error' in err: print(case, cls, err[-160:], flush=True)
            os.unlink(path)
    print(counts)


if __name__ == "__main__":
    main()
