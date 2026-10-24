# HLS queue internals — PPC32 Unicorn harness (57.23-74170-1-16)

The live player no longer crashes on HLS, so the ring maths is only observable in
an emulator. This harness maps the anacapad image, builds a synthetic parent
object with the real field offsets, and calls the three queue functions with
chosen field values:

| address | role |
|---|---|
| `0x10191fbc` | append — `memcpy(slot, item, 3152)`, advances the index |
| `0x10192674` | compactor |
| `0x1019278c` | split |

Parent fields: `+0x00` pending/work byte, `+0x04` index, `+0x08`/`+0x0c` ring
counters, `+0x20` slot buffer, `+0xc508` "C", `+0xc50c` threshold, `+0xc510`
producer counter, `+0xc514` limit. Slots are 3152 bytes, ring is 16 slots.

## What the code actually does

Both entry points start with `lbz r0,0(r3)` / `cmpwi` / `bt` and **return
immediately when `+0x00` is zero**; both clear it on completion. So the byte is a
"work armed" flag, not a re-entrancy guard:

- setter: `0x10192614` (`stb 1,0(this)`, then seeds `this+0xc508` from `this+8`),
  called from `0x101908c8` in the HLS writer right after enqueue `0x101923e0`;
- clearers: the compactor at `0x10192718`, the split at `0x101927f8`.

With `+0x00` set, the compactor still needs `+0xc510 != 0` and `+0xc508 != 0`,
otherwise it falls into the early-exit shortcut at `0x101926e8`.

Body at `0x10192754`:

```
n   = *(this+0xc514) - *(this+0xc508)          # unsigned, wraps
C   = 16 - n                                    # stored to this+0xc508
dst = this + C*3152 + 8
src = this + h*3152 + 8
memcpy(dst, src, n*3152); *(this+0xc514) = 16
```

Split at `0x1019278c` prepares those inputs instead of moving data:

```
*(this+0xc508) = *(this+0xc514)                 # h := limit
*(this+0xc50c) = (*(this+0xc550) < *(this+0xc554)) ? *(this+0xc548) + limit : 0
*(this+0xc514) = 16
```
(on this object the `q` fields land at `+0xcd48/+0xcd4c/+0xcd50/+0xcd54`.)

## Trace table (emulated, `scripts/hls-queue-emu/hls_emu.py`)

| `+0` | `+0xc510` | h=`+0xc508` | lim=`+0xc514` | body | C after | dst |
|---|---|---|---|---|---|---|
| 0 | 1 | 5 | 40 | no | 5 | — |
| 1 | 0 | 40 | 40 | no | 40 | — |
| 1 | 1 | 40 | 40 | no | 40 | — |
| 1 | 1 | 0 | 16 | no | 0 | — |
| 1 | 1 | 5 | 21 | yes | **0** | in range |
| 1 | 1 | 5 | 22 | yes | **−1** (`0xFFFFFFFF`) | one slot *below* the object |
| 1 | 1 | 20 | 40 | yes | **−4** (`0xFFFFFFFC`) | four slots below |
| 1 | 1 | 16 | 16 | yes | 16 | in place |
| 1 | 1 | 28 | 16 | yes | **28** | 28 slots below |
| 1 | 1 | 32 | 16 | yes | **32** | 32 slots below |
| 1 | 1 | 20 | 16 | yes | 20 | wraps `n = 0xFFFFFFFC` |
| 1 | 1 | 5 | 40 | yes | **−19** (`0xFFFFFFED`) | 19 slots below, 110 320 B copy |

Split-then-compact (the reachable production shape):

| split q4/q50/q54/limit | C after split | compact runs | C after |
|---|---|---|---|
| 0/5/5/40 | 40 | yes (`x50c = 0`) | 40 |
| 0/5/6/40 | 40 | no (`x50c = 40`, equal to h) | 40 |
| 0/5/6/28 | 28 | no | 28 |
| 0/0/1/16 | 16 | no (`x510 = 0`) | 16 |

## Answers

- **C ≥ 28 or negative** — negative comes from `limit > h + 16` (`h=5, lim=22`
  is the smallest: C = −1; `h=5, lim=40` gives −19 and a 110 KB copy into slots
  below the object). C ≥ 28 is only reachable through the split: it sets
  `+0xc508 = limit`, so any split with `limit ≥ 28` followed by a compaction
  gives `C = limit` (28, 32, 40 measured). A split with `q50 < q54` sets
  `+0xc50c = q4 + limit`, which suppresses the following compaction.
- **Does the pending byte gate the compactor** — yes: `+0 == 0` makes both the
  compactor and the split no-ops, and each clears it. It is set by `0x10192614`
  from the writer at `0x101908c8`; the compactor alone additionally needs
  `+0xc510 != 0` and `+0xc508 != 0`.

## Run

```sh
~/.venv-re/bin/python hls_emu.py compact 1 5 22 1      # pending h lim x510
~/.venv-re/bin/python hls_emu.py split-then-compact 0 5 5 40
~/.venv-re/bin/python hls_emu.py append  1             # append x3 + compact
```

Mapping starts 0x80000 below the object so a negative C still has a destination;
a copy larger than 2 MB is counted but not performed.
