#!/usr/bin/env python3
"""Serializer budget arithmetic for the 57.23 identity record (bead 9sd).

Reads the constants straight out of the binary so the numbers cannot drift from the code:

  0x10219960  cmplwi r0, 0x458      -> output budget
  0x102198e8  stb r3, 1(r30)        -> title length is a single byte (<=255)
  0x1021994c  lwz r5, 0x448(r31)    -> blob length, r31 = record+0xe58 -> record+0xea0
  0x10224c44..0x10224c4c            -> setter guard 0 < len <= 0x400
  0x10219874  addi r31, r28, 0xe58  -> buffer base inside the record
  0x1021987c  addi r30, r11,-0x2a30 -> destination is global 0x1054d5d0

Usage: serializer_budget.py [--bin PATH]
"""
import argparse
import struct
import sys

BIN = "captures/firmware/57.23-74170-1-16/anacapad"
CHECKS = {
    0x10219960: (0x2B800458, "cmplwi r0,0x458 (budget)"),
    0x102198E8: (0x987E0001, "stb r3,1(r30) (title len byte)"),
    0x1021994C: (0x80BF0448, "lwz r5,0x448(r31) (blob len)"),
    0x10219874: (0x3BFC0E58, "addi r31,r28,0xe58 (buffer)"),
    0x1021987C: (0x3BCBD5D0, "addi r30,r11,-0x2a30 (dest global)"),
    0x10224C48: (0x2F800400, "cmpwi r0,0x400 (setter cap)"),
}
BUDGET = 0x458
PREFIX = 7          # 0x49 tag + length byte + 0x50 tag + word + 0x51 tag + len byte + ...
TITLE_MAX = 0xFF
BLOB_MAX = 0x400


def word(d, off):
    return struct.unpack_from(">I", d, off)[0]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--bin", default=BIN)
    ap.add_argument("--title", type=int, default=None, help="title length to evaluate")
    ap.add_argument("--len", type=int, default=None, help="blob length to evaluate")
    a = ap.parse_args()
    d = open(a.bin, "rb").read()

    # ELF vaddr -> file offset via program headers
    phoff, = struct.unpack_from(">I", d, 28)
    phentsize, phnum = struct.unpack_from(">HH", d, 42)
    segs = []
    for i in range(phnum):
        o = phoff + i * phentsize
        t, off, va, pa, fs, ms, fl, al = struct.unpack_from(">IIIIIIII", d, o)
        if t == 1:
            segs.append((va, fs, off))

    def at(vaddr):
        for va, fs, off in segs:
            if va <= vaddr < va + fs:
                return off + (vaddr - va)
        raise KeyError(hex(vaddr))

    ok = True
    print("constant check (binary must match the analysis):")
    for vaddr, (want, label) in sorted(CHECKS.items()):
        got = word(d, at(vaddr))
        good = got == want
        ok &= good
        print("  %#010x %-42s %s" % (vaddr, label, "OK" if good else "MISMATCH %#x" % got))

    print("\nbudget: %#x bytes at global 0x1054d5d0; title length is one byte (max %d);"
          % (BUDGET, TITLE_MAX))
    print("blob length capped by the setter at %#x." % BLOB_MAX)
    print("offset before the blob = %d + strlen(title); overflow when that + len > %#x."
          % (PREFIX, BUDGET))

    print("\nthresholds:")
    print("  max blob %#x needs title >= %d bytes to overflow"
          % (BLOB_MAX, max(0, BUDGET - PREFIX - BLOB_MAX + 1)))
    for title in (0, 32, 64, 128, 200, 255):
        need = BUDGET - PREFIX - title + 1
        print("  title %3d -> blob must be >= %4d (%s)"
              % (title, need, "reachable" if need <= BLOB_MAX else "unreachable"))
    worst = PREFIX + TITLE_MAX + BLOB_MAX
    print("\nworst case end offset %#x -> overrun %#x bytes past the budget,"
          % (worst, worst - BUDGET))
    print("landing range 0x1054da28..0x1054dad6 (includes the emit string at 0x1054da3c).")

    if a.title is not None and a.len is not None:
        start = PREFIX + a.title
        end = start + a.len
        emit = 0x1054DA3C - 0x1054D5D0          # 0x46c
        print("\nevaluate title=%d len=%d:" % (a.title, a.len))
        print("  copy occupies offsets %#x..%#x (budget %#x)" % (start, end, BUDGET))
        print("  overrun: %d bytes" % max(0, end - BUDGET))
        if start <= emit < end:
            print("  emit string 0x1054da3c IS overwritten by blob byte %#x" % (emit - start))
        else:
            print("  emit string 0x1054da3c NOT reached (needs len > %#x)" % (emit - start))
        if a.len > BLOB_MAX:
            print("  note: len %#x exceeds the setter cap %#x" % (a.len, BLOB_MAX))
    return 0 if ok else 1


if __name__ == "__main__":
    sys.exit(main())
