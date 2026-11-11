#!/usr/bin/env python3
"""Append one extra .upd section after the existing tail, prefix untouched.

The type-17 record's digest covers every byte before it, so appending after it
keeps both the digest and the signature valid. Use this to probe what the
verifier/flasher does with an unused section type.

Usage:
  append_upd_section.py BASE.upd --type 5 --payload-hex 00 [--field 0] --out OUT.upd
  append_upd_section.py BASE.upd --type 4 --payload-file ROOTFS.bin \
      --key recipient.pem [--recipient HEX] --out OUT.upd

With --key the payload is wrapped in the 886499ca envelope before it is
appended, which is what the encrypted record types (3, 4, 6) need. The
recipient id defaults to the one already carried by a record of the same type
in the base, so an appended record addresses the same key as the original.
"""

import hashlib
import os
import struct
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from build_upd_rootfs import envelope, recipient_of  # noqa: E402

MAGIC = b"\x49\x7f\x16\x35"
ENV_TYPES = (3, 4, 6)


def main():
    a = sys.argv[1:]
    if not a or "--type" not in a:
        sys.exit(__doc__)
    base = a[0]
    typ = int(a[a.index("--type") + 1])
    field = int(a[a.index("--field") + 1]) if "--field" in a else 0
    key = a[a.index("--key") + 1] if "--key" in a else None
    out = a[a.index("--out") + 1] if "--out" in a else base + ".appended"

    if "--payload-file" in a:
        payload = open(a[a.index("--payload-file") + 1], "rb").read()
    else:
        payload = bytes.fromhex(a[a.index("--payload-hex") + 1])

    d = open(base, "rb").read()

    # find the type-17 record and insist it is the last one; remember the
    # recipient of any record of the same type so we can address it again
    o, last, same = 0x1C, None, None
    while o + 16 <= len(d):
        magic, t, ln = struct.unpack_from("<III", d, o)
        if d[o:o + 4] != MAGIC:
            raise SystemExit("bad magic at 0x%x" % o)
        if t == typ and t in ENV_TYPES and same is None:
            same = recipient_of(d[o + 16:])
        last = (o, t, ln)
        o += ln
    if last[1] != 17 or last[0] + last[2] != len(d):
        raise SystemExit("last record is type %d, not a trailing type-17" % last[1])

    if key:
        if "--recipient" in a:
            recipient = bytes.fromhex(a[a.index("--recipient") + 1])
        elif same:
            recipient = same
        else:
            raise SystemExit("no recipient: pass --recipient, or use a type "
                             "already present in the base")
        body = envelope(payload, key, recipient, field)
        payload = body

    rec = MAGIC + struct.pack("<III", typ, 16 + len(payload), field) + payload
    new = d + rec
    open(out, "wb").write(new)

    before = hashlib.sha256(d[:last[0]]).hexdigest()
    after = hashlib.sha256(new[:last[0]]).hexdigest()
    print("base      %d bytes" % len(d))
    print("record    type %d len %d field %d payload %s"
          % (typ, len(rec), field, payload.hex()[:48] or "-"))
    print("out       %d bytes (%+d)" % (len(new), len(new) - len(d)))
    print("prefix    %s (%s)" % (before[:16], "unchanged" if before == after else "CHANGED"))
    print("wrote     %s" % out)


if __name__ == "__main__":
    main()
