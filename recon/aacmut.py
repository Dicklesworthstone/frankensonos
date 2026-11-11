#!/usr/bin/env python3
"""aacmut.py - surgical ADTS frame-header mutations.

The Sonos AAC path is in-binary, proprietary and unaudited (no libavcodec in
the rootfs, no ffmpeg strings). ADTS headers carry a 13-bit `frame_length`
that the demuxer must trust to find the next frame; it is the classic
"declared length vs bytes present" bug. This mutator changes exactly that
field and nothing else, so a verdict attributes to the length handling rather
than to a mangled payload.

ADTS header (no CRC when protection_absent=1):
  sync 12 | ID 1 | layer 2 | protection_absent 1 | profile 2 | sf 4 |
  private 1 | ch_cfg 3 | frame_length 13 | buffer_fullness 11 | blocks 2

Modes:
  FLEN_MAX    frame_length := 0x1FFF on the target frame
  FLEN_OVER   frame_length := actual + N
  FLEN_ZERO   frame_length := 0
  BUF_MAX     buffer_fullness := 0x7FF (keep length valid)
  SYNC_NEXT   keep length, clear the syncword of the target frame

usage:
  aacmut.py IN OUT --mode FLEN_MAX [--frame N] [--delta 0x400]
"""
import argparse


def frames(d: bytes):
    """Yield offsets of plausible ADTS frames (syncword 0xFFF, layer bits 00)."""
    o = 0
    while o + 7 <= len(d):
        if d[o] == 0xFF and (d[o + 1] & 0xF6) == 0xF0:
            yield o
            flen = ((d[o + 3] & 0x03) << 11) | (d[o + 4] << 3) | (d[o + 5] >> 5)
            o += flen if flen >= 7 else 7
        else:
            o += 1


def set_flen(d: bytearray, o: int, val: int):
    v = val & 0x1FFF
    d[o + 3] = (d[o + 3] & 0xFC) | ((v >> 11) & 0x03)
    d[o + 4] = (v >> 3) & 0xFF
    d[o + 5] = (d[o + 5] & 0x1F) | ((v & 0x07) << 5)


def get_flen(d: bytes, o: int) -> int:
    return ((d[o + 3] & 0x03) << 11) | (d[o + 4] << 3) | (d[o + 5] >> 5)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("inp")
    ap.add_argument("out")
    ap.add_argument("--mode", required=True)
    ap.add_argument("--frame", type=int, default=3)
    ap.add_argument("--delta", type=lambda s: int(s, 0), default=0x400)
    a = ap.parse_args()

    d = bytearray(open(a.inp, "rb").read())
    fr = list(frames(bytes(d)))
    if not fr:
        raise SystemExit("no ADTS frames found")
    o = fr[min(a.frame, len(fr) - 1)]
    orig = get_flen(d, o)

    if a.mode == "FLEN_MAX":
        set_flen(d, o, 0x1FFF)
    elif a.mode == "FLEN_OVER":
        set_flen(d, o, orig + a.delta)
    elif a.mode == "FLEN_ZERO":
        set_flen(d, o, 0)
    elif a.mode == "BUF_MAX":
        d[o + 5] = (d[o + 5] & 0xE0) | 0x1F
        d[o + 6] = 0xFF
    elif a.mode == "SYNC_NEXT":
        d[o] = 0xFE
    else:
        raise SystemExit("unknown mode")

    open(a.out, "wb").write(bytes(d))
    print("wrote %s (%d bytes, mode=%s frame=%d off=%d flen %d->%d)"
          % (a.out, len(d), a.mode, a.frame, o, orig, get_flen(d, o)))


if __name__ == "__main__":
    main()
