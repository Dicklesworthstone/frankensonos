#!/usr/bin/env python3
"""Compute S = the audible-session payload address from an ordered allocation record.

Input: TSV, one call per line, in execution order:  size <TAB> callee <TAB> site
       (callee in malloc|calloc|realloc|free; `free` rows carry the *freed* pointer's
        owning size only as a comment and are handled as top-chunk givebacks or no-ops)

Model (see dlmalloc_policy.md): glibc-2.3-era ptmalloc2 on 32-bit, no ASLR.
  header 8 bytes, alignment 8, MINSIZE 16, request2size(req) = max(16, (req + 11) & ~7).
  The heap is a linear carve from brk_base; only the sum of chunk sizes matters.

Usage: s_model.py record.tsv [--session-site 0x...]
Prints S and the ±8 window. Rows after the session row are ignored for S.
"""
import sys

BRK_BASE = 0x1064E000
MINSIZE = 16


def request2size(req):
    if req >= 0xFFFFFFF0:
        raise ValueError("absurd request %#x" % req)
    return max(MINSIZE, (req + 4 + 7) & ~7)


def main(path, session_site=None):
    top = BRK_BASE
    carved = 0
    rows = 0
    session_addr = None
    pending_free = 0
    for line in open(path):
        line = line.strip()
        if not line or line.startswith("#") or line.lower().startswith("size"):
            continue
        f = line.split("\t")
        size = int(f[0], 0)
        callee = f[1] if len(f) > 1 else "malloc"
        site = f[2] if len(f) > 2 else ""
        rows += 1
        if callee == "free":
            # only a free of the current top chunk changes the layout; the record marks
            # those with site == "top" (other frees are no-ops this early)
            if site == "top":
                top -= pending_free
                pending_free = 0
            continue
        if session_site and site.lower() == session_site.lower() and session_addr is None:
            session_addr = top + 8
            session_size = size
            break
        top += request2size(size)
        carved += 1
    if session_addr is None:
        session_addr = top + 8
        session_size = None
    print("rows\t%d" % rows)
    print("carved_chunks\t%d" % carved)
    print("S\t%#x" % session_addr)
    print("S-8\t%#x\tlower bound (chunk rounding)" % (session_addr - 8))
    print("S+8\t%#x\tupper bound (chunk rounding)" % (session_addr + 8))
    if session_size is not None:
        print("session_size\t%#x" % session_size)
    print("top_after\t%#x" % (session_addr + (request2size(session_size) if session_size else 0)))


if __name__ == "__main__":
    main(sys.argv[1], sys.argv[3] if len(sys.argv) > 3 else None)
