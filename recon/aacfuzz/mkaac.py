#!/usr/bin/env python3
"""Generate the AAC/ADTS targeted corpus for bead frankensonos-qa2 (see docs/RE_AAC_FUZZ.md).

Parser facts baked into these shapes (r2, anacapad 57.23, fcn.10212778):
  sync 0xFF+(b1&0xF0)==0xF0; layer (b1>>5)&3 must be 0; protection_absent b1&1;
  freq index (b2>>2)&0xF table; frame_len = ((b3&3)<<11)|(b4<<3)|(b5>>5), 0 -> 4096;
  guard 0x8000-offset >= frame_len; ID3/private frames via fcn.10212454 with
  syncsafe size b6<<21|b7<<14|b8<<7|b9.
"""
import os
import sys


def adts_header(payload_len, freq_idx=7, ch=2, profile=1, protection_absent=1):
    b0 = 0xFF
    b1 = 0xF0 | (protection_absent)
    b2 = (profile << 6) | (freq_idx << 2) | (ch >> 2)
    b3 = ((ch & 3) << 6) | ((payload_len >> 11) & 3)
    b4 = (payload_len >> 3) & 0xFF
    b5 = ((payload_len & 7) << 5) & 0xFF
    return bytes([b0, b1, b2, b3, b4, b5, 0x00])


def id3_tag(size_syncsafe, tag=b'TPE1', data_len=16):
    hdr = b'ID3' + bytes([3, 0, 0])
    sz = bytes([(size_syncsafe >> 21) & 0x7F, (size_syncsafe >> 14) & 0x7F,
                (size_syncsafe >> 7) & 0x7F, size_syncsafe & 0x7F])
    frame = (tag + bytes([(data_len >> 24) & 0xFF, (data_len >> 16) & 0xFF,
                          (data_len >> 8) & 0xFF, data_len & 0xFF]) + b'\x00\x00' + b'A' * data_len)
    return hdr + sz + frame


def cases():
    pl = bytes(100)
    f = adts_header(107) + pl
    yield 'ta_valid', f + f
    yield 'tb_huge_len', adts_header(0x1FFF) + bytes(10)
    yield 'tc_zerolen', bytes([0xFF, 0xF1, 0x50, 0x00, 0x00, 0x00, 0x00]) + bytes(16)
    yield 'td_id3_huge', id3_tag(0x0FFFFFFF, data_len=32) + bytes(64)
    yield 'te_id3_incomplete', id3_tag(0x1000, data_len=8) + bytes(16)
    yield 'tf_resync', b'\x00' * 3 + b'\xFF\x00' * 5 + f + b'\xAA' * 17 + f
    yield 'tg_crc_short', bytes([0xFF, 0xF0, 0x50, 0x80, 0x00, 0x00, 0x00]) + bytes(2)
    yield 'tg2_crc_only', bytes([0xFF, 0xF0, 0x50, 0x80, 0x00, 0x00, 0x00])
    yield 'th_layer_bad', bytes([0xFF, 0xF6, 0x50, 0x80, 0x00, 0x00, 0x00]) + bytes(16)
    for v in (0xD, 0xF):
        yield 'ti_freq_%x' % v, bytes([0xFF, 0xF1, 0x40 | (v << 2), 0x80, 0, 0, 0]) + bytes(16)
    for v in (0, 7):
        b2 = 0x40 | (7 << 2) | (v >> 2)
        b3 = (v & 3) << 6
        yield 'tj_ch_%d' % v, bytes([0xFF, 0xF1, b2, b3, 0, 0, 0]) + bytes(16)


def main():
    out = sys.argv[1] if len(sys.argv) > 1 else '.'
    os.makedirs(out, exist_ok=True)
    for name, data in cases():
        with open(os.path.join(out, 'aac_%s.aac' % name), 'wb') as fh:
            fh.write(data)
        print(name, len(data))


if __name__ == '__main__':
    main()
