# Packet-length copies in anacapad: top 5

Method: full disassembly of both builds (`llvm-objdump --triple=powerpc` for
57.23, `mips-linux-gnu-objdump -m mips:16` for 86.10), every `bl memcpy` /
`jal memcpy|memmove` site, then the length argument traced to its source and
the preceding 10-20 instructions read for a clamp. 491 sites on 86.10, 94 with
a field-loaded length on 57.23. Ranked by "length comes from the wire and the
destination is fixed".

## 1. 57.23 `0x1007b2c4` — URL path span copied with no clamp (the one finding)

Function entry `0x1007ad60` (`stwu 1,-5344(1)`), a URL splitter: `strchr` for
`/` at `0x1007ae34` and again at `0x1007ae48`, then

```
0x1007ae54  addi 29, 1, 3136        ; buffer
0x1007ae60  sub  9, 9, 24           ; span = second_slash - url_base
0x1007ae68  addi 9, 9, 1
0x1007ae70  stw  9, 5244(1)         ; span stored
0x1007ae74  li   5, 1025
0x1007ae7c  bl   0x10546ba8         ; strlcpy(tail, buf, 1025)  <- clamped
...
0x1007b2b8  lwz  5, 5244(1)         ; span again
0x1007b2bc  mr   4, 18              ; src = ORIGINAL url base (r18 = r4)
0x1007b2c0  mr   3, 31              ; dst = r1+16
0x1007b2c4  bl   0x10546ad8         ; memcpy(dst, src, span)  <- no clamp
0x1007b2d8  stb  30, 16(9)          ; NUL at dst+span
```

The 1025-byte `strlcpy` clamp is **bypassed**: the span is computed before it
and reused for the `memcpy`, which writes into a 5344-byte frame from `+16`.
Any URL whose path span exceeds ~5328 bytes overruns the saved registers and
the caller's frame; the 1025-byte buffer at `+3136` is not the destination, so
the clamp gives a false sense of safety. No `cmpw`/`cmplw` on the span exists
between `0x1007ae70` and the copy. Caller not yet identified (no direct `bl`
and no stored pointer to `0x1007ad64`), so the reachable surface is open.

## 2. 86.10 `0x5f59b4` — `sonos::json::Stream` string read: BOUNDED

Clamp immediately before the copy: `0x5f59a2` `addiu v1,-1`, `0x5f59a4`
`subu a3,v1,a1`, `0x5f59a6` `sltu s1,a3`, `0x5f59aa` `move a3,s1` — the length
is `min(capacity-1-offset, available)`. Copy at `0x5f59b4` uses `a3`.

## 3. 86.10 `0x4f80c4` — `RKVReportData::addPair`: BOUNDED

`0x4f80a2` `lw v0,0(s0)` (cap), `0x4f80a4` `subu a0,v0`, `0x4f80a6`
`sltu a0,v1` (needed = key+1+value+1), `0x4f80aa` `btnez` → skip when it does
not fit. Both copies (`0x4f80b8`, `0x4f80c4`) run only inside the fit.

## 4. 86.10 `0x4cd3ea` — JSON string append: BOUNDED

`0x4cd3ac` `addu v0,s1,v0`, `0x4cd3ae` `sltu a0,v0`, `0x4cd3b0` `btnez` →
grow path (`0x4cd3d2`), else the copy at `0x4cd3ea` with `a2 = s1` fits.

## 5. 86.10 `0x4aa6b2` / `0x4aa6ec` — `/notify` body and params: BOUNDED

Body read `4f5990(req, sp+32, 4096, 0, 2000)` at `0x4aa6b2`; the query parser
then gets 32/128-byte destinations at `0x4aa6ec` (`a2=32`, `a1=sp+4256`) and
`0x4aa6de` (`a2=32`, `a3=sp+4128`).

Also checked and bounded: devmode statement `memcpy` `0x4f3bc6` (length
`<1025` enforced at `0x4f3b56`), the strdup-style sites `0x49b802` and
`0x65a244/0x65a276` (strlen+1 then malloc), and the ring copy `0x5a8d06`
(`sltu` at `0x5a8cf6`/`0x5a8cfe`).

## Status

One unbounded packet-length copy (`0x1007b2c4`). The rest of the sampled
surface is careful; the parsers that matter most (JSON, SOAP args, GENA body)
clamp. Next: identify the caller of `0x1007ad64` (search for the address in
`.data.rel.ro` tables by offset, or break on it in the emulator) and craft a
>5.3 KB URL for it.
