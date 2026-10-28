#!/usr/bin/env python3
"""Generate every targeted FLAC trigger for bead frankensonos-81v (see docs/RE_FLAC_FUZZ.md).

Writes t1..t11 .flac files into the current directory. Deterministic: no randomness.
Each trigger attacks one 1.3.4-vs-upstream decode-path delta; expected behavior per file
is in the doc's trigger table.
"""
import struct
import sys

from flacgen import BW, crc8, crc16, utf8enc, streaminfo, meta_block, frame, subframe_constant, flac_file

BS = 256


def t1_wasted():
    def sf(b):
        b.w(0, 1)          # zero bit
        b.w(0b001000, 6)   # FIXED order 0
        b.w(1, 1)          # wasted-bits flag
        for _ in range(255):
            b.w(0, 1)
        b.w(1, 1)          # u = 255 -> wasted = 256 >= bps 16
    return flac_file([(0, streaminfo(BS, BS, 44100, 16, BS))],
                     frame(0, BS, 16, 0, [sf], bs_code=6))


def t2_bs65536():
    return flac_file([(0, streaminfo(65536, 65536, 44100, 16, 65536))],
                     frame(0, 65536, 16, 0, [subframe_constant(0, 16)], bs_code=7))


def t3_rice_ovf():
    def sf(b):
        b.w(0, 1); b.w(0b001000, 6); b.w(0, 1)   # FIXED order 0
        b.w(0, 2); b.w(0, 4)                     # partitioned rice, order 0
        b.w(14, 4)                               # rice parameter 14
        for _ in range(255):                     # 255 short residuals
            b.w(1, 1); b.w(0, 14)
        for _ in range(300000):                  # huge msbs on the last one
            b.w(0, 1)
        b.w(1, 1)
        b.w(0x3FFF, 14)
    return flac_file([(0, streaminfo(BS, BS, 44100, 16, BS))],
                     frame(0, BS, 16, 0, [sf], bs_code=6))


def t5_seektable0():
    return flac_file([(0, streaminfo(BS, BS, 44100, 16, BS)), (3, b'')], b'')


def t6_vcnull():
    vc = struct.pack('<I', 0) + struct.pack('<I', 1) + struct.pack('<I', 0)
    return flac_file([(0, streaminfo(BS, BS, 44100, 16, BS)), (4, vc)], b'')


def t7_cuesheet0():
    cs = (b'\x00' * 128 + bytes([0]) + b'\x00' * 258 + bytes([0, 0, 0, 0, 0, 0, 0, 0]))[:394]
    return flac_file([(0, streaminfo(BS, BS, 44100, 16, BS)), (5, cs)], b'')


def t8_wasted_side():
    def ch0(b):
        b.w(0, 1); b.w(0, 6); b.w(0, 1); b.w(0, 16)
    def ch1(b):
        b.w(0, 1); b.w(0, 6); b.w(1, 1)
        for _ in range(17):
            b.w(0, 1)
        b.w(1, 1)          # wasted = 18 >= bps 17 (side channel)
    return flac_file([(0, streaminfo(BS, BS, 44100, 16, BS))],
                     frame(0, BS, 16, 10, [ch0, ch1], bs_code=6))


def t9_badcrc_wave():
    frames = b''.join(
        frame(i, BS, 16, 0, [subframe_constant(0, 16)], bs_code=6, crc_ok=(i % 2 == 0))
        for i in range(200))
    return flac_file([(0, streaminfo(BS, BS, 44100, 16, BS * 200))], frames)


def t11_seekhuge():
    return flac_file([(0, streaminfo(BS, BS, 44100, 16, BS))],
                     frame(0, BS, 16, 0, [subframe_constant(0, 16)], bs_code=6))


def t11b_seekoff():
    pts = struct.pack('>Q', 100) + struct.pack('>Q', 0x7FFFFFFFFFFFFFF0) + struct.pack('>H', 256)
    return flac_file([(0, streaminfo(BS, BS, 44100, 16, BS)), (0, pts)],
                     frame(0, BS, 16, 0, [subframe_constant(0, 16)], bs_code=6))


def t11c_seektbl():
    pts = b''
    for sn, off, fs in [(1000, 40, 0), (5, 0, 256), (0x7FFFFFFFFFFFFFFF, 0x10, 1), (50, 0x100000, 256)]:
        pts += struct.pack('>Q', sn) + struct.pack('>Q', off) + struct.pack('>H', fs)
    return flac_file([(0, streaminfo(BS, BS, 44100, 16, 2000)), (0, pts)],
                     frame(0, BS, 16, 0, [subframe_constant(0, 16)], bs_code=6))


def base():
    return flac_file([(0, streaminfo(BS, BS, 44100, 16, BS))],
                     frame(0, BS, 16, 0, [subframe_constant(0, 16)], bs_code=6))


def main():
    for name, fn in [("base", base), ("t1_wasted", t1_wasted), ("t2_bs65536", t2_bs65536),
                     ("t3_rice_ovf", t3_rice_ovf), ("t5_seektable0", t5_seektable0),
                     ("t6_vcnull", t6_vcnull), ("t7_cuesheet0", t7_cuesheet0),
                     ("t8_wasted_side", t8_wasted_side), ("t9_badcrc_wave", t9_badcrc_wave),
                     ("t11_seekhuge", t11_seekhuge), ("t11b_seekoff", t11b_seekoff),
                     ("t11c_seektbl", t11c_seektbl)]:
        data = fn()
        with open(name + ".flac", "wb") as f:
            f.write(data)
        print(name, len(data))


if __name__ == "__main__":
    main()
