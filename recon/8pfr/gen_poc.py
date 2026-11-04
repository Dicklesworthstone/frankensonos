#!/usr/bin/env python3
"""8pfr — generate minimal ISO-BMFF mutation PoCs from a known-good 16-bit ALAC .m4a.

Base layout (offsets fixed by ffmpeg, asserted below):
  0x000 ftyp(28)  0x01c moov(1123)  0x47f free(8)  0x487 mdat(170096)
  moov/trak/mdia/minf/stbl/stsd: stsd at 0x1a9 (entries=1 at 0x1c1),
  sample entry 'alac' at 0x1c5, ALAC cookie box at 0x1e5 (size 0x24, magic 'alac'),
  cookie fields at 0x1f1: frameLength(4) compatVersion(1) bitDepth(1) pb(3) mb(4) kb(4) ...

Each case mutates exactly one thing, so a differing device verdict is attributable.
Box-size lies are written into the size field only (`size<8`, `0` = to-EOF, `1` = 64-bit with a
huge largesize, `0xFFFFFFFF`), which is the classic mov.c hardening set; truncations cut bytes;
esds cases rebuild the 36-byte sample-entry child as mp4a+esds with lying TLV lengths.
"""
import os, shutil, struct, sys

SRC = os.environ.get("SRC", "/tmp/m4a_poc/base.m4a")
OUT = os.environ.get("OUT", "/tmp/m4a_poc/poc")

def u32(b, off): return struct.unpack(">I", b[off:off+4])[0]
def put(b, off, v): b[off:off+4] = struct.pack(">I", v)

def load():
    b = bytearray(open(SRC, "rb").read())
    assert b[4:8] == b"ftyp", "base is not an mp4"
    assert b[0x1c+4:0x1c+8] == b"moov"
    assert b[0x1a9+4:0x1a9+8] == b"stbl"
    assert b[0x1b1+4:0x1b1+8] == b"stsd"
    assert b[0x1c1+4:0x1c1+8] == b"alac", "sample entry is not alac"
    assert b[0x1e5+4:0x1e5+8] == b"alac", "cookie box is not alac"
    assert b[0x1f6] == 0x10, "cookie bitDepth is not at 0x1f6"
    return b

def grow_fields(b, off, delta):
    """delta added to a box body: fix every enclosing size from stbl up to moov, and mdat offset."""
    for p in (0x1a9, 0x1b1, 0x16d, 0x118, 0x90, 0x1c):   # stbl, minf, mdia, trak, moov
        put(b, p, u32(b, p) + delta)

CASES = {}

def case(fn):
    CASES[fn.__name__] = fn
    return fn

# ---- class A: ftyp -------------------------------------------------------
@case
def ftyp_size_zero(b): put(b, 0, 0)
@case
def ftyp_size_seven(b): put(b, 0, 7)
@case
def ftyp_size_ones(b): put(b, 0, 0xFFFFFFFF)
@case
def ftyp_size_one_64bit(b):
    b[0:4] = b"\x00\x00\x00\x01"
    b[8:8] = struct.pack(">Q", 0x0000000100000000 + 28)   # largesize, absurdly large
    b.extend(b"") if False else None
@case
def ftyp_brand_garbage(b): b[8:12] = b"\xde\xad\xbe\xef"
@case
def ftyp_truncated(b): del b[20:]

# ---- class B: top-level / moov / moof ------------------------------------
@case
def moov_size_zero(b): put(b, 0x1c, 0)          # moov runs to EOF, swallowing free+mdat
@case
def moov_size_ones(b): put(b, 0x1c, 0xFFFFFFFF)
@case
def moov_size_seven(b): put(b, 0x1c, 7)
@case
def moov_size_one_64bit(b):
    b[0x1c:0x20] = b"\x00\x00\x00\x01"
    b[0x20:0x20] = struct.pack(">Q", 0xFFFFFFFF00000000)
@case
def no_moov(b): del b[0x1c:0x47f]
@case
def no_mdat(b): del b[0x487:]
@case
def mdat_size_zero(b): put(b, 0x487, 0)
@case
def mdat_size_ones(b): put(b, 0x487, 0xFFFFFFFF)
@case
def nested_moov(b):
    inner = bytearray(b[0x24:0x47f])              # moov body, minus the mvhd..udta? keep as-is
    nb = bytearray(b[:0x1c]) + b[0x1c:0x24] + struct.pack(">I", len(inner)+8) + b"moov" + inner + b[0x47f:]
    b[:] = nb

