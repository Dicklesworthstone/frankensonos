#!/usr/bin/env python3
"""Run ZZ0137 (the devmode statement validator) offline on a candidate blob.

ZZ0137 lives in libsonoscrypto.so.1 (MIPS32, 57.23 build) and is the exact
function the device calls from POST /devmode. Running it here answers "would the
device accept this statement, and if not, which check failed" without a device.

Usage:
    emu_zz0137.py STATEMENT [DEVICE_ID|str:DEVICE_ID] [MODEL] [VER_MAJOR] [VER_MINOR] [WINDOW_LO,WINDOW_HI]

STATEMENT is the raw decoded TLV (not base64). DEVICE_ID is hex, or ASCII
with a `str:` prefix (that is what the player's Device ID is). It defaults to
000102030405060708090a0b, MODEL to 0x2f, the version to 57.23.
WINDOW_LO,WINDOW_HI overrides the hardcoded 57.23 version window (the S1
library only knows that one; pass the target version for other firmware).

The signature verify is stubbed: reaching it prints "signature verify" and the
run stops there, because the compiled SPKI is what decides and we cannot sign.
Every earlier check is the real code.
"""
import hashlib
import os
import struct
import sys

from unicorn import Uc, UcError, UC_ARCH_MIPS, UC_HOOK_CODE, UC_MODE_BIG_ENDIAN, UC_MODE_MIPS32
from unicorn.mips_const import (
    UC_MIPS_REG_A0, UC_MIPS_REG_A1, UC_MIPS_REG_A2, UC_MIPS_REG_A3,
    UC_MIPS_REG_GP, UC_MIPS_REG_PC, UC_MIPS_REG_RA, UC_MIPS_REG_SP,
    UC_MIPS_REG_T7, UC_MIPS_REG_T9, UC_MIPS_REG_V0,
)

LIB = "captures/firmware/57.23-74170-1-12/libsonoscrypto.so.1"
ENTRY = 0xAB54
SENTINEL = 0x601800
STUB = 0x500000
BUF = 0x600000
DEV = 0x601000
STACK = 0x610000

# Where each check lives, so a stop can be named.
STAGES = [
    (0xAB64, "length < 4"),
    (0xABC0, "magic"),
    (0xABE8, "model"),
    (0xAC18, "lower version bound"),
    (0xAC5C, "upper version bound"),
    (0xAC7C, "exact-length 32"),
    (0xAC94, "device id length"),
    (0xACA4, "device id value"),
    (0xACDC, "payload length"),
    (0xACF4, "ring length"),
    (0xAD48, "ring parse"),
    (0xAD78, "signature verify"),
]


def sections(d, shoff, se, sn, shstr):
    """Return {name: (addr, offset, size)} for every section."""
    base, = struct.unpack(">I", d[shoff + shstr * se + 16:shoff + shstr * se + 20])
    out = {}
    for i in range(sn):
        o = shoff + i * se
        no, = struct.unpack(">I", d[o:o + 4])
        end = d.index(b"\0", base + no)
        addr, off, size = struct.unpack(">III", d[o + 12:o + 24])
        out[d[base + no:end].decode()] = (addr, off, size)
    return out


