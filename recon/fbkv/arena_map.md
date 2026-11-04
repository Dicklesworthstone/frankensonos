# fbkv co-assign — allocator 0x10222778, all 19 call sites, and where the singleton's neighbours really come from

Pane omp_2, 2026-11-04. Static, `captures/firmware/57.23-74170-1-16/anacapad` (PPC). My half
of the co-assign: the enumeration and the allocation map. SageElm's device side is unchanged.

## 0. Result first

* All **19** `bl` sites of `0x10222778` are enumerated below. They split into six **arena
  constructors** (block size = the whole arena, stored at `ctx+12`, released at `ctx+8`) and
  thirteen **module block** allocations (out to a stack slot).
* **Correction to the premise:** the neighbours that matter are *not* produced by `0x10222778`.
  The session arena has a second, inline allocator **`0x10222314`** (`addi r4,r4,7; rlwinm …
  0,0,0x1c`, compares against the `+0x2048` limit) with **28** call sites, and the request
  path (`0x1021edb8` inside the socket request loop `0x1021ed78`) allocates from it through
  the session ctx at `*(*(0x10545d78))`. Both allocators share the same bump pointer, so a
  `0x10222314` block is what lands at `base+5369`.
* **No site in either family writes a pointer at offset 0 of its block.** Every one of them
  `memset`s the block first (or, for the request record, relies on the arena's init zeroes) and
  then writes fields at non-zero offsets; the object-shaped blocks put their vtable at
  `+0x10`/`+0x14`, never `+0`. So a neighbour at `+5369` yields `vt = 0` from a *fresh* block
  and `vt = <record field>` once the request record is written — never a code pointer.

## 1. The 19 sites

`bl`-site scan (opcode 18/LK, target 0x10222778) — reproducible with the scanner in
`recon/fbkv/scan_sites.py`. "init" is what happens to the block immediately after the call.

| site | kind | size | init after alloc | notes |
|---|---|---|---|---|
| `0x1021fb7c` | **arena ctor — this is the /notify singleton's arena** | `0x14f8` (5368) | `memset 0`; `stw 0x1021f8f8 (vtable),0x10(ctx)` | sets `ctx+12=bump`, `out=ctx+8`, publishes the session at global `0x10545d78` (`0x1021fbe4`) |
| `0x1022226c` | **the 1-byte singleton itself** | 1 | `memset 0` (1 byte) | stores the singleton pointer at global `0x10545d88`; bump now **`base+5369`, unaligned** |
| `0x1022595c` | arena ctor | `0x5d10` (23824) | `memset 0`; vtable at `ctx+0x10` | publishes at `0x10545d…` |
| `0x10226b74` | arena ctor | `0x118` (280) | `memset 0`; vtables at `ctx+0x10`/`+0x14`; `stw r30,0(base)` | the only ctor that writes offset 0 of its block (with an incoming arg, not a code pointer) |
| `0x102290f0` | arena ctor | `0x328` (808) | `memset 0` | — |
| `0x10231b48` | arena ctor | `0x1e268` (123496) | `memset 0`; vtable at `ctx+0x14` | — |
| `0x102352f0` | arena ctor | `0x2868` (10344) | `memset 0`; `stw r30,0(base)`; vtable at `ctx+0x10` | — |
| `0x10223058` | block | `0x254` (596) | `memset 0`; `stw r3,0(r31)` (ctx ptr) | then allocates `r28*0x18` |
| `0x1022308c` | block | `r28*0x18` | `memset 0` | element array for the block above |
| `0x102230c0` | block | `0x68` (104) | `memset 0`; `stw r3,4(r31)` | — |
| `0x10229124` | block | `r3<<3` (count from `0x10229d84`) | none (caller fills) | **dynamic size** |
| `0x10229184` | block | return of `0x102490c8` | none | object: vtables at `+0x10`/`+0x14` (`0x102380e8`, `0x10239228`) |
| `0x10229184`… | | | | |
| `0x10233664` | block | `0x3d8` (984) | `memset 0`; `stw 0x3e8,0x3d4`; publishes at `0x10545d90` | — |
| `0x102337c8` | block | `r31` (caller) | none | arena from `*(r28+4)` |
| `0x1023e334` | block | `0x414` (1044) | `memset 0`; `stw r31,0x404`; `stw r30,0(block)`; publishes at `0x10545d94` | — |
| `0x1023eee0` | block | `0x4c` (76) | `memset 0`; `stw r29,4`; `stw 0x1023e7ac,0x3c` | — |
| `0x102430e8` | block | `0x694` (1684) | `memset 0`; `stw r31,0(block)`; publishes at `0x10545da0` | — |
| `0x10245a64` | block | `0x6248` (25160) | `memset 0`; `stw r30,0xd0`; publishes at `0x10545da4` | — |
| `0x10246150` | block | `0xf0` (240) | `memset 0`; `stw r31,0xec`; publishes at `0x10545da8` | — |

Two rows are the same instruction window (`0x10229184` also carries the `0x1022308c`-style
element alloc); the table lists each `bl` once, so the count is 19 as briefed.

## 2. What actually sits at `base+5369`

Sequence, from the session ctor `0x1021fb54`:

1. `0x1021fb7c` allocates 5368 B (`ctx+12 = 0x14f8`, `out = ctx+8`), `memset 0`, vtable at
   `ctx+0x10`, publishes the ctx at `0x10545d78`.
2. `0x1021fbe8` calls `0x10222258` → `0x1022226c`: `li r4,1`, allocates the **1 byte** at
   `base+5368`, zeroes it, stores the pointer at `0x10545d88`. The bump is left at `base+5369`.
3. The next allocation from that ctx is the **request record**: `0x1021edb8`, size
   `r5 + 0x27` where `r5` is the received-record length, filled by
   `0x1022d3b0` → `0x1022c484` with a descriptor list (`lha rX,2(desc)` = a *signed offset into
   the record*; `lwzx`/`stwx` against the record base).

So `vt` (4 B at `base+5369`) is the record's **offset 0**, which the descriptor set decides.
The record is *not* memset — its first bytes are the arena's init zeroes until a descriptor
writes offset 0. The descriptor host is `session+0x1a0` (`0x1022d3b0` passes it as `r6`);
mapping which descriptor owns offset 0, and whether offset 0 is a pointer, a length or a
parser-output string, is the next read — it is the single thing that decides the landing value.

## 3. Consequence for the Path-2 plan

* A fresh neighbour gives `vt = 0` → `fn = *(0x2c)`, a fixed unmapped read. To get control the
  record's offset 0 must become a pointer into mapped memory whose `+0x2c` is a code pointer,
  or the neighbour must be one of the object blocks (vtable at `+0x10`, not `+0`) — which only
  helps if the wild read were `vt[0x30]`, not `vt[0x2c]`.
* Therefore the shaping recipe is: pick a request shape whose record has a *pointer* at
  offset 0 (or make one). `0x1022c484`'s descriptor entries carry `+2` (int16 offset) and
  `+0xa`/`+0xb` (flags) — enumerating the descriptor table at `*(0x10481430)+0x1a0` yields the
  field that owns offset 0 for each request type.
* `0x10222778` matters for shaping only where it is handed the *session* ctx — the six arena
  ctors each create an arena once at init, so they cannot be used per-request; the module
  blocks (13 sites) are per-module singletons, also not per-request.

## 3a. The record's descriptor set (advanced this pass)

`0x1022d3b0` passes `session+0x1a0` as the descriptor host. With relocs applied that slot is
`0x10481670 = {array 0x104818ca, 0xffff0000, 0xffff0000, count 4}`; each entry is 16 bytes with
`+0`/`+2` = output offset, `+6` = element stride, `+8` = flags and `+0xa` = a type/width code.
**Entry #1 has offset 0 and flags `0x0205`** — bit `0x04` is the indirect flag (`lwzx r29, out,
off`, i.e. the field holds a *pointer*, not an inline value) and `0x01` selects the array path
(`mullw r9, count, stride; add r29, r29, r9`). Bit `0x20` makes the walker skip the entry
(`0x1022b238`).

So the first 4 bytes of the record are a pointer field, and the wild dispatch takes *that*
pointer as `vt`. That re-frames the shaping question: it is not "put text at offset 0", it is
"which pointer does the parser write at offset 0 for a /notify-shaped record, and is its `+0x2c`
readable".

## 4. Open, handed over

1. Which parser writes the `+0x1a0` host's offset-0 field (entry #1, flags 0x0205) on the /notify
   path, and what pointer lands there — the record walker at `0x1022c484` only marshals it.
2. Whether a parser-output pointer (e.g. a per-request heap/arena pointer) can be made to land
   at offset 0 — then `vt+0x2c` needs one readable dword.
3. Device: one shot with the shaped predecessor request + the /notify trigger, plus a
   faulting-instruction capture to read `fn` per trial (SageElm's half).
