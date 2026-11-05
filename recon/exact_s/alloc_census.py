#!/usr/bin/env python3
"""Census of every heap-allocator call site in the 57.23 anacapad (PPC).

For each `bl malloc|calloc|realloc|free`, recover the size argument statically from the
preceding instructions: constants via li / lis+addi|ori, `base+N` expressions, or dynamic
(marked with the load/move that produced it). TSV out so an emulator run can be matched
to sizes without guessing.

Usage: alloc_census.py > census.tsv
"""
import capstone, struct, sys

BIN = "captures/firmware/57.23-74170-1-16/anacapad"
TO, TV, TS = 0x755C, 0x1000755C, 0x435638
TARGETS = {0x10546560: "malloc", 0x10546A68: "realloc", 0x10546C68: "calloc", 0x10546C88: "free"}

f = open(BIN, "rb").read()
md = capstone.Cs(capstone.CS_ARCH_PPC, capstone.CS_MODE_32 + capstone.CS_MODE_BIG_ENDIAN)


def raw_sites(targets):
    out = []
    for off in range(0, TS - 4, 4):
        w = struct.unpack(">I", f[TO + off:TO + off + 4])[0]
        if (w >> 26) != 18 or not (w & 1):
            continue
        d = w & 0x03FFFFFC
        if d & 0x02000000:
            d -= 0x04000000
        t = TV + off + d
        if t in targets:
            out.append((TV + off, targets[t]))
    return out


STARTS = sorted({a for a, _ in raw_sites({t: t for t in TARGETS})} | {TV})


def containing(a):
    lo, hi, best = 0, len(STARTS) - 1, STARTS[0]
    while lo <= hi:
        mid = (lo + hi) // 2
        if STARTS[mid] <= a:
            best = STARTS[mid]
            lo = mid + 1
        else:
            hi = mid - 1
    return best


def context(a, nbytes=56):
    start = max(TV, a - nbytes)
    ins = list(md.disasm(f[TO + start - TV:TO + a - TV], start))
    return ins


def reg_writes(reg, ins, depth=0):
    """Last writer of `reg` walking backwards: ('const',v) ('expr',base,off) ('dyn',src)."""
    steps = 0
    for k in range(len(ins) - 1, -1, -1):
        steps += 1
        if steps > 14:
            break
        a, m, o = ins[k].address, ins[k].mnemonic, ins[k].op_str
        parts = [p.strip() for p in o.split(",")]
        if m in ("li", "lis") and parts[0] == reg:
            v = int(parts[1], 0)
            if m == "li":
                return ("const", v)
            for j in range(k + 1, len(ins)):
                p2 = [p.strip() for p in ins[j].op_str.split(",")]
                if ins[j].mnemonic in ("addi", "ori", "addis", "oris") and p2[0] == reg and p2[1] == reg:
                    return ("const", (v << 16) + (int(p2[2], 0) & 0xFFFF))
            return ("const", v << 16)
        if m == "addi" and parts[0] == reg:
            base, off = parts[1], int(parts[2], 0)
            if depth < 2:
                sub = reg_writes(base, ins[:k], depth + 1)
                if sub[0] == "const":
                    return ("const", sub[1] + off)
            return ("expr", base, off)
        if m == "mr" and parts[0] == reg:
            if depth < 2:
                return reg_writes(parts[1], ins[:k], depth + 1)
            return ("dyn", "mr from " + parts[1])
        if m.startswith("lw") and parts[0] == reg:
            return ("dyn", "load " + o)
        if m.startswith("st") and parts[0] == reg:
            break
    return ("dyn", "?")


def main():
    print("site\tfn\tcallee\targ_kind\targ_value\tprev")
    for a, callee in raw_sites(TARGETS):
        ins = context(a)
        if not ins or ins[-1].address != a:
            # the bl itself may not decode cleanly from a-56; append it from raw words
            pass
        rw = reg_writes("r3", ins)
        val = rw[1] if len(rw) == 2 else "%s+%#x" % (rw[1], rw[2])
        prev = " | ".join("%s %s" % (i.mnemonic, i.op_str) for i in ins[-4:])
        print("%#x\t%#x\t%s\t%s\t%s\t%s" % (a, containing(a), callee, rw[0], val, prev))


main()
