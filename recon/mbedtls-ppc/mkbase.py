#!/usr/bin/env python3
"""Rewrite a certificate's validity to a fixed window so the harness clock never matters.

usage: mkbase.py <in.der> <out.der> [notBefore notAfter]
Defaults: 200101000000Z / 300101000000Z (UTCTime, 2020-2030).
"""
import sys
import os

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dertree  # noqa: E402
from dertree import Node  # noqa: E402


def main():
    if len(sys.argv) < 3:
        print(__doc__)
        return 2
    nb = sys.argv[3].encode() if len(sys.argv) > 3 else b"200101000000Z"
    na = sys.argv[4].encode() if len(sys.argv) > 4 else b"300101000000Z"

    root = dertree.parse_one(open(sys.argv[1], "rb").read())
    tbs = root.value[0]
    f = tbs.value
    idx = 1 if f and f[0].tag == 0xA0 else 0
    validity = f[idx + 3]
    validity.value = [Node(0x17, nb), Node(0x17, na)]
    open(sys.argv[2], "wb").write(root.encode())
    return 0


if __name__ == "__main__":
    sys.exit(main())
