#!/usr/bin/env python3
"""Replay a devmode statement at a player and report whether it took.

usage: devmode.py IP [STATEMENT_FILE]

The statement is a fixed blob (the parser only SHA-256s it and compares against a
compiled digest, no RSA), so one captured statement works on any device of that
line. GET /devmode yields Model, Device ID, Version and a csrfToken; the POST
needs `statement` (base64) and that token.

Exit codes: 0 accepted (port 22 open), 1 rejected, 2 transport error.
"""
import base64
import re
import socket
import sys
import urllib.parse
import urllib.request

ip = sys.argv[1]
path = sys.argv[2] if len(sys.argv) > 2 else None


def get(url, data=None, headers=None):
    req = urllib.request.Request(url, data=data, headers=headers or {})
    try:
        with urllib.request.urlopen(req, timeout=10) as r:
            return r.status, r.read().decode("latin1", "replace")
    except urllib.error.HTTPError as e:
        return e.code, e.read().decode("latin1", "replace")


def port_open(host, port=22, timeout=3):
    try:
        with socket.create_connection((host, port), timeout):
            return True
    except OSError:
        return False


status, body = get(f"http://{ip}:1400/devmode")
print(f"GET /devmode -> {status}")
for label, pat in (("Model", r"Model: ([^<\s]+)"), ("DeviceID", r"Device ID: ([^<\s]+)"), ("Version", r"Version: ([^<\s]+)")):
    m = re.search(pat, body)
    if m:
        print(f"  {label}: {m.group(1)}")
token = re.search(r'name="csrfToken" value="([^"]+)"', body)
if not token:
    print("no csrfToken in the GET body; cannot post")
    sys.exit(2)

if not path:
    print("no statement file given; GET only")
    sys.exit(2)

raw = open(path, "rb").read()
if b"BEGIN CERTIFICATE" in raw:
    raw = b"".join(raw.split())  # strip PEM armour/whitespace if present
stmt = base64.b64encode(raw).decode()
print(f"statement: {len(raw)} bytes -> {len(stmt)} b64 chars")

data = urllib.parse.urlencode({"statement": stmt, "csrfToken": token.group(1).strip()}).encode()
status, body = get(
    f"http://{ip}:1400/devmode",
    data=data,
    headers={"Content-Type": "application/x-www-form-urlencoded"},
)
print(f"POST /devmode -> {status}")
print(body[:400].strip())

if port_open(ip):
    print("port 22 OPEN - devmode accepted")
    sys.exit(0)
print("port 22 closed")
sys.exit(1 if status == 200 else 2)
