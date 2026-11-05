# exact-S — allocator policy model and the contract with the emulator

Half of `[DECISIVE] compute S exactly via emulator` (CalmHawk, mail 3807). SageElm owns the PPC
harness run; this half is the static census and the arithmetic. **No device involvement.**

## Fixed geometry (given, no ASLR on the 2.4.25 kernel)

- `.bss` ends at `0x10548000 + 0x105ecc = 0x1064DECC`; `.comment`/`LOAD2` tail is the same address.
- glibc `brk` therefore starts at `0x1064DECC`, and the first `malloc` chunk begins at the
  page-aligned break **`0x1064E000`**.
- Chunk geometry is ptmalloc2/glibc-2.3-era dlmalloc, 32-bit: `prev_size` + `size` = **8-byte
  header**, payload starts at chunk+8, `MALLOC_ALIGNMENT` 8, `MINSIZE` 16,
  `request2size(req) = max(16, (req + 4 + 7) & ~7)`.
- `sysmalloc` extends the top chunk by `nb + TOP_PAD` (TOP_PAD default **128 KiB**) rounded up to
  the 4096-byte page. Every request before the first `free` is a plain carve from the top, so the
  payload address of request *i* is
  `S_i = 0x1064E000 + 8 + Σ_{j<i} request2size(size_j)`.

The stream has no `free` on the SetAV path before the audible session is constructed
(`census.tsv` marks the 245 `free` sites; the boot prefix's only frees are the shutdown path).

## The model

`s_model.py` takes the ordered record (one line per call: `size<TAB>callee<TAB>site`) and prints
`S` — the payload address of the designated row — plus the ±8-byte window from chunk rounding.
Rows marked `malloc`/`calloc` carve; `realloc` of a live top chunk can extend in place (treat as
carve for the upper bound and in-place for the lower); `free` of the *top* chunk returns the space
(coalesced into the top, so it is subtractive) and any other `free` is a no-op for the layout
before the first reuse.

Because the model is linear, only the *sum* matters: two adjacent sizes swapped give the same S,
so the record only has to be right about which sizes occur on the path, and right in order about
the session ctor row.

## Contract with the harness

1. Hook `malloc`/`calloc`/`realloc` (PLT `0x10546560` / `0x10546c68` / `0x10546a68`) and log
   `(site=LR, callee, size)` — the census maps every site to its static size, so a mismatch
   between hook size and census size is a defect in one of the two and must be reported, not
   smoothed over.
2. Return `S_i = top; top += request2size(size)` and record `top` per call.
3. The session object is the block whose *initialiser* is `0x10103598`: it writes `stb 27,
   6380(this)`, `stb 27, 7405(this)`, `stb 27, 16626(this)`, `stb 27, 17651(this)` and
   `stw 0, 21748(this)` (verified at `0x10103914`, `0x1010391c`, `0x1010392c`, `0x10103930`,
   `0x10103734`). **S = that block's payload address.**
4. Cross-check the returned pointer against the proven relations: `S+6380` = the strcpy
   destination, `S+30000` = the observer cell, `S+30028` = the resume counter.
5. Second control: run the same path in a *second* process and require the identical `S`.

## Known gap

`0x10103598` is not reachable statically: no `bl`/`b` targets it, and it is stored neither in
`.rodata`, `.data`, `.sdata`, `.got2`, `.ctors` nor as a `lis/addi` pair (searched both with a
base register of every kind). The audible class vtable `0x1046e960` that the initialiser installs
is equally absent from the data sections. Both must be reconstructed at run time from a table the
static scan has not located — so the harness must identify the session block by the *initialiser's
field writes* (item 3), not by a vtable or a call site.

## Boot-prefix measurement (first static pass)

`boot_prefix.tsv` is the allocator record of the static-constructor subtree, produced by DFS over
direct `bl` edges from the ten `.ctors` entries in table order (`boot_order.py`). Result: the boot
prefix allocates almost nothing — four constant requests (1, 8, 200, 2048) plus a handful of
dynamic ones (string builders at `0x1043c538`/`0x1043c690`/`0x104347a4`, `realloc` at
`0x10439650`, a 1-byte at `0x10434678`). Constants alone put a hard floor at
**S ≥ 0x1064E900** (0x1064E000 + 8 + 16 + 16 + 208 + 2056).

So S is dominated by the SetAV request path, not by boot; the harness must still produce that
tail, and the session row's position in it is what matters. 416 of the 442 census sites are
outside the boot prefix (request-path), which is where the tail's sizes come from.

## Static gaps to keep in mind

- The constructor chain is table-driven and mostly indirect: only 26 allocator sites are
  statically reachable, so the DFS order is a prefix, not the full record.
- `0x10103598` (session initialiser) and the class vtable `0x1046e960` are built at run time; no
  static reference exists (searched `.rodata`, `.data`, `.sdata`, `.got2`, `.ctors`, and every
  `lis/addi` base-register form). Identify the block by the initialiser's field writes.