# ---- class C: sample-table arithmetic ------------------------------------
@case
def stsz_count_max(b):
    # stsz body = ver/flags(4) sample_size(4) sample_count(4); count at 0x245+12
    put(b, 0x245+12, 0xFFFFFFFF)
@case
def stsz_size_max(b):
    put(b, 0x245+8, 0xFFFFFFFF); put(b, 0x245+12, 1)   # constant huge sample size
@case
def stco_offset_max(b):
    put(b, 0x409+12, 0xFFFFFFFF)                       # first chunk offset
@case
def stsc_entries_max(b):
    put(b, 0x229+12, 0xFFFFFFFF)                       # entry_count
@case
def stts_count_max(b):
    put(b, 0x209+8, 0xFFFFFFFF)                        # entry_count
@case
def mdhd_timescale_zero(b): put(b, 0x120+16, 0)
@case
def mvhd_timescale_zero(b): put(b, 0x24+16, 0)
@case
def stsd_entry_count_zero(b): put(b, 0x1bd, 0)
@case
def stsd_entry_count_ffff(b): put(b, 0x1bd, 0xFFFF)
@case
def stsd_entry_size_zero(b): put(b, 0x1c1, 0)
@case
def stsd_entry_size_ones(b): put(b, 0x1c1, 0xFFFFFFFF)

# ---- class E: ALAC cookie -------------------------------------------------
@case
def cookie_bitdepth_20(b): b[0x1f6] = 20
@case
def cookie_bitdepth_24(b): b[0x1f6] = 24
@case
def cookie_bitdepth_32(b): b[0x1f6] = 32
@case
def cookie_bitdepth_zero(b): b[0x1f6] = 0
@case
def cookie_frame_length_zero(b): put(b, 0x1f1, 0)
@case
def cookie_frame_length_max(b): put(b, 0x1f1, 0xFFFFFFFF)
@case
def cookie_channels_zero(b): b[0x1fa] = 0
@case
def cookie_channels_ff(b): b[0x1fa] = 0xFF
@case
def cookie_truncated_24(b):
    del b[0x1f9:0x209]
    put(b, 0x1e5, 0x14); put(b, 0x1c1, 0x38); grow_fields(b, 0x1e5, -16)
@case
def cookie_size_zero(b): put(b, 0x1e5, 0)
@case
def cookie_size_ones(b): put(b, 0x1e5, 0xFFFFFFFF)

# ---- class D: mp4a/esds ---------------------------------------------------
def esds_child(b, payload):
    """replace the 36-byte alac cookie box with an equal-size esds carrying `payload`."""
    blk = bytearray(payload)
    assert len(blk) <= 28, len(blk)
    blk += b"\x00" * (28 - len(blk))
    box = struct.pack(">I", 36) + b"esds" + b"\x00\x00\x00\x00" + bytes(blk)   # 8+4+28
    b[0x1e5:0x1e5+36] = box
    b[0x1c5:0x1c9] = b"mp4a"

@case
def esds_minimal(b):
    # ES_Descriptor(0x03) -> DecoderConfig(0x04) -> DecSpecificInfo(0x05, OTI 0x40 AAC)
    esds_child(b, bytes([0x03,0x14,0x00,0x01,0x00,
              0x04,0x0D,0x40,0x15,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,0x00,
              0x05,0x02,0x12,0x10]))
@case
def esds_length_zero(b):
    esds_child(b, bytes([0x03,0x00,0x00,0x01,0x00,0x04,0x00,0x40]))
@case
def esds_length_ffff(b):
    esds_child(b, bytes([0x03,0x80,0x80,0x80,0xFF,0x00,0x01,0x00]))
@case
def esds_deep_nesting(b):
    # 12 nested expandable-class descriptors (tag 0x80..) without terminal
    esds_child(b, bytes([0x03,0x18]) + bytes([0x80,0x01,0x00]) * 8)
@case
def esds_tag_only(b):
    esds_child(b, bytes([0x03]))

def main():
    os.makedirs(OUT, exist_ok=True)
    for name, fn in CASES.items():
        b = load()
        try:
            fn(b)
        except Exception as e:
            print(f"SKIP {name}: {e}"); continue
        open(os.path.join(OUT, name + ".m4a"), "wb").write(bytes(b))
    print(f"{len(os.listdir(OUT))} cases -> {OUT}")

if __name__ == "__main__":
    main()
