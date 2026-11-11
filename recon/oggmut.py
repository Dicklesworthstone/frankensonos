#!/usr/bin/env python3
"""oggmut.py - OGG page-header mutations against the Sonos page parser.

The daemon carries its own OGG page parser (not libogg): lacing values,
segment counts, granule positions and page sequences are all reconciled by
Sonos code, and a normal file plays, so a fault is attributable.

Modes (each mutates ONE structural invariant):
  LACING_OVER   last lacing value far larger than the body that follows
  SEGCNT_MAX    segcount forced to 255 with 1-byte lacings
  SEGCNT_OVER   segcount raised past the remaining page bytes
  GRANULE_MAX   granule 0xFFFFFFFFFFFFFFFF on the target page
  SEQ_GAP       page sequence numbers jumped by 1000 per page
  SEQ_ZERO      every page sequence forced to 0
  HDR_LEN       first (ident) page lacing inflated, type bits cleared wrong

usage:
  oggmut.py IN OUT --mode LACING_OVER [--page N]
"""
import argparse

CAP = b"OggS"


def pages(d: bytes):
    """Yield (offset, header_len, body_len) for each page."""
    o = 0
    while True:
        i = d.find(CAP, o)
        if i < 0:
            return
        if i + 27 > len(d):
            return
        segcnt = d[i + 26]
        hl = 27 + segcnt
        if i + hl > len(d):
            return
        body = sum(d[i + 27:i + hl])
        yield i, hl, body
        o = i + hl + body


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("inp")
    ap.add_argument("out")
    ap.add_argument("--mode", required=True)
    ap.add_argument("--page", type=int, default=1)
    a = ap.parse_args()

    d = bytearray(open(a.inp, "rb").read())
    pg = list(pages(bytes(d)))
    if not pg:
        raise SystemExit("no OGG pages found")
    idx = min(a.page, len(pg) - 1)
    off, hl, body = pg[idx]

    if a.mode == "LACING_OVER":
        d[off + hl - 1] = 0xFF           # last lacing says 255, body ≠ 255
    elif a.mode == "SEGCNT_MAX":
        # rebuild: 255 one-byte lacings, keep body bytes as they are
        segs = bytes([1]) * 255
        d[off + 26] = 255
        d[off + 27:off + 27] = b""       # splice new table
        d[off + 27:off + 27] = segs
    elif a.mode == "SEGCNT_OVER":
        d[off + 26] = min(255, d[off + 26] + 0x80)
    elif a.mode == "GRANULE_MAX":
        d[off + 6:off + 14] = b"\xff" * 8
    elif a.mode == "SEQ_GAP":
        for n, (po, _ph, _pb) in enumerate(pg):
            d[po + 18:po + 22] = (n * 1000 + 1).to_bytes(4, "little")
    elif a.mode == "SEQ_ZERO":
        for po, _ph, _pb in pg:
            d[po + 18:po + 22] = b"\x00" * 4
    elif a.mode == "HDR_LEN":
        d[off + 5] = 0x02                # ident page: BOS type cleared
        d[off + 26] = 1
    else:
        raise SystemExit("unknown mode")

    open(a.out, "wb").write(bytes(d))
    print("wrote %s (%d bytes, mode=%s page=%d off=%d)"
          % (a.out, len(d), a.mode, idx, off))


if __name__ == "__main__":
    main()
