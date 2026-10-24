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

## Reachability and fault thresholds

`C = 16 − (limit − h)` only leaves the 16-slot ring through **`limit`**, i.e. the
size of the batch the split is given. The split writes `h = limit` itself, so the
same batch size sets C directly: **C = limit** after the split, independent of
the ring state. Negative C without a split needs a compaction whose limit is
already more than 16 past the head.

| batch (=limit) | C | destination | copy |
|---|---|---|---|
| 1–16 | 0…16 | inside the ring | 1–16 slots |
| 17 | −1 (`0xFFFFFFFF`) | 1 slot below the object | 17 slots |
| 20 | −4 (`0xFFFFFFFC`) | 4 slots below | 20 slots |
| 28 | 28 | 28 slots below | 28 slots |
| 32 | 32 | 32 slots below | 32 slots |
| 35 | −19 (`0xFFFFFFED`) without split, 35 with | 19/35 slots below | 110 320 B |

Reachability conditions, all of which the emulator exercised: the pending byte
must be armed (writer `0x101908c8` → setter `0x10192614`) and `+0xc510` must be
non-zero; a split with `q50 < q54` writes `+0xc50c = q4 + limit` and suppresses
the following compaction, so `q50 == q54` is what keeps the C ≥ 28 path live.

### Recommended playlist for MagentaBarn

- **28–40 media segments in one enqueue burst** (`limit` ≥ 28 → C ≥ 28, dst at
  or past the ring edge); 40 gives the largest in-band C without a 4 GB wrap.
- Uniform 2 s segments, sequential `EXT-X-MEDIA-SEQUENCE`, no `EXT-X-DISCONTINUITY`
  inside the batch (a discontinuity reset re-arms `+0xc508` and loses the setup).
- Keep `q50 == q54` at the split (no `q4 + limit` term) so the compaction is not
  suppressed; let the writer re-arm `+0` after the split.
- Stay under 65 536 entries so the append path's 16-bit index is untouched.

## The decisive chain: split → shortcut → append → compact

Reachability first: the non-copy block at `0x101926f8` (capacity := limit) only
executes when **bit 31 of `+0xc510` is set** (producer −1 / INT_MIN shape) and
the cursor `+0xc508` is 0 — a normal 0…65535 counter takes the neighbouring
`0x10192700` branch instead. With those set the shortcut does `+0xc50c :=
+0xc514` and `+0xc514 := 16`; the cursor itself is left alone, so the append
after it goes to slot `+0xc50c`, not to 16.

`split(limit) → append at the cursor → compact` (cursor = limit from the split):

| batch = limit | h | n | **C** | dst = src | len | Unicorn |
|---|---|---|---|---|---|---|
| 16 | 16 | 0 | 16 | `obj+0xc508` | 0 | clean |
| 17 | 17 | −1 | **17** | `obj+0xd158` | `0xfffff3b0` | **write fault after 720 896 B** |
| 20 | 20 | −4 | **20** | `obj+0xf648` | `0xffffcec0` | **fault after 720 896 B** |
| 28 | 28 | −12 | **28** | `obj+0x158c8` | `0xffff6c40` | **fault after 655 360 B** |
| 40 | 40 | −24 | **40** | `obj+0x1ec88` | `0xfffed880` | **fault after 655 360 B** |

The `mulli` wraps: `n` is negative, so the length becomes `2³² − (N−16)` slots
(≈ 4 GiB), `dst == src == slot N`, and the copy walks off the object. The fault
offset above is bounded by this harness's mapping, not by the device; on the
player the same access runs into whatever follows the queue in the heap.

**C maps back one-to-one to the batch**: the split writes `h = limit` and the
body computes `C = 16 − n = limit`. Any observed C at this chain *is* the number
of segments the split was handed — 17 is the first fatal value, 16 the last
harmless one, 40 the largest without an additional pointer wrap.

### Final recommendation for MagentaBarn

- Aim at **28–40 segments in one burst** (C ≥ 28, and the huge-length copy is
  present from 17 upward). 16 alone is a no-op — do not use it as a control.
- Uniform 2 s segments, sequential `EXT-X-MEDIA-SEQUENCE`, **no**
  `EXT-X-DISCONTINUITY` in the batch, `q50 == q54` at the split.
- Producer/`+0xc510` should be a wrapped-to-negative value if the shortcut path
  is wanted; a small positive counter skips `0x101926f8` and only gives the
  bounded `n > 16` variant.
- Repro: `hls_emu.py split-append-compact 17`.
