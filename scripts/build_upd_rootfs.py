#!/usr/bin/env python3
"""Rebuild a Sonos .upd with one section's payload replaced (re-encrypted).

Everything is copied byte-for-byte except the chosen record, whose payload is
re-wrapped in the 886499ca envelope to the recipient the base package already
uses for that record. The record's 16-byte header (magic, type, total length,
field) is preserved when the new ciphertext length matches, so a replacement
that keeps the plaintext size also keeps the header identical.

The type-17 section is never touched and is asserted byte-identical; its
signature covers only the list2 region, so a byte-identical type-17 stays
valid. The script re-checks that (sha256 of payload[0x10:0x49] == the digest
in the type-2 list2 record) and prints it.

Usage:
  build_upd_rootfs.py BASE.upd --type 4 --payload-file ROOTFS.bin \
      --key model16-private.pem --out OUT.upd
"""

import hashlib
import os
import struct
import subprocess
import sys
import tempfile

MAGIC = b"\x49\x7f\x16\x35"          # 0x35167f49, little-endian on disk
ENV = bytes.fromhex("886499ca")
FILE_HDR = 0x1C                      # magic, version, header_len, 0, ver triple


def records(d):
    """Yield (type, offset, total_len) for every record."""
    o = FILE_HDR
    while o < len(d):
        if d[o:o + 4] != MAGIC:
            raise SystemExit("bad magic at 0x%x" % o)
        t, tot = struct.unpack_from("<II", d, o + 4)
        yield t, o, tot
        o += tot


def unwrap_header(p):
    hl = struct.unpack_from(">I", p, 4)[0]
    if p[:4] != ENV or hl != 0x13A:
        raise SystemExit("unexpected envelope header %r/0x%x" % (p[:4], hl))
    return p[0x2A:0x2E]               # cipher, rsa_len


def recipient_of(blob):
    rl = struct.unpack_from(">I", blob, 0x11)[0]
    kt = blob[0x15]
    if kt != 3:
        raise SystemExit("recipient keytype %d" % kt)
    return blob[0x16:0x16 + rl]


def envelope(payload, key, recipient, field):
    """Build a record payload: envelope header + RSA-OAEP key + AES-CBC body."""
    with tempfile.TemporaryDirectory() as td:
        aes, iv = os.urandom(16), os.urandom(16)
        inp = os.path.join(td, "p")
        with open(inp, "wb") as f:
            f.write(payload)
        wrapped = subprocess.run(
            ["openssl", "pkeyutl", "-encrypt", "-pubin", "-inkey", key,
             "-pkeyopt", "rsa_padding_mode:oaep", "-pkeyopt", "rsa_oaep_md:sha1",
             "-in", "/dev/stdin"],
            input=aes, capture_output=True, check=True).stdout
        ct = subprocess.run(
            ["openssl", "aes-128-cbc", "-e", "-K", aes.hex(), "-iv", iv.hex(),
             "-in", inp], capture_output=True, check=True).stdout
    if len(wrapped) != 0x100:
        raise SystemExit("wrapped key is %d bytes" % len(wrapped))
    body = (b"\x00\x01\x00\x00" + b"\x00" * 4 + b"\x01"
            + struct.pack(">I", len(recipient)) + b"\x03" + recipient
            + struct.pack("<I", 1) + struct.pack(">I", len(wrapped)) + wrapped
            + struct.pack("<I", 1) + struct.pack(">I", 16 + len(ct)) + iv + ct)
    # header_len runs to the IV: fixed 22 bytes + recipient + cipher/rsa/data fields
    hdr = ENV + struct.pack(">I", 0x16 + len(recipient) + 4 + 4 + 0x100 + 4 + 4)
    blob = hdr + body
    if len(blob) != 0x13A + 16 + len(ct) or struct.unpack_from(">I", blob, 4)[0] != 0x13A:
        raise SystemExit("envelope length mismatch")
    return blob


