#!/usr/bin/env python3
"""Static boot-path ordering of heap-allocator call sites (57.23 anacapad, PPC).

DFS over direct `bl` edges from the entry point; allocator sites are emitted in the order
their containing functions are first entered. Indirect calls (bctrl through a vtable) break
the chain, so this is an *approximation* of the runtime order that the emulator must confirm —
but it fixes the ordering of everything statically reachable and lists the rest separately.

Usage: boot_order.py [entry_hex] > boot_order.tsv
"""
import struct, sys

BIN = "captures/firmware/57.23-74170-1-16/anacapad"
TO, TV, TS = 0x755C, 0x1000755C, 0x435638
TARGETS = {0x10546560: "malloc", 0x10546A68: "realloc", 0x10546C68: "calloc", 0x10546C88: "free"}

f = open(BIN, "rb").read()


def bl_edges():
    """site -> target, for every bl inside .text (direct calls only)."""
    out = {}
    for off in range(0, TS - 4, 4):
        w = struct.unpack(">I", f[TO + off:TO + off + 4])[0]
        if (w >> 26) != 18 or not (w & 1):
            continue
        d = w & 0x03FFFFFC
        if d & 0x02000000:
            d -= 0x04000000
        site = TV + off
        tgt = site + d
        if TV <= tgt < TV + TS:
            out.setdefault(tgt, []).append(site)
    return out


def containing(a):
    lo, hi, best = 0, len(SRT) - 1, SRT[0]
    while lo <= hi:
        mid = (lo + hi) // 2
        if SRT[mid] <= a:
            best = SRT[mid]; lo = mid + 1
        else:
            hi = mid - 1
    return best


def main(entry=TV):
    calls_from = {}
    alloc_at = {}
    for off in range(0, TS - 4, 4):
        w = struct.unpack(">I", f[TO + off:TO + off + 4])[0]
        if (w >> 26) != 18 or not (w & 1):
            continue
        site = TV + off
        d = w & 0x03FFFFFC
        if d & 0x02000000:
            d -= 0x04000000
        tgt = site + d
        fn = containing(site)
        if tgt in TARGETS:
            alloc_at.setdefault(fn, []).append((site, TARGETS[tgt]))
            continue
        if not (TV <= tgt < TV + TS):
            continue
        calls_from.setdefault(fn, []).append((site, tgt))

    seen = set()
    order = []
    stack = list(reversed(ROOTS if ROOTS else [entry]))
    while stack:
        cur = stack.pop()
        if cur in seen:
            continue
        seen.add(cur)
        for site, callee in alloc_at.get(cur, []):
            order.append((len(order), site, cur, callee))
        for site, tgt in reversed(calls_from.get(cur, [])):
            if tgt not in seen:
                stack.append(tgt)

    print("order\tsite\tfn\tcallee")
    for row in order:
        print("%d\t%#x\t%#x\t%s" % row)
    reached = {r[2] for r in order}
    unreached = [(s2, c) for fn2, lst in alloc_at.items() if fn2 not in reached for s2, c in lst]
    sys.stderr.write("reachable functions: %d, alloc sites: %d, unreached sites: %d\n"
                     % (len(seen), len(order), len(unreached)))


SRT = set()
for off in range(0, TS - 4, 4):
    w = struct.unpack(">I", f[TO + off:TO + off + 4])[0]
    if (w >> 26) == 18 and (w & 1):
        d = w & 0x03FFFFFC
        if d & 0x02000000:
            d -= 0x04000000
        t = TV + off + d
        if TV <= t < TV + TS:
            SRT.add(t)

ROOTS = [int(x, 16) for x in (sys.argv[2].split(",") if len(sys.argv) > 2 else [])]
SRT = sorted(SRT | set(ROOTS) | {TV})
main(int(sys.argv[1], 16) if len(sys.argv) > 1 else TV)
