#!/usr/bin/env python3
"""Recompute a .upd's type-17 prefix digest in place.

The type-17 record's type-2 list2 value is SHA-256 of every byte before the
record; the RSA signature separately covers the list2 region. Rewriting the
digest makes the prefix check pass and leaves the signature stale, which is the
shape wanted when isolating the signature stage from the parse.

Usage:
  fix_upd_prefix_digest.py FILE.upd [--out OUT.upd]
"""

import hashlib
import struct
import sys

MAGIC = bytes.fromhex("621da74d")


def main():
    a = sys.argv[1:]
    if not a:
        sys.exit(__doc__)
    src = a[0]
    out = a[a.index("--out") + 1] if "--out" in a else src
    d = bytearray(open(src, "rb").read())

    # type 22 uses the same container magic; the type-17 record is the last one
    hits = [i for i in range(len(d)) if d[i:i + 4] == MAGIC]
    if not hits:
        raise SystemExit("no type-17 magic")
    off = hits[-1] - 16
    if struct.unpack_from("<I", d, off + 4)[0] != 17:
        raise SystemExit("last container is not a type-17 record")
    p = d[off + 16:off + 380]

    cur = 0x10
    for _ in range(struct.unpack_from(">I", p, 0x0C)[0]):      # skip list1
        cur += 8 + struct.unpack_from(">I", p, cur + 4)[0]
    cur += 4
    n2 = struct.unpack_from(">I", p, cur - 4)[0]
    slot = None
    for _ in range(n2):
        t, ln = struct.unpack_from(">II", p, cur)
        if t == 2 and slot is None:
            slot = off + 16 + cur + 9
        cur += 8 + ln
    if slot is None:
        raise SystemExit("no type-2 digest record in list2")

    before = bytes(d[slot:slot + 32]).hex()
    new = hashlib.sha256(bytes(d[:off])).digest()
    d[slot:slot + 32] = new
    open(out, "wb").write(d)
    print("record at 0x%x, digest at 0x%x" % (off, slot))
    print("%s -> %s" % (before[:16], new.hex()[:16]))
    print("prefix ok: %s" % (hashlib.sha256(bytes(d[:off])).digest() == new))


if __name__ == "__main__":
    main()
