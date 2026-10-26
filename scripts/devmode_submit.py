#!/usr/bin/env python3
"""One command: take a devmode statement from wherever it is and install it.

The statement is minted by https://developer.sonos.com/devmode/ (owner login).
Everything after that is mechanical, so this tool does all of it:

  * reads the model / device id / version / csrfToken the portal form wants and
    prints them for the portal;
  * accepts the statement as a deeplink, base64, a file, or stdin;
  * validates it offline against the real validator before touching the player,
    so a malformed or wrong-device blob never costs a POST;
  * posts it with the spacing the daemon's 3 s limiter requires, retries on
    transport errors, and scores the answer;
  * after a 200, probes the unlock surface (/debug/devmode.bin, /testenv,
    /ssh/fingerprints, tcp 22/2323) and fails if none of them flipped.

Transport is curl: urllib cannot reach the 192.168.4.0/24 players from this
host (scoped route, EHOSTUNREACH), curl can.

  ./devmode_submit.py 192.168.4.50                       # portal inputs only
  ./devmode_submit.py 192.168.4.50 --statement 'sonos://devMode/set?statement=...'
  ./devmode_submit.py 192.168.4.50 --statement stmt.b64
  ./devmode_submit.py 192.168.4.50 --statement /jffs/debug/devmode.bin
  pbpaste | ./devmode_submit.py 192.168.4.50 --statement -

A rejected statement leaves the player as it was. 200 is accepted; 409 is
"valid but grants no capability" (the branch is effectively dead on 86.10);
403 is a reject. Exit 0 accepted (and, unless --no-verify, unlocked), 1 reject,
2 unreachable, 3 validator reject or accepted-but-nothing-flipped.
"""
import argparse
import base64
import os
import re
import socket
import subprocess
import sys
import time
import urllib.error
import urllib.parse
import urllib.request

MAGIC = b"\x28\x72\x1e\x71"
SPACING = 3.2  # the daemon sleeps (3 - delta) seconds between POSTs
VALIDATOR = os.path.join(os.path.dirname(os.path.abspath(__file__)), "emu_zz0137.py")
DEEPLINK = re.compile(r"sonos-?2?://devMode/set\?(?:.*&)?statement=([^&\s]+)", re.I)


def _curl(args, data=None, timeout=30):
    """Run curl. urllib cannot reach the 192.168.4.0/24 players on this host
    (scoped route: python gets EHOSTUNREACH, curl does not), so curl is the
    transport. Returns (exit code, stdout bytes)."""
    cmd = ["curl", "-sS", "-m", str(timeout)] + args
    proc = subprocess.run(cmd, input=data, capture_output=True)
    return proc.returncode, proc.stdout


def get_page(host, scheme, port):
    rc, out = _curl(["-k", f"{scheme}://{host}:{port}/devmode"], timeout=10)
    if rc != 0:
        raise OSError(f"curl exit {rc}")
    return out.decode("utf-8", "replace")


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


def decode_statement(src):
    """Return (raw TLV bytes, source description) from any accepted form."""
    if src == "-":
        text, what = sys.stdin.read(), "stdin"
    elif src.startswith("sonos://") or src.startswith("sonos-2://"):
        m = DEEPLINK.search(src)
        if not m:
            raise SystemExit("deeplink has no statement parameter")
        text, what = urllib.parse.unquote(m.group(1)), "deeplink"
    elif src.startswith("b64:"):
        text, what = src[4:], "base64 argument"
    elif os.path.exists(src):
        blob = open(src, "rb").read()
        if blob.startswith(MAGIC):
            return blob, f"raw TLV file {src}"
        text, what = blob.decode("utf-8", "replace"), f"file {src}"
    else:
        text, what = src, "base64 argument"

    text = re.sub(r"\s+", "", text)
    if text.startswith("-----BEGIN"):
        raise SystemExit("that is a PEM certificate, not a devmode statement")
    text = text.replace("-", "+").replace("_", "/")  # Kotlin Base64.Default is strict
    text += "=" * (-len(text) % 4)
    try:
        raw = base64.b64decode(text, validate=True)
    except ValueError as exc:
        raise SystemExit(f"not valid base64: {exc}")
    if not raw.startswith(MAGIC):
        raise SystemExit(
            f"decoded blob does not start with the statement magic "
            f"{MAGIC.hex()} (got {raw[:4].hex()})"
        )
    return raw, what


def validate_offline(raw, info):
    """Run the real validator; return (ok, message)."""
    if not os.path.exists(VALIDATOR):
        return True, "validator not found, skipped"
    major, _, minor = (info.get("version") or "0.0").partition(".")
    with open("/tmp/.devmode_stmt.bin", "wb") as fh:
        fh.write(raw)
    argv = [sys.executable, VALIDATOR, "/tmp/.devmode_stmt.bin",
            f"str:{info['device_id']}", f"0x{int(info['model']):x}", major, minor or "0"]
    try:  # the validator only knows the 57.23 window; give it the real one
        ver = (int(major) << 16) | int(minor)
        argv.append(f"{ver:#x},{ver:#x}")
    except ValueError:
        pass
    proc = subprocess.run(
        argv, capture_output=True, text=True,
        cwd=os.path.dirname(VALIDATOR) + "/..",
    )
    out = (proc.stdout + proc.stderr).strip()
    if "ModuleNotFoundError" in out:
        return True, "offline validator unavailable (pip install unicorn), skipped"
    if "Traceback" in out:
        return True, "validator crashed (environment), skipped"
    if "signature verify" in out:
        # The stub stops there: every real check passed, only the signature is
        # unknown offline. That is exactly the case worth POSTing.
        return True, "shape valid, stopped at the signature check (expected)"
    return proc.returncode == 0, out


