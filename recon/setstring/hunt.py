#!/usr/bin/env python3
"""One /setstring write with a fresh CSRF token, then a liveness watch.

The token contains '+' and '/', so it must be URL-encoded in the body; a raw
'+' decodes to a space and the handler answers 400. The body must be >= 200
bytes (the reader rejects shorter), so a short value is padded with a dummy
field only when the value itself is short — but unknown fields are rejected,
so pad the *value* instead.

    hunt.py --ip 192.168.4.202 --key BootSeq --value 1 --observe 80
    hunt.py --ip 192.168.4.202 --key 'a<b' --value x --pad 200

Appends one TSV row to keys.tsv: ts, key, value_len, post_code, first_down,
recovered, note.
"""
import argparse
import re
import subprocess
import time
import urllib.parse

TSV = "keys.tsv"


def curl(args, timeout=8):
    return subprocess.run(["curl", "-s", "-m", str(timeout)] + args,
                          capture_output=True, text=True)


def token(ip):
    r = curl([f"http://{ip}:1400/setstring"])
    m = re.search(r'name="csrfToken" value="([^"]*)"', r.stdout)
    return m.group(1) if m else None


def post(ip, key, value):
    tok = token(ip)
    if tok is None:
        return 0
    body = "csrfToken=%s&key=%s&value=%s" % (
        urllib.parse.quote(tok, safe=""),
        urllib.parse.quote(key, safe=""),
        urllib.parse.quote(value, safe=""),
    )
    r = curl(["-o", "/dev/null", "-w", "%{http_code}", "-X", "POST",
              "-H", "Content-Type: application/x-www-form-urlencoded",
              "--data-binary", body, f"http://{ip}:1400/setstring"], timeout=20)
    return int(r.stdout.strip() or 0)


def up(ip):
    return curl(["-o", "/dev/null", "-w", "%{http_code}",
                 f"http://{ip}:1400/status/VERSION"], timeout=3).stdout.strip() == "200"


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--ip", required=True)
    ap.add_argument("--key", required=True)
    ap.add_argument("--value", default="x")
    ap.add_argument("--pad", type=int, default=0,
                    help="pad the value with 'A' to at least this many chars")
    ap.add_argument("--observe", type=int, default=80)
    ap.add_argument("--settle", type=int, default=0,
                    help="seconds to wait before the write")
    a = ap.parse_args()

    if a.settle:
        time.sleep(a.settle)
    if not up(a.ip):
        print("unit not answering before the write")
        return 2

    value = a.value
    if a.pad and len(value) < a.pad:
        value = value + "A" * (a.pad - len(value))

    code = post(a.ip, a.key, value)
    print(f"POST {code} key={a.key!r} value_len={len(value)}")

    t0 = time.time()
    first_down = recovered = None
    last = None
    while time.time() - t0 < a.observe:
        time.sleep(2)
        ok = up(a.ip)
        if ok != last:
            print(f"  {time.time()-t0:5.1f}s {'UP' if ok else 'DOWN'}")
            last = ok
        if not ok and first_down is None:
            first_down = time.time() - t0
        if first_down is not None and ok and recovered is None:
            recovered = time.time() - t0 - first_down
            break

    note = "" if first_down is None else "KILL"
    with open(TSV, "a") as f:
        f.write("%s\t%s\t%d\t%d\t%s\t%s\t%s\n" % (
            time.strftime("%Y-%m-%dT%H:%M:%S"), a.key, len(value), code,
            "-" if first_down is None else "%.1f" % first_down,
            "-" if recovered is None else "%.1f" % recovered, note))
    print(f"verdict first_down={first_down} recovered={recovered}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
