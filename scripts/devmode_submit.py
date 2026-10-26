#!/usr/bin/env python3
"""Submit a devmode statement to a player, or just read the portal inputs.

The statement is minted by https://developer.sonos.com/devmode/ (owner login).
This tool does the device half: it reads the model / device id / version /
csrfToken the portal form wants, and posts a statement with the spacing the
daemon's 4 s limiter requires.

  ./devmode_submit.py 192.168.4.50                 # print the portal inputs
  ./devmode_submit.py 192.168.4.50 --statement f   # post the base64 in file f
  ./devmode_submit.py 192.168.4.50 --statement -   # ... read it from stdin

The response is scored, not just printed: a parser reject answers in ~0.6-1.0 s
while a run that reaches the signature check takes 2.7-5.7 s, so the elapsed
time says whether the blob got past the parser. 200 means accepted; 409 means
already unlocked; 403 is a reject.

Nothing here is destructive: a rejected statement leaves the player as it was.
"""
import argparse
import re
import sys
import time
import urllib.parse
import urllib.request

SPACING = 5.5  # the daemon sleeps ~2.1 s per missed 4 s interval


def get_page(host):
    url = f"http://{host}/devmode"
    with urllib.request.urlopen(url, timeout=10) as r:
        return r.read().decode("utf-8", "replace")


def parse_page(html):
    def one(label):
        m = re.search(label + r":\s*([^<\s]+)", html)
        return m.group(1) if m else None

    tok = re.search(r'name="csrfToken" value="([^"]+)"', html)
    return {
        "model": one("Model"),
        "device_id": one("Device ID"),
        "version": one("Version"),
        "csrf": tok.group(1) if tok else None,
    }


def post(host, csrf, statement, button="submit"):
    body = urllib.parse.urlencode(
        {"csrfToken": csrf, "statement": statement, "button": button}
    ).encode()
    req = urllib.request.Request(
        f"http://{host}/devmode",
        data=body,
        headers={"Content-Type": "application/x-www-form-urlencoded"},
    )
    t0 = time.monotonic()
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            code, text = r.status, r.read().decode("utf-8", "replace")
    except urllib.error.HTTPError as e:
        code, text = e.code, e.read().decode("utf-8", "replace")
    return code, text, time.monotonic() - t0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("host")
    ap.add_argument("--statement", help="file with the base64 statement, or -")
    ap.add_argument("--button", default="submit", choices=("submit", "delete"))
    a = ap.parse_args()

    info = parse_page(get_page(a.host))
    print(f"model     {info['model']}")
    print(f"device id {info['device_id']}")
    print(f"version   {info['version']}")
    print(f"csrf      {info['csrf']}")
    print()
    print("portal: https://developer.sonos.com/devmode/  (sign in, then submit")
    print("the model and device id above; copy the returned base64 statement)")

    if not a.statement:
        return 0

    if a.statement == "-":
        stmt = sys.stdin.read()
    else:
        stmt = open(a.statement).read()
    stmt = stmt.strip().replace("\n", "").replace("\r", "")
    if not stmt:
        print("empty statement", file=sys.stderr)
        return 2
    print(f"\nstatement {len(stmt)} base64 chars")

    code, text, dt = post(a.host, info["csrf"], stmt, a.button)
    print(f"HTTP {code} in {dt:.2f}s")
    if text.strip():
        print(text[:400])
    if code == 200:
        print("ACCEPTED")
    elif code == 409:
        print("already unlocked")
    elif dt >= 2.5:
        print("rejected, but the slow path ran: the parser and the signature "
              "check both executed, so the shape is right and only the "
              "signature failed")
    else:
        print("rejected before the signature check: the blob is malformed")
    return 0 if code in (200, 409) else 1


if __name__ == "__main__":
    sys.exit(main())
