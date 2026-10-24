#!/usr/bin/env python3
"""Append one extra .upd section after the existing tail, prefix untouched.

The type-17 record's digest covers every byte before it, so appending after it
keeps both the digest and the signature valid. Use this to probe what the
verifier/flasher does with an unused section type.

Usage:
  append_upd_section.py BASE.upd --type 5 --payload-hex 00 [--field 0] --out OUT.upd
"""

import hashlib
import struct
import sys

MAGIC = b"\x49\x7f\x16\x35"


def main():
    a = sys.argv[1:]
    if not a or "--type" not in a:
        sys.exit(__doc__)
    base = a[0]
    typ = int(a[a.index("--type") + 1])
    field = int(a[a.index("--field") + 1]) if "--field" in a else 0
    payload = bytes.fromhex(a[a.index("--payload-hex") + 1])
    out = a[a.index("--out") + 1] if "--out" in a else base + ".appended"

    d = open(base, "rb").read()

    # find the type-17 record and insist it is the last one
    o, last = 0x1C, None
    while o + 16 <= len(d):
        magic, t, ln = struct.unpack_from("<III", d, o)
        if d[o:o + 4] != MAGIC:
            raise SystemExit("bad magic at 0x%x" % o)
        last = (o, t, ln)
        o += ln
    if last[1] != 17 or last[0] + last[2] != len(d):
        raise SystemExit("last record is type %d, not a trailing type-17" % last[1])

    rec = MAGIC + struct.pack("<III", typ, 16 + len(payload), field) + payload
    new = d + rec
    open(out, "wb").write(new)

    before = hashlib.sha256(d[:last[0]]).hexdigest()
    after = hashlib.sha256(new[:last[0]]).hexdigest()
    print("base      %d bytes" % len(d))
    print("record    type %d len %d field %d payload %s"
          % (typ, len(rec), field, payload.hex() or "-"))
    print("out       %d bytes (%+d)" % (len(new), len(new) - len(d)))
    print("prefix    %s (%s)" % (before[:16], "unchanged" if before == after else "CHANGED"))
    print("wrote     %s" % out)


if __name__ == "__main__":
    main()