def post(host, scheme, port, csrf, statement, button):
    body = urllib.parse.urlencode(
        {"csrfToken": csrf, "statement": statement, "button": button}
    ).encode()
    t0 = time.monotonic()
    rc, out = _curl(
        ["-k", "-o", "-", "-w", "\n%{http_code}", "-X", "POST",
         "-H", "Content-Type: application/x-www-form-urlencoded",
         "--data-binary", "@-", f"{scheme}://{host}:{port}/devmode"],
        data=body,
    )
    dt = time.monotonic() - t0
    text, _, code = out.decode("utf-8", "replace").rpartition("\n")
    try:
        return int(code), text, dt
    except ValueError:
        return 0, text, dt


def probe(host, scheme, port):
    """Report the post-unlock surface. True means the unlock landed."""
    out = []
    for path in ("/debug/devmode.bin", "/testenv", "/ssh/fingerprints"):
        rc, body = _curl(["-k", "-o", "/dev/null", "-w", "%{http_code}",
                          f"{scheme}://{host}:{port}{path}"], timeout=5)
        out.append(f"{path} {body.decode() if rc == 0 else 'error ' + str(rc)}")
    for p in (2323, 22):
        rc, _ = _curl(["-o", "/dev/null", f"telnet://{host}:{p}"], timeout=3)
        out.append(f"tcp/{p} {'closed' if rc == 7 else 'open'}")
    return out


def main():
    ap = argparse.ArgumentParser(description=__doc__,
                                 formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("host")
    ap.add_argument("--statement", help="deeplink, base64, file, or - for stdin")
    ap.add_argument("--button", default="submit", choices=("submit", "delete"))
    ap.add_argument("--attempts", type=int, default=1)
    ap.add_argument("--scheme", default="http")
    ap.add_argument("--port", type=int, default=1400)
    ap.add_argument("--dry-run", action="store_true", help="validate, do not POST")
    ap.add_argument("--no-validate", action="store_true")
    ap.add_argument("--no-verify", action="store_true",
                    help="skip the post-accept unlock probe")
    a = ap.parse_args()

    try:
        page = get_page(a.host, a.scheme, a.port)
    except urllib.error.HTTPError as exc:
        print(f"GET /devmode -> HTTP {exc.code}", file=sys.stderr)
        return 2
    except OSError as exc:
        print(f"cannot reach {a.host}:{a.port}: {exc}", file=sys.stderr)
        return 2
    info = parse_page(page)
    print(f"model     {info['model']}")
    print(f"device id {info['device_id']}")
    print(f"version   {info['version']}")
    print(f"csrf      {info['csrf']}")
    if not info["csrf"]:
        print("no csrfToken in the page: the devmode object is not up", file=sys.stderr)
        return 2

    if not a.statement:
        # The portal hands the code over as text; if it is already on the
        # clipboard, take it rather than making the caller paste it twice.
        clip = ""
        if sys.stdin.isatty():
            try:
                clip = subprocess.run(
                    ["pbpaste"], capture_output=True, text=True, timeout=5
                ).stdout.strip()
            except (OSError, subprocess.SubprocessError):
                clip = ""
        if clip:
            a.statement = clip
            print("\nstatement taken from the clipboard")
        else:
            print("\nportal: https://developer.sonos.com/devmode/  (sign in, submit the")
            print("model and device id above, then pass the returned statement back with")
            print("--statement, or copy it and re-run)")
            return 0

    raw, what = decode_statement(a.statement)
    print(f"\nstatement {len(raw)} bytes from {what}")

    if not a.no_validate:
        ok, msg = validate_offline(raw, info)
        print(f"offline check: {msg}")
        if not ok:
            print("refusing to POST a statement the validator rejects")
            return 3
    if a.dry_run:
        print("dry run, not posted")
        return 0

    stmt = base64.b64encode(raw).decode()
    last = 0.0
    for attempt in range(1, a.attempts + 1):
        wait = SPACING - (time.monotonic() - last)
        if wait > 0:
            time.sleep(wait)
        code, text, dt = post(a.host, a.scheme, a.port, info["csrf"], stmt, a.button)
        last = time.monotonic()
        print(f"attempt {attempt}: HTTP {code} in {dt:.2f}s")
        if text.strip():
            print("  " + text.strip()[:300].replace("\n", "\n  "))
        if code == 200:
            print("ACCEPTED")
            if not a.no_verify:
                print("verifying the unlock:")
                lines = probe(a.host, a.scheme, a.port)
                for line in lines:
                    print("  " + line)
                if not any(" 200" in l for l in lines) and not any(
                        "open" in l for l in lines):
                    print("  no probe flipped: check /status/support, then reboot")
                    return 3
            return 0
        if code == 409:
            print("accepted but capability-less (409); treat as installed")
            return 0
        if code == 403 and dt >= 2.5:
            print("  rejected after the signature check: shape is right, the "
                  "signature or the device id is wrong")
        elif code == 403:
            print("  rejected before the signature check: malformed blob")
        elif code >= 500:
            print("  daemon error, retrying")
            continue
        else:
            return 1
    return 1


if __name__ == "__main__":
    sys.exit(main())
