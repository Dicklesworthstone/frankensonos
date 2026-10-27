#!/usr/bin/env python3
"""Deterministic structural DER corpus for the 57.23 device libmbedx509 (bead frankensonos-wbz).

Every case is built on a real parsed tree and re-serialized, so the enclosing lengths are
correct and the mutation is what the parser sees. Families: wrong declared lengths (all
encodings), truncation, tag swaps, duplication, emptied values, nesting.

usage: gen_corpus.py <base.der> <outdir>
Writes <outdir>/<case>.der and <outdir>/manifest.tsv (case, family, note).
"""
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import dertree  # noqa: E402

MAX_LEN = 0xFFFFFFFF


def enc_len(n, size):
    if size == 1:
        return bytes([n & 0x7F])
    n &= (1 << (8 * (size - 1))) - 1
    return bytes([0x80 | (size - 1)]) + n.to_bytes(size - 1, "big")


def main():
    if len(sys.argv) != 3:
        print(__doc__)
        return 2
    base = open(sys.argv[1], "rb").read()
    outdir = sys.argv[2]
    os.makedirs(outdir, exist_ok=True)

    root = dertree.parse_one(base)
    nodes = [n for n, _ in root.walk()]
    cases = []

    def emit(name, family, note, data):
        if data == base:
            return
        with open(os.path.join(outdir, name + ".der"), "wb") as f:
            f.write(data)
        cases.append((name, family, note))

    # ---- family: wrong declared lengths ---------------------------------------
    for i, n in enumerate(nodes):
        real = len(n.body())
        for label, v in (("zero", 0), ("minus1", max(real - 1, 0)), ("plus1", real + 1),
                         ("0x7f", 0x7F), ("0x80", 0x80), ("0xff", 0xFF), ("0x100", 0x100),
                         ("0xffff", 0xFFFF), ("0xffffffff", MAX_LEN)):
            for size in (1, 2, 4):
                if size == 1 and v >= 0x80:
                    continue
                if size == 2 and v < 0x80:
                    continue
                m = root.clone()
                mn = [x for x, _ in m.walk()][i]
                mn.len_override = enc_len(v, size)
                emit(f"len_{i:03d}_{label}_s{size}", "length",
                     f"node#{i} tag=0x{n.tag:02x} real={real} declared={v:#x} size={size}",
                     m.encode())

    # ---- family: truncation ---------------------------------------------------
    for i, n in enumerate(nodes):
        enc = n.encode()
        for k in (0, 1, 2):
            if k < len(enc):
                emit(f"trunc_{i:03d}_{k}", "truncate",
                     f"node#{i} tag=0x{n.tag:02x} cut {k} bytes into its header",
                     enc[:k])

    # ---- family: tag swaps ----------------------------------------------------
    for i, n in enumerate(nodes):
        for nt in (0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x0C, 0x13, 0x16, 0x17,
                   0x18, 0x1E, 0x30, 0x31, 0xA0, 0xA3, 0x80, 0x81, 0x82, 0xFF):
            if nt == n.tag:
                continue
            m = root.clone()
            [x for x, _ in m.walk()][i].tag = nt
            emit(f"tag_{i:03d}_{nt:02x}", "tag", f"node#{i} tag 0x{n.tag:02x}->0x{nt:02x}", m.encode())

    # ---- family: duplicate ----------------------------------------------------
    for i, n in enumerate(nodes):
        if not n.constructed:
            continue
        m = root.clone()
        target = [x for x, _ in m.walk()][i]
        target.value = target.value + [c.clone() for c in target.value]
        emit(f"dup_{i:03d}", "duplicate", f"node#{i} tag=0x{n.tag:02x} children duplicated", m.encode())

    # ---- family: emptied value ------------------------------------------------
    for i, n in enumerate(nodes):
        m = root.clone()
        target = [x for x, _ in m.walk()][i]
        target.value = [] if n.constructed else b""
        emit(f"empty_{i:03d}", "empty", f"node#{i} tag=0x{n.tag:02x} emptied", m.encode())

    # ---- family: nesting ------------------------------------------------------
    for depth in (2, 8, 64, 200):
        data = base
        for _ in range(depth):
            data = b"\x30\x83" + len(data).to_bytes(3, "big") + data
        emit(f"nest_{depth}", "nest", f"cert wrapped in {depth} SEQUENCEs", data)

    with open(os.path.join(outdir, "manifest.tsv"), "w") as f:
        for name, family, note in cases:
            f.write(f"{name}\t{family}\t{note}\n")
    fams = {}
    for _, family, _ in cases:
        fams[family] = fams.get(family, 0) + 1
    print(f"{len(cases)} cases -> {outdir}  {fams}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
