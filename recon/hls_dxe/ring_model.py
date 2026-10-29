#!/usr/bin/env python3
"""dxe: pure-Python model of the 57.23 HLS segment ring (bead frankensonos-dxe).

Functions modeled from docs/RE_HLS_EXPLOIT.md field/dispatch tables:

  append(rec)   FUN_10191fbc
      guard: if limit >= capacity: return 0
      memcpy(array + cursor*0xc50, rec, 0xc50)
      cursor = FUN_101913fc(cursor, capacity)   # (cursor+1) % capacity when cursor < capacity
      limit += 1

  compaction()  FUN_10192674   (runs only when parent+0 pending flag != 0; flag writer
      0x10192614 is unreferenced on this build -> flag stays 0; model still tracks it)
      head_, cap_ = head, capacity
      C = cap_ - head_
      if C > 0: memcpy(parent + 8 - (C-16)*0xc50, parent + head_*0xc50 + 8, C*0xc50)
                head = 16 - C
      else (non-copy branch):
                if head == 0 and C <= 0: cursor = capacity; capacity = 16
      clears flag

  split()       FUN_1019278c   (only when pending flag != 0)
      sl, sc = sibling.limit, sibling.count
      limit = sl
      head = capacity            # OLD capacity
      cursor = (sibling.cursor + old_capacity) if sl < sc else 0
      capacity = 16
      clears flag

  reset()       0x101923e0 (queue reset at SetAVTransportURI)
      head=0 cursor=0 limit=0 capacity=16

  big-capacity phase: appends keep `limit` growing; capacity stays 16 unless a split
  sets it. The split's `head := old capacity` means a LARGE capacity at split time
  (e.g. 40 from a previous split with capacity:=sibling_cursor+old) is the only way
  head ends > 16 -> C = 16 - head < 0.

The device-observable knob: MEDIA-SEQUENCE advances drive reloads; N (segments per
window) drives appends; the split fires on the parser path we know is dead on this
build (r18 pinned), so this model searches *assuming* the split fires (upper bound
on reachability) and reports which parameter sets reach C>16 / C<0.
"""

C550 = 0xc50

class Ring:
    def __init__(self, name="R", base=8):
        self.name = name
        self.base = base            # offset of array base in the parent object
        self.head = 0
        self.cursor = 0
        self.limit = 0
        self.capacity = 16
        self.flag = 0
        self.copies = []            # (dst_off, src_off, nbytes)
        self.trace = []

    def snapshot(self):
        return (self.head, self.cursor, self.limit, self.capacity, self.flag)

    def restore(self, s):
        self.head, self.cursor, self.limit, self.capacity, self.flag = s

    # ---- FUN_10191fbc ---------------------------------------------------
    def append(self, rec_head, rec_cursor, rec_limit, rec_capacity):
        """Write rec (first 0x10 bytes land on the header when cursor==16)."""
        if self.limit >= self.capacity:
            self.trace.append("append REFUSED (limit %d >= capacity %d)"
                              % (self.limit, self.capacity))
            return 0
        dst = self.base + self.cursor * C550
        if self.cursor == 16:      # writes the ring header itself
            self.trace.append("append SLOT16: header := rec(head=%#x cur=%#x lim=%#x cap=%#x)"
                              % (rec_head, rec_cursor, rec_limit, rec_capacity))
            self.head = rec_head
            self.cursor = rec_cursor
            self.limit = rec_limit
            self.capacity = rec_capacity
        else:
            self.trace.append("append slot %d" % self.cursor)
        self.cursor = (self.cursor + 1) % self.capacity
        self.limit += 1
        return 1

    # ---- FUN_10192674 ---------------------------------------------------
    def compaction(self):
        if self.flag == 0:
            self.trace.append("compaction skipped (flag 0)")
            return
        head_, cap_ = self.head, self.capacity
        C = cap_ - head_
        self.trace.append("compaction C=%d (head=%d cap=%d)" % (C, head_, cap_))
        if C > 0:
            dst = 8 - (C - 16) * C550
            src = self.base + head_ * C550 + 8
            n = C * C550
            self.copies.append((dst, src, n))
            self.trace.append("  copy dst=%#x src=%#x len=%#x" % (dst, src, n))
            self.head = 16 - C
        else:
            # non-copy branch; the cursor:=capacity store only fires with head==0
            # and C<=0, which forces capacity<=0 -> dead in practice
            if head_ == 0:
                self.trace.append("  non-copy branch; cursor:=capacity dead store")
        self.flag = 0

    # ---- FUN_1019278c ---------------------------------------------------
    def split(self, sibling):
        if self.flag == 0:
            self.trace.append("split skipped (flag 0)")
            return
        sl, sc, scur = sibling.limit, sibling.capacity, sibling.cursor
        old_cap = self.capacity
        self.limit = sl
        self.head = old_cap
        self.cursor = (scur + old_cap) if sl < sc else 0
        self.capacity = 16
        self.flag = 0
        self.trace.append("split: sl=%d sc=%d scur=%d -> head=%d cursor=%d capacity=16"
                          % (sl, sc, scur, self.head, self.cursor))

    # ---- 0x101923e0 ------------------------------------------------------
    def reset(self):
        self.head = self.cursor = self.limit = 0
        self.capacity = 16


