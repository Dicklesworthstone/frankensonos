#!/usr/bin/env python3
"""Rebuild a Sonos .upd with a new preinstall script and a corrected digest.

The type-17 signature is left as-is (it will not verify - that needs a vendor
key), so the gate is framing + both digests. The script section is re-wrapped
in the 886499ca envelope to the recipient already used by the base package.

Usage:
  build_upd.py BASE.upd --script-file S --key PRIVATE.pem --out OUT.upd
"""
import hashlib
import os
import struct
import subprocess
import sys
import tempfile

MAGIC = b"\x49\x7f\x16\x35"
ENV = bytes.fromhex("886499ca")


def sections(d):
    o = 0
    while o < len(d):
        if d[o:o + 4] != MAGIC:
            raise SystemExit("bad magic at 0x%x" % o)
        t, tot, _ = struct.unpack_from("<III", d, o + 4)
        yield t, o, tot
        o += tot


def envelope(payload, key, recipient):
    with tempfile.TemporaryDirectory() as td:
        aes = os.urandom(16)
        iv = os.urandom(16)
        inp = os.path.join(td, "p")
        open(inp, "wb").write(payload)
        wrapped = subprocess.run(
            ["openssl", "pkeyutl", "-encrypt", "-pubin", "-inkey", key,
             "-pkeyopt", "rsa_padding_mode:oaep", "-pkeyopt", "rsa_oaep_md:sha1",
             "-in", "/dev/stdin"],
            input=aes, capture_output=True, check=True).stdout
        ct = subprocess.run(
            ["openssl", "aes-128-cbc", "-e", "-K", aes.hex(), "-iv", iv.hex(),
             "-in", inp], capture_output=True, check=True).stdout
    body = (struct.pack(">I", 0x10000) + b"\0" * 8
            + struct.pack("<I", 1) + bytes([len(recipient), 3]) + recipient
            + struct.pack("<I", 1) + struct.pack(">I", len(wrapped)) + wrapped
            + struct.pack("<I", 1) + struct.pack(">I", len(iv) + len(ct)) + iv + ct)
    return ENV + struct.pack(">I", 0x14 + len(body)) + body


def main():
    a = sys.argv[1:]
    if not a or "--script-file" not in a:
        sys.exit(__doc__)
    base, script = a[0], a[a.index("--script-file") + 1]
    key = a[a.index("--key") + 1] if "--key" in a else "/tmp/model16.pem"
    out = a[a.index("--out") + 1] if "--out" in a else base + ".new"

    d = open(base, "rb").read()
    anchor_pub = os.path.splitext(key)[0] + ".pem"

    secs = list(sections(d))
    t3 = next((o, tot) for t, o, tot in secs if t == 3)
    recipient = None
    for t, o, tot in secs:
        if t in (3, 4, 6):
            p = d[o + 16:o + tot]
            if p[:4] == ENV:
                recipient = p[0x16:0x16 + p[0x14]]
            break
    if recipient is None:
        raise SystemExit("no envelope to copy the recipient from")

    new = envelope(open(script, "rb").read(), anchor_pub if os.path.exists(anchor_pub) else key, recipient)
    o, tot = t3
    rebuilt = d[:o] + MAGIC + struct.pack("<III", 3, 16 + len(new), 4) + new + d[o + tot:]

    # fix the type-17 digest over the new prefix
    for t, so, _ in sections(rebuilt):
        if t == 17:
            break
    buf = bytearray(rebuilt)
    buf[so + 16 + 0x1D:so + 16 + 0x3D] = hashlib.sha256(bytes(buf[:so])).digest()
    open(out, "wb").write(buf)
    print("wrote", out, len(buf), "bytes; script", len(open(script, 'rb').read()))
    rc = subprocess.run([sys.executable, os.path.join(os.path.dirname(__file__), "verify_upd.py"),
                         out, "--digests-only"]).returncode
    sys.exit(rc)


if __name__ == "__main__":
    main()
