#!/usr/bin/env python3
"""Decode the PPC branch algebra around 0x10115478 (57.23 anacapad).

Question: under what register conditions does the UNCHECKED copy
strcpy(session+6380, node) at 0x10115478-80 execute, vs the capped
paths at +7405 etc.? And what copies do the skipped-to targets do?
"""
import struct

BASE = 0x10000000
BLOB = open("captures/firmware/57.23-74170-1-16/anacapad", "rb").read()

OP31_NAMES = {
    266: "add", 10: "addc", 138: "adde", 40: "subf", 8: "subfc", 104: "subfe",
    28: "and", 444: "or", 284: "eqv", 812: "nand", 476: "nor", 412: "orc",
    792: "sraw", 824: "srawi", 24: "slw", 536: "srw", 954: "extsb", 922: "extsh",
    23: "lwzx", 55: "lwzux", 87: "lbzx", 119: "lbzux", 151: "stwx", 183: "stwux",
    215: "stbx", 278: "sync", 598: "??", 854: "eieio", 197: "??", 659: "??",
}


def dec(i: int, addr: int) -> str:
    op = (i >> 26) & 0x3F
    rT = (i >> 21) & 0x1F
    rA = (i >> 16) & 0x1F
    rB = (i >> 11) & 0x1F
    d = i & 0xFFFF
    if op == 36:
        return f"lwz r{rT},{d}(r{rA})"
    if op == 37:
        return f"lwzu r{rT},{d}(r{rA})"
    if op == 34:
        return f"lbz r{rT},{d}(r{rA})"
    if op == 35:
        return f"lbzu r{rT},{d}(r{rA})"
    if op == 38:
        return f"stb r{rT},{d}(r{rA})"
    if op == 39:
        return f"stbu r{rT},{d}(r{rA})"
    if op == 32:
        return f"lwz r{rT},{d}(r{rA})"
    if op == 33:
        return f"lwzu r{rT},{d}(r{rA})"
    if op == 14:
        sim = d - 0x10000 if d >= 0x8000 else d
        return f"addi r{rT},r{rA},{sim}"
    if op == 15:
        return f"addis r{rT},r{rA},0x{d:x}"
    if op == 12:
        sim = d - 0x10000 if d >= 0x8000 else d
        return f"addic r{rT},r{rA},{sim}"
    if op == 13:
        sim = d - 0x10000 if d >= 0x8000 else d
        return f"addic. r{rT},r{rA},{sim}"
    if op == 11:
        sim = d - 0x10000 if d >= 0x8000 else d
        return f"cmpwi cr{(i >> 23) & 7}, r{rA}, {sim}"
    if op == 10:
        return f"cmplwi cr{(i >> 23) & 7}, r{rA}, 0x{d:x}"
    if op == 16:
        bo = (i >> 21) & 0x1F
        bi = (i >> 16) & 0x1F
        bd = i & 0xFFFC
        if bd >= 0x8000:
            bd -= 0x10000
        crf = bi // 4
        bit = bi % 4
        name = {2: "eq", 0: "lt", 1: "gt", 3: "so"}[bit]
        sense = {4: f"b{'n' if bit != 2 else 'e'}", 12: f"b{'e' if bit == 2 else 'n'}"}
        nm = f"b{'e' if (bo in (12, 4) and bit == 2) else 'n'}{'' if bo in (12, 4) else '?'} cr{crf}.{name}"
        lk = i & 1
        aa = (i >> 1) & 1
        tgt = (aa and (bd + BASE)) or (addr + bd)
        return f"{nm}{'l' if lk else ''} 0x{tgt:08x}   (bo={bo},bi={bi},bd={bd:+#x})"
    if op == 18:
        d2 = i & 0x03FFFFFC
        if d2 >= 0x02000000:
            d2 -= 0x04000000
        lk = i & 1
        aa = (i >> 1) & 1
        tgt = (aa and (d2 + BASE)) or (addr + d2)
        return f"b{'l' if lk else ''}{'a' if aa else ''} 0x{tgt:08x}"
    if op == 19:
        xo = (i >> 1) & 0x3FF
        if xo == 16:
            return "bclr/bclrl" if rA == 0 else f"bclr variant rA={rA}"
        if xo == 528:
            return "bcctr/bctrl"
        return f"op19.x{x}"
    if op == 31:
        xo = (i >> 1) & 0x3FF
        n = OP31_NAMES.get(xo, f"x{xo}")
        if xo == 444:
            return f"mr r{rA},r{rB}" if rT == rA else f"or r{rA},r{rT},r{rB}"
        return f"op31 {n} r{rT},r{rA},r{rB}" + ("." if i & 1 else "")
    if op == 24:
        return f"ori r{rA},r{rT},0x{d:x}"
    if op == 25:
        return f"oris r{rA},r{rT},0x{d:x}"
    if op == 28:
        return f"andic. r{rA},r{rT},0x{d:x}"
    if op == 29:
        return f"andis. r{rA},r{rT},0x{d:x}"
    if op == 21:
        sh = (i >> 11) & 0x1F
        mb = (i >> 6) & 0x1F
        me = (i >> 1) & 0x1F
        return f"rlwinm r{rT},r{rA},{sh},{mb},{me}"
    if op == 20:
        return f"rlwimi r{rT},r{rA},..."
    if op == 23:
        return f"lwzx r{rT},r{rA},r{rB}"
    if op == 87:
        return f"lbzx r{rT},r{rA},r{rB}"
    if op == 3:
        sim = d - 0x10000 if d >= 0x8000 else d
        return f"twi {rT},r{rA},{sim}"
    return f"op{op} raw=0x{i:08x}"


def dump(start: int, end: int, tag: str) -> None:
    print(f"--- {tag} ---")
    for addr in range(start, end, 4):
        off = addr - BASE
        i = struct.unpack(">I", BLOB[off : off + 4])[0]
        print(f"0x{addr:08x}: {i:08x}  {dec(i, addr)}")


if __name__ == "__main__":
    # The strcpy site + the guards (context window)
    dump(0x10115400, 0x1011549C, "strcpy site + guards")
    # beq cr7 target: r24==0 path
    dump(0x1011549C, 0x10115510, "r24==0 target (+0x50 -> 0x10115494)")
    # beq cr4 (r22==0) target
    dump(0x1011564C, 0x10115700, "r22==0 target (0x1011564C)")
    # bne cr7 ([r26]!=0) far target
    dump(0x101156B0, 0x10115720, "r0!=0 far target (0x101156B0)")