class Sibling:
    def __init__(self, cursor=0, limit=0, count=16):
        self.cursor = cursor
        self.limit = limit
        self.capacity = count   # S+0xc is 'count' per doc (used as sc)


def big_capacity_phase(r, n, m):
    """n appends x m rounds with compactions between: drive limit/cursor upward.
    Returns rounds completed. Note: capacity stays 16 on this build, so the only
    lever the ring itself exposes is the split's head:=old_capacity."""
    rounds = 0
    for _ in range(m):
        for _ in range(n):
            if not r.append(0, 0, 0, 16):
                break
        r.flag = 1
        r.compaction()
        rounds += 1
        if r.limit >= r.capacity:
            break
    return rounds


def search(max_cap=64, verbose=False):
    """Enumerate: reset -> big-capacity phase (capacity grows only via splits) ->
    split with sibling states -> appends -> compaction. Find any path where the
    post-append compaction sees C>16 or C<0."""
    hits = []
    for old_cap_phase in range(17, max_cap + 1):
        # pretend a first split left capacity = old_cap_phase (model the phase)
        for scur in range(0, 17):
            for sl in range(0, old_cap_phase + 1):
                for sc in (sl + 1, 16):
                    if sl >= sc:
                        continue
                    r = Ring()
                    sib = Sibling(cursor=scur, limit=sl, count=sc)
                    # phase 1: capacity becomes old_cap_phase via a prior split
                    r.capacity = old_cap_phase
                    r.flag = 1
                    r.split(sib)
                    # after split: head = old_cap_phase, capacity = 16
                    # next compaction: C = 16 - old_cap_phase (<0 when old_cap>16)
                    r.flag = 1
                    r.compaction()
                    # find any C>16 or C<0 in this sequence
                    for t in r.trace:
                        if "compaction C=" in t:
                            c = int(t.split("C=")[1].split(" ")[0])
                            if c > 16 or c < 0:
                                hits.append((old_cap_phase, scur, sl, sc, c, list(r.trace)))
                                if verbose:
                                    print("HIT cap_phase=%d scur=%d sl=%d sc=%d -> %s"
                                          % (old_cap_phase, scur, sl, sc, t))
                    if len(hits) > 3:
                        return hits
    return hits


if __name__ == "__main__":
    hits = search(verbose=True)
    print("total hits:", len(hits))
    if hits:
        cap, scur, sl, sc, c, tr = hits[0]
        print("\n=== first hit trace (cap_phase=%d scur=%d sl=%d sc=%d) ===" % (cap, scur, sl, sc))
        for line in tr:
            print(" ", line)
