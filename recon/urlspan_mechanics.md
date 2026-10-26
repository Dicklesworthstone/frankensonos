# URL-span stack overflow — mechanics (BoldOrchid)

Site: `0x1007b2c4` in anacapad 57.23-74170-1-16 (file offset `0x7b2c4`).

```
1007ae60: sub 9, 9, 24        ; len = X - 24
1007ae68: addi 9, 9, 1        ; len = X - 23
1007ae70: stw 9, 5244(1)      ; stash len on the stack
...
1007b2b4: addi 31, 1, 16      ; dst = r1 + 16
1007b2b8: lwz 5, 5244(1)      ; len from the stash
1007b2c0: mr 3, 31
1007b2c4: bl memcpy           ; NO bound
```

- Destination is the stack frame at `r1+16`.
- Length is `X-23`, where `X` is the parsed URL span — no cap anywhere.
- Frame is 5344 bytes, so usable space below the saved state is `5344-16 = 5328`.
- `X > 5351` writes past the frame into saved registers and the saved LR.

Live behaviour on .202 matches: 404 at N=1000/3000, 408/000 at N=5300-6000.

## Why it is pre-auth

The splitter runs before any credential check on the URL path, so the write happens
for an unauthenticated request.

## Open

Exact saved-register offsets (which N lands on which saved GPR and on the LR) need
the function prologue's `stmw`/`stw` offsets; the prologue was not located in the
time available, and the frame size did not appear as a literal `stwu -5344`.

## Clobber map (prologue at 0x1007ad60)

The splitter **is** the function: `0x1007ad60: stwu 1, -5344(1)`. Saved-register
offsets from the prologue, and the URL length `X` that reaches each (dst is
`r1+16`, `len = X-23`, so `X = offset - 16 + 23`):

| saved | stack offset | bytes written | URL length X |
|---|---|---|---|
| r18 | 5288 (0x14A8) | 5272 | 5295 |
| r24 | 5312 (0x14C0) | 5296 | 5319 |
| r26 | 5320 (0x14C8) | 5304 | 5327 |
| r27 | 5324 (0x14CC) | 5308 | 5331 |
| r28 | 5328 (0x14D0) | 5312 | 5335 |
| r30 | 5336 (0x14D8) | 5320 | 5343 |
| **LR** | **5348 (0x14E4)** | **5332** | **5355** |

So `X = 5355` is the first length that reaches the saved LR, and `X = 5335`
already corrupts saved `r28`. The live 408/000 at N=5300-6000 brackets this
exactly: 5300 is below r18, 5355 is the LR.

Note `mfcr`/`stw 12` is not present — the CR is saved via `mfcr 12` and stored
elsewhere, so the map above is the complete saved-GPR set.

## 2.1c clobber map (prologue 0x101cc878)

Second stack write, same primitive, different frame: `0x101cc878: stwu 1, -2160(1)`,
destination `r1+16`. Saved offsets from the prologue, and the span that reaches each
(`span = offset - 16`):

| saved | offset | span |
|---|---|---|
| r21 | 2116 | 2100 |
| r22 | 2120 | 2104 |
| r23 | 2124 | 2108 |
| r24 | 2128 | 2112 |
| r25 | 2132 | 2116 |
| r26 | 2136 | 2120 |
| r27 | 2140 | 2124 |
| r28 | 2144 | 2128 |
| r29 | 2148 | 2132 |
| r30 | 2152 | 2136 |
| r31 | 2156 | 2140 |
| **LR** | **2164** | **2148** |

So a browse-result span of 2148 bytes reaches the saved LR — 1207 bytes shorter
than the URL path needs (`X=5355`). The saved set is dense here (`r21`-`r31`
contiguous), unlike the URL splitter's sparse set, so the aim window is cleaner.
