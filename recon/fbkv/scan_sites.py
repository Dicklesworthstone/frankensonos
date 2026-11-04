#!/usr/bin/env python3
"""Enumerate direct `bl` call sites of a target in the 57.23 anacapad (PPC).

Usage: scan_sites.py 0x10222778    (prints one site per line, plus the containing prologue)
Used for the fbkv allocator map; the same script finds any statically-called function.
"""
import struct, sys

import os
BIN = os.environ.get("BIN", "/Users/jemanuel/projects/frankensonos/captures/firmware/57.23-74170-1-16/anacapad")
TEXT_OFF, TEXT_VA, TEXT_SZ = 0x755C, 0x1000755C, 0x435638

def main(target):
    f = open(BIN, "rb").read()
    for off in range(0, TEXT_SZ - 4, 4):
        w = struct.unpack(">I", f[TEXT_OFF + off:TEXT_OFF + off + 4])[0]
        if (w >> 26) == 18 and (w & 1):                 # bl
            d = w & 0x03FFFFFC
            if d & 0x02000000: d -= 0x04000000
            if (TEXT_VA + off) + d == target:
                print(hex(TEXT_VA + off))

if __name__ == "__main__":
    main(int(sys.argv[1], 16))