def type17_digest(d, off, tot):
    """Return (prefix sha256, stored digest, digest offset, signed region sha256).

    The record's type-2 value is SHA-256 of the whole file prefix; the RSA
    signature separately covers the list2 region. Both are checked by the
    verifier, so both are reported here."""
    p = d[off + 16:off + tot]
    if p[:4] != bytes.fromhex("621da74d"):
        raise SystemExit("type-17 magic is not 621da74d")
    if struct.unpack_from(">I", p, 4)[0] > len(p):
        raise SystemExit("header_len past the payload")
    cur = 0x10
    for _ in range(struct.unpack_from(">I", p, 0x0C)[0]):     # list1 is not signed
        cur += 8 + struct.unpack_from(">I", p, cur + 4)[0]
    start = cur
    n2 = struct.unpack_from(">I", p, cur)[0]
    cur += 4
    stored = dg = None
    for _ in range(n2):
        t, ln = struct.unpack_from(">II", p, cur)
        if t == 2 and stored is None:
            dg = off + 16 + cur + 9
            stored = p[cur + 9:cur + 8 + ln].hex()
        cur += 8 + ln
    return (hashlib.sha256(d[:off]).hexdigest(), stored, dg,
            hashlib.sha256(p[start:cur]).hexdigest())


def main():
    a = sys.argv[1:]
    if not a:
        sys.exit(__doc__)
    base = a[0]
    want = int(a[a.index("--type") + 1]) if "--type" in a else 4
    payload = open(a[a.index("--payload-file") + 1], "rb").read()
    key = a[a.index("--key") + 1] if "--key" in a else \
        "captures/firmware/57.23-74170-1-16/model16-private.pem"
    out = a[a.index("--out") + 1] if "--out" in a else base + ".patched"
    bump = "--bump-build" in a
    raw = "--raw" in a

    d = open(base, "rb").read()
    recs = list(records(d))
    hits = [r for r in recs if r[0] == want]
    if len(hits) != 1:
        raise SystemExit("%d sections of type %d" % (len(hits), want))
    _, off, tot = hits[0]
    old = d[off:off + tot]
    field = old[12:16]
    blob = payload if raw else envelope(payload, key, recipient_of(old[16:]), field)

    new = bytearray(d[:off]) + bytearray(old[:16]) + blob + bytearray(d[off + tot:])
    struct.pack_into("<I", new, off + 8, 16 + len(blob))   # keep the length exact
    out_recs = list(records(bytes(new)))
    if [t for t, _, _ in out_recs] != [t for t, _, _ in recs]:
        raise SystemExit("section sequence changed")

    # invariants: everything but the replaced record is byte-identical
    for i, (t, o1, tot1) in enumerate(recs):
        if t == want:
            continue
        o2 = out_recs[i][1]
        if bytes(new[o2:o2 + tot1]) != d[o1:o1 + tot1]:
            raise SystemExit("section %d at index %d differs" % (t, i))

    if bump:                                  # version triple sits right after the header
        b = struct.unpack_from("<I", new, FILE_HDR - 4)[0] + 1
        struct.pack_into("<I", new, FILE_HDR - 4, b)
        print("version   build bumped to %d" % b)

    sign_off, sign_tot = [(o, tot2) for t2, o, tot2 in out_recs if t2 == 17][0]
    prefix, _old, dg, _signed = type17_digest(bytes(new), sign_off, sign_tot)
    # the record's digest field is the prefix hash: rewrite it for the new file
    new[dg:dg + 32] = bytes.fromhex(prefix)
    prefix2, stored2, _dg, signed = type17_digest(bytes(new), sign_off, sign_tot)
    if prefix2 != stored2:
        raise SystemExit("digest rewrite failed (%s != %s)" % (prefix2, stored2))

    open(out, "wb").write(new)
    print("base      %d bytes" % len(d))
    print("out       %d bytes (%+d)" % (len(new), len(new) - len(d)))
    print("type %-3d  %d -> %d bytes%s" % (want, tot, len(blob), " raw" if raw else ""))
    print("type-17   record bytes identical; digest rewritten to %s" % prefix2)
    print("type-17   list2 sha256 %s (signature now stale unless re-signed)" % signed)
    print("wrote     %s" % out)


if __name__ == "__main__":
    main()