def main():
    if len(sys.argv) < 2:
        print(__doc__)
        return 2
    blob = open(sys.argv[1], "rb").read()
    arg = sys.argv[2] if len(sys.argv) > 2 else ""
    if arg.startswith("str:"):
        devid = arg[4:].encode()
    elif arg:
        devid = bytes.fromhex(arg)
    else:
        devid = bytes(range(12))
    model = int(sys.argv[3], 0) if len(sys.argv) > 3 else 0x2F
    major = int(sys.argv[4], 0) if len(sys.argv) > 4 else 57
    minor = int(sys.argv[5], 0) if len(sys.argv) > 5 else 23
    # The S1 library hardcodes the 57.23 window. For another version, patch the
    # two immediates so the bound check matches the target firmware.
    window = None
    if len(sys.argv) > 6:
        lo, _, hi = sys.argv[6].partition(",")
        window = (int(lo, 0), int(hi or lo, 0))

    d = open(LIB, "rb").read()
    phoff, = struct.unpack(">I", d[28:32])
    pe, pn = struct.unpack(">HH", d[42:46])
    shoff, = struct.unpack(">I", d[32:36])
    se, sn = struct.unpack(">HH", d[46:50])
    shstr, = struct.unpack(">H", d[50:52])
    sec = sections(d, shoff, se, sn, shstr)

    names = []
    sym_addr, sym_off, sym_size = sec[".dynsym"]
    str_off = sec[".dynstr"][1]
    for i in range(sym_size // 16):
        no, = struct.unpack(">I", d[sym_off + i * 16:sym_off + i * 16 + 4])
        end = d.index(b"\0", str_off + no)
        names.append(d[str_off + no:end].decode())

    mu = Uc(UC_ARCH_MIPS, UC_MODE_MIPS32 | UC_MODE_BIG_ENDIAN)
    for i in range(pn):
        o = phoff + i * pe
        ptype, off, va, _, fsize, msize, _, _ = struct.unpack(">IIIIIIII", d[o:o + 32])
        if ptype != 1:
            continue
        lo = va & ~0xFFF
        mu.mem_map(lo, ((va + msize + 0xFFF) & ~0xFFF) - lo)
        mu.mem_write(va, d[off:off + fsize])
    mu.mem_map(STUB, 0x8000)
    mu.mem_map(BUF, 0x20000)
    mu.mem_map(0xDEAD0000, 0x1000)
    mu.mem_write(SENTINEL, b"\0" * 4)

    # The .MIPS.stubs entries are lazy-binding trampolines; patch each to a jump
    # into our handler so no dynamic linker is needed.
    stubs = {}
    stub_addr, stub_off, stub_size = sec[".MIPS.stubs"]
    for i in range(0, stub_size - 8, 4):
        word, = struct.unpack(">I", d[stub_off + i:stub_off + i + 4])
        if word >> 16 != 0x2418:
            continue
        name = names[word & 0xFFFF]
        if name not in stubs:
            stubs[name] = STUB + len(stubs) * 16
        target = stubs[name]
        mu.mem_write(stub_addr + i - 12, struct.pack(">I", 0x08000000 | ((target >> 2) & 0x03FFFFFF)))

    if window:
        lo, hi = window
        for addr, word in ((0xAC0C, 0x3C020000 | ((lo + 1) >> 16)),
                           (0xAC10, 0x24420000 | ((lo + 1) & 0xFFFF)),
                           (0xAC50, 0x3C030000 | (hi >> 16)),
                           (0xAC54, 0x24630000 | (hi & 0xFFFF))):
            mu.mem_write(addr, struct.pack(">I", word))
    mu.mem_write(BUF, blob)
    mu.mem_write(DEV, devid)
    mu.reg_write(UC_MIPS_REG_SP, STACK)
    mu.reg_write(UC_MIPS_REG_GP, 0x263B0)
    mu.reg_write(UC_MIPS_REG_T9, ENTRY)
    mu.reg_write(UC_MIPS_REG_RA, SENTINEL)
    mu.reg_write(UC_MIPS_REG_A0, BUF)
    mu.reg_write(UC_MIPS_REG_A1, len(blob))
    mu.reg_write(UC_MIPS_REG_A2, model)
    mu.reg_write(UC_MIPS_REG_A3, DEV)
    # stack args: devidlen, outPayloadLen, outPayloadOff, outLower, outUpper,
    # verMajor, verMinor
    for k, v in enumerate([len(devid), 0x602000, 0x602100, 0x602200, 0x602300, major, minor]):
        mu.mem_write(STACK + 16 + k * 4, struct.pack(">I", v))

    state = {"stop": None, "sha": bytearray(), "stage": None, "hist": []}

    def hook(uc, pc, _size, _ud):
        if pc == SENTINEL:
            state["stop"] = "returned"
            uc.emu_stop()
            return
        if 0x9F24 <= pc < 0xA400:
            state["hist"].append(pc)
            del state["hist"][:-40]
        for addr, name in STAGES:
            if pc == addr:
                state["stage"] = name
        if pc not in stubs.values():
            return
        name = next(n for n, a in stubs.items() if a == pc)
        a0, a1, a2 = (uc.reg_read(r) for r in (UC_MIPS_REG_A0, UC_MIPS_REG_A1, UC_MIPS_REG_A2))
        if name == "memcmp":
            x, y = bytes(uc.mem_read(a0, a2)), bytes(uc.mem_read(a1, a2))
            uc.reg_write(UC_MIPS_REG_V0, 0 if x == y else 1)
        elif name == "memset":
            for i in range(a2):
                uc.mem_write(a0 + i, bytes([a1 & 0xFF]))
            uc.reg_write(UC_MIPS_REG_V0, a0)
        elif name in ("memcpy", "memmove"):
            data = bytes(uc.mem_read(a1, a2))
            for i, c in enumerate(data):
                uc.mem_write(a0 + i, bytes([c]))
            uc.reg_write(UC_MIPS_REG_V0, a0)
        elif name in ("mbedtls_sha256_init", "mbedtls_sha256_free"):
            uc.reg_write(UC_MIPS_REG_V0, 0)
        elif name in ("mbedtls_sha256_starts", "mbedtls_sha256_starts_ret"):
            uc.reg_write(UC_MIPS_REG_V0, 0)
        elif name in ("mbedtls_sha256_update", "mbedtls_sha256_update_ret"):
            state["sha"] += bytes(uc.mem_read(a1, a2))
            uc.reg_write(UC_MIPS_REG_V0, 0)
        elif name in ("mbedtls_sha256_finish", "mbedtls_sha256_finish_ret"):
            for i, c in enumerate(hashlib.sha256(bytes(state["sha"])).digest()):
                uc.mem_write(a1 + i, bytes([c]))
            uc.reg_write(UC_MIPS_REG_V0, 0)
        else:
            state["stop"] = f"stub:{name}"
            uc.emu_stop()
            return
        uc.reg_write(UC_MIPS_REG_PC, uc.reg_read(UC_MIPS_REG_T7))

    mu.hook_add(UC_HOOK_CODE, hook)
    try:
        mu.emu_start(ENTRY, 0, count=5_000_000)
    except UcError as exc:
        state["stop"] = f"fault:{exc}"

    v0 = mu.reg_read(UC_MIPS_REG_V0)
    if state["stop"] == "returned":
        print("ACCEPTED" if v0 == 1 else "REJECTED", f"(ZZ0137 -> {v0}, last check: {state['stage']})")
        if os.environ.get("EMU_TRACE"):
            print("ring parser:", " ".join(f"{x:#x}" for x in state["hist"]))
        return 0 if v0 == 1 else 1
    pc = mu.reg_read(UC_MIPS_REG_PC)
    print(f"stopped at {pc:#x} after \"{state['stage']}\": {state['stop']}")
    if os.environ.get("EMU_TRACE"):
        print("ring parser:", " ".join(f"{x:#x}" for x in state["hist"]))
    return 1


if __name__ == "__main__":
    sys.exit(main())
