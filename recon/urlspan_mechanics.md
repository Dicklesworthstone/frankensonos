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
