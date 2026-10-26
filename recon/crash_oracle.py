#!/usr/bin/env python3
"""crash_oracle.py - a trustworthy down/up signal for a Sonos player.

Polls /status/zp every 100 ms and prints an exact timestamp for every transition, so "the daemon died"
becomes a fact with a time on it instead of an inference from a slow or missing answer.

Transport is `curl`, not a raw socket: on this workstation Python's `connect()` to 192.168.4.0/24
returns EHOSTUNREACH while curl and nc both connect (the subnet is reached over a scoped route). Using
curl keeps the oracle honest about what the daemon actually does.

/status/zp is the probe target because it is a registered route on both lines, it is cheap, and it
touches no audio - the player stays muted.

Every probe is classified. `refused`, `reset` and `timeout` all count as DOWN but stay distinguishable,
because a wedged daemon (accepts, never answers) is a different failure from a dead one.

Usage
  ./crash_oracle.py                          # 30 s, transitions printed live
  ./crash_oracle.py --until-down             # exit on the first DOWN
  ./crash_oracle.py --duration 120 --jsonl run.jsonl
  ./crash_oracle.py --host 192.168.4.202 --interval 0.1

Exit: 0 never down, 1 saw a down, 2 bad usage.

Run it in one pane, fire the attack in another, then read the timeline.

Measured 2026-10-26 on .202 (57.23): the URL-span overflow does NOT drop /status/zp. 112 consecutive
probes stayed 200 while N=5800 answered 408, and the daemon was still healthy afterwards (/status/zp 200,
/getaa 404). So a 000 or 408 on /getaa is a dropped request, not a daemon death - do not read it as one.
Point --path at the attacked route if the question is "is that route wedged" rather than "is the daemon
alive".
"""
import argparse
import json
import subprocess
import sys
import time
from datetime import datetime, timezone

DOWN = "DOWN"
UP = "UP"

# curl exit codes worth naming
CURL_REFUSED = 7
CURL_TIMEOUT = 28
CURL_EMPTY = 52
CURL_RECV = 56


def probe(host, port, path, timeout):
    """One classification. UP means a 200 status line; everything else is DOWN with a reason."""
    url = "http://%s:%d%s" % (host, port, path)
    t0 = time.monotonic()
    try:
        r = subprocess.run(
            ["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}",
             "--max-time", "%.2f" % timeout, "--connect-timeout", "%.2f" % timeout, url],
            capture_output=True, text=True, timeout=timeout + 1.0,
        )
    except subprocess.TimeoutExpired:
        return DOWN, "timeout", (time.monotonic() - t0) * 1000
    ms = (time.monotonic() - t0) * 1000
    if r.returncode == 0:
        code = r.stdout.strip()
        if code == "200":
            return UP, "200", ms
        return DOWN, "status:%s" % (code or "empty"), ms
    if r.returncode == CURL_REFUSED:
        return DOWN, "refused", ms
    if r.returncode == CURL_TIMEOUT:
        return DOWN, "timeout", ms
    if r.returncode in (CURL_EMPTY, CURL_RECV):
        return DOWN, "reset", ms
    return DOWN, "curlerr:%d" % r.returncode, ms


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="192.168.4.202")
    ap.add_argument("--port", type=int, default=1400)
    ap.add_argument("--path", default="/status/zp")
    ap.add_argument("--interval", type=float, default=0.1)
    ap.add_argument("--duration", type=float, default=30.0)
    ap.add_argument("--timeout", type=float, default=0.5)
    ap.add_argument("--until-down", action="store_true", help="exit on the first DOWN")
    ap.add_argument("--settle", type=float, default=0.0,
                    help="keep polling this long after the first DOWN, to time the recovery")
    ap.add_argument("--confirm", type=int, default=2,
                    help="consecutive failed probes before declaring DOWN (1 disables)")
    ap.add_argument("--jsonl", help="append one JSON object per transition")
    a = ap.parse_args()

    if a.timeout > a.interval * 6:
        ap.error("--timeout must stay within 6x --interval")

    out = open(a.jsonl, "a", buffering=1) if a.jsonl else None
    t0 = time.monotonic()
    wall0 = datetime.now(timezone.utc)
    state = None
    down_since = first_down = None
    downs = 0
    longest = 0.0
    samples = 0
    streak = 0
    reasons = {}

    def emit(kind, cls, detail, ms):
        rec = {
            "t": round(time.monotonic() - t0, 3),
            "wall": datetime.now(timezone.utc).isoformat(timespec="milliseconds"),
            "kind": kind, "state": cls, "detail": detail, "ms": round(ms, 1),
        }
        print(json.dumps(rec), flush=True)
        if out:
            out.write(json.dumps(rec) + "\n")

    print("# oracle start %s host=%s:%d path=%s interval=%.0fms"
          % (wall0.isoformat(timespec="milliseconds"), a.host, a.port, a.path, a.interval * 1000),
          flush=True)

    try:
        while True:
            now = time.monotonic()
            if now - t0 >= a.duration:
                break
            if first_down is not None and a.settle and now - first_down >= a.settle:
                break

            cls, detail, ms = probe(a.host, a.port, a.path, a.timeout)
            samples += 1
            reasons[detail] = reasons.get(detail, 0) + 1
            if cls == DOWN:
                streak += 1
            else:
                streak = 0
            # a single slow probe is not a crash: hold the state until the streak is long enough
            if cls == DOWN and state == UP and streak < a.confirm:
                delay = a.interval - (time.monotonic() - now)
                if delay > 0:
                    time.sleep(delay)
                continue

            if state is None:
                state = cls
                if cls == DOWN:
                    down_since = first_down = time.monotonic()
                    downs += 1
                emit("initial", cls, detail, ms)
            elif cls != state:
                if cls == DOWN and state == UP:
                    down_since = first_down = time.monotonic()
                    downs += 1
                    emit("DOWN", cls, detail, ms)
                elif cls == UP and state == DOWN:
                    if down_since is not None:
                        longest = max(longest, time.monotonic() - down_since)
                    down_since = None
                    emit("UP", cls, detail, ms)
                else:
                    emit("class-change", cls, detail, ms)
                state = cls

            if a.until_down and first_down is not None:
                break

            delay = a.interval - (time.monotonic() - now)
            if delay > 0:
                time.sleep(delay)
    except KeyboardInterrupt:
        print("# interrupted", flush=True)

    if down_since is not None:
        longest = max(longest, time.monotonic() - down_since)

    summary = {
        "samples": samples,
        "downs": downs,
        "first_down_wall": (
            datetime.fromtimestamp(wall0.timestamp() + (first_down - t0), timezone.utc)
            .isoformat(timespec="milliseconds") if first_down is not None else None),
        "longest_outage_s": round(longest, 3),
        "recovered": bool(downs) and state == UP,
        "final_state": state,
        "reasons": reasons,
    }
    print("# SUMMARY " + json.dumps(summary), flush=True)
    if out:
        out.write(json.dumps({"summary": summary}) + "\n")
        out.close()
    return 1 if downs else 0


if __name__ == "__main__":
    sys.exit(main())
