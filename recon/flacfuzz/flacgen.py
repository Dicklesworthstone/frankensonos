#!/usr/bin/env python3
"""Clean FLAC bitstream builder for the 1.3.4 decode-diff triggers (bead frankensonos-81v).

Frame-header grammar per FLAC format spec:
  sync(14)=0b11111111111110, reserved(1), blocking(1), blocksize(4), samplerate(4),
  channel(4), bps(3), reserved(1), utf8 frame/sample number, [blocksize/samplerate extras], crc8.
Subframe: zero-bit(1), type(6), wasted-bits-flag(1) [+unary k], payload.
Frame footer: crc16 over the whole frame from the sync.
"""
import struct


class BW:
    def __init__(self):
        self.bits = []

    def w(self, v, n):
        v &= (1 << n) - 1
        self.bits += [(v >> i) & 1 for i in range(n - 1, -1, -1)]

    def pack(self):
        b = bytearray()
        bits = self.bits
        for i in range(0, len(bits), 8):
            c = bits[i:i + 8]
            c += [0] * (8 - len(c))
            v = 0
            for x in c:
                v = (v << 1) | x
            b.append(v)
        return bytes(b)


def crc8(data):
    crc = 0
    for byte in data:
        crc ^= byte
        for _ in range(8):
            crc = ((crc << 1) ^ 0x07) & 0xFF if crc & 0x80 else (crc << 1) & 0xFF
    return crc


def crc16(data):
    crc = 0
    for byte in data:
        crc ^= byte << 8
        for _ in range(8):
            crc = ((crc << 1) ^ 0x8005) & 0xFFFF if crc & 0x8000 else (crc << 1) & 0xFFFF
    return crc


def utf8enc(n):
    if n < 0x80:
        return bytes([n])
    if n < 0x800:
        return bytes([0xC0 | (n >> 6), 0x80 | (n & 0x3F)])
    if n < 0x10000:
        return bytes([0xE0 | (n >> 12), 0x80 | ((n >> 6) & 0x3F), 0x80 | (n & 0x3F)])
    return bytes([0xF0 | (n >> 18), 0x80 | ((n >> 12) & 0x3F), 0x80 | ((n >> 6) & 0x3F), 0x80 | (n & 0x3F)])


BPS_CODE = {8: 1, 12: 2, 16: 4, 20: 5, 24: 6, 32: 7}

# channel-assignment codes: 0..7 = independent 1..8 ch; 8=L/S, 9=R/S, 10=M/S


def streaminfo(minbs, maxbs, rate, bps, total, ch=1):
    b = BW()
    b.w(minbs, 16); b.w(maxbs, 16); b.w(0, 24); b.w(0, 24)
    b.w(rate, 20); b.w(bps - 1, 5); b.w(ch - 1, 3); b.w(total, 36); b.w(0, 128)
    d = b.pack()
    assert len(d) == 34, len(d)
    return d


def meta_block(last, mtype, payload):
    return bytes([(0x80 if last else 0) | mtype]) + len(payload).to_bytes(3, 'big') + payload


def frame(frame_no, blocksize, bps, chan_code, subframes, bs_code=7, crc_ok=True,
          sync=0b11111111111110, blocking=0, rate_code=9, bps_override=None):
    """subframes: list of callables f(BW) appending subframe bits.
    For side-channel bps the CALLER is responsible for emitting one extra bit of payload
    (the subframe functions receive effective_bps via closure)."""
    b = BW()
    b.w(sync, 14)
    b.w(0, 1)               # reserved
    b.w(blocking, 1)
    b.w(bs_code, 4)
    b.w(rate_code, 4)
    b.w(chan_code, 4)
    b.w(bps_override if bps_override is not None else BPS_CODE[bps], 3)
    b.w(0, 1)
    for byte in utf8enc(frame_no):
        b.w(byte, 8)
    if bs_code == 6:
        b.w(blocksize - 1, 8)
    elif bs_code == 7:
        b.w(blocksize - 1, 16)
    raw = b.pack()
    crc8_bits = []
    hdr = raw + bytes([crc8(raw)])
    # rebuild everything bit-exactly: easier to continue from bits
    b.bits += [(crc8(raw) >> i) & 1 for i in range(7, -1, -1)]
    for sf in subframes:
        sf(b)
    # byte-align zero padding
    while len(b.bits) % 8:
        b.w(0, 1)
    body = b.pack()
    c = crc16(body)
    out = body + bytes([c >> 8, c & 0xFF]) if False else body + c.to_bytes(2, 'big')
    if not crc_ok:
        out = out[:-1] + bytes([out[-1] ^ 0xFF])
    return out


def subframe_constant(value, bps):
    def f(b):
        b.w(0, 1); b.w(0, 6); b.w(0, 1); b.w(value, bps)
    return f


def subframe_fixed(order, warmup_bits, partitions):
    """warmup_bits: list of (value,bits). partitions: list of (rice_param, [(msbs,lsbs_bits)...])."""
    def f(b):
        b.w(0, 1); b.w(0b001000 + order, 6); b.w(0, 1)
        for v, n in warmup_bits:
            b.w(v, n)
        # entropy coding method: partitioned rice (type 0), order p
        part_order = len(partitions).bit_length() - 1
        b.w(0, 2); b.w(part_order, 4)
        for param, rice_vals in partitions:
            b.w(param, 4)
            for msbs, lsbs_bits in rice_vals:
                b.w(0, msbs); b.w(1, 1); b.w(lsbs_bits, param) if param else b.w(0, 0)
    return f


def flac_file(blocks, frames=b''):
    out = b'fLaC'
    for i, (t, p) in enumerate(blocks):
        out += meta_block(i == len(blocks) - 1, t, p)
    return out + frames
