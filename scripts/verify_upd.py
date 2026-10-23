#!/usr/bin/env python3
"""Verify a Sonos .upd package offline, without a device.

Checks, in order:
  1. section framing (magic 497f1635, no truncation)
  2. type-22 MessageDigest == SHA-256 of the rcbundle that follows its container
  3. type-17 MessageDigest == SHA-256 of every byte before its section
  4. the type-17 signature against the keyring in a libsonoscrypto.so.1

Usage:
  verify_upd.py PKG [--lib PATH]

Exit 0 only when every check passes. The signature is checked with every
2048-bit SPKI found at the keyring offsets; success names the key index.
"""
import hashlib
import struct
import subprocess
import sys
import tempfile

MAGIC = b"\x49\x7f\x16\x35"
KEY_OFFSETS = (0xD8B8, 0xDAE0, 0xDC08, 0xDED8)  # 57.23 libsonoscrypto.so.1
SPKI_LEN = 0x126


def sections(d):
    o = 0
    while o < len(d):
        if d[o:o + 4] != MAGIC:
            raise ValueError("bad section magic at 0x%x" % o)
        t, tot, _flags = struct.unpack_from("<III", d, o + 4)
        if tot < 16 or o + tot > len(d):
            raise ValueError("section %d truncated at 0x%x" % (t, o))
        yield t, o, tot
        o += tot
    if o != len(d):
        raise ValueError("trailing bytes")


def digest_of(payload):
    return payload[0x1D:0x3D]


def main():
    if len(sys.argv) < 2:
        sys.exit(__doc__)
    path = sys.argv[1]
    lib = "/tmp/sq57x/lib/libsonoscrypto.so.1"
    if "--lib" in sys.argv:
        lib = sys.argv[sys.argv.index("--lib") + 1]
    d = open(path, "rb").read()

    found = {}
    for t, o, tot in sections(d):
        found.setdefault(t, (o, tot))
    ok = True

    if 22 in found:
        o, tot = found[22]
        payload = d[o + 16:o + tot]
        want = digest_of(payload)
        got = hashlib.sha256(payload[0x16C:]).digest()
        print("type-22 bundle digest :", "ok" if want == got else "MISMATCH")
        ok &= want == got
    else:
        print("type-22             : absent")
        ok = False

    if 17 not in found:
        print("type-17             : absent")
        sys.exit(1)
    o, tot = found[17]
    payload = d[o + 16:o + tot]
    want = digest_of(payload)
    got = hashlib.sha256(d[:o]).digest()
    print("type-17 prefix digest:", "ok" if want == got else "MISMATCH")
    ok &= want == got

    signed = payload[0x10:0x49]

    def check(n, key):
        with tempfile.NamedTemporaryFile(suffix=".sig") as s, \
                tempfile.NamedTemporaryFile() as m:
            s.write(payload[-256:]); s.flush()
            m.write(signed); m.flush()
            r = subprocess.run(
                ["openssl", "pkeyutl", "-verify", "-pubin", "-inkey", key,
                 "-keyform", "DER", "-rawin", "-digest", "sha256",
                 "-in", m.name, "-sigfile", s.name],
                capture_output=True, text=True)
            return r.returncode == 0

    libdata = open(lib, "rb").read()
    signed_by = None
    for i, off in enumerate(KEY_OFFSETS, 1):
        with tempfile.NamedTemporaryFile(suffix=".der") as k:
            k.write(libdata[off:off + SPKI_LEN]); k.flush()
            if check(i, k.name):
                signed_by = i
                break
    print("type-17 signature   :", ("ok (key #%d)" % signed_by) if signed_by else "MISMATCH")
    ok &= signed_by is not None

    print("verdict             :", "PASS" if ok else "FAIL")
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
