# The 1112-byte record sink: what fills it, and whether :1400 reaches it

Lane LilacRaven, follow-up to the BoldOrchid thread in
[`FIRMWARE_PATHS.md`](FIRMWARE_PATHS.md). Binary: the PPC S2 `anacapad`
(`captures/research/ghidra-upd/anacapad`, `.text` at `0x1000755c`). All offsets are
file offsets equal to vaddrs.

## The copy and its missing check

`0x10219848` is the blob-append case of the record serializer. It writes into the
static buffer at `0x1054d5d0` (`addi r30, r11, -0x2a30` at `0x1021987c`) and the
only size test is **after** the copy:

```
10219950: add   3, 29, 30        ; dest = 0x1054d5d0 + (name_len + 7)
10219954: bl    memcpy           ; len = *(r31+0x448)
10219958: lwz  0, 0x448(r31)
1021995c: add  0, 29, 0
10219960: cmplwi 7, 0, 0x458     ; 1112 — logs only, does not stop the copy
```

So the destination is 1112 bytes and the copy is unchecked. The record is
`r31 = *(singleton+8) + 0xE58`; the name is at `r31+0`, the blob length at
`r31+0x448`, the blob at `r31+0x48` (`addi r4, r28, 0xea0` at `0x10219910`).

The path is selected by bit `0x80` of the record's type word at `r31+0x44`:
`andi. r9, r0, 0x80` at `0x102198ac` — set means "no blob", clear means "append it".

## Who fills the blob, and the cap

`0x10224c58` is the only writer of the length:

```
10224c30: lwz  9, 0x34(r28)      ; type/id
10224c34: cmpwi 7, 9, 0          ; 0 -> skip
10224c3c: lwz  0, 0x38(r28)      ; blob length
10224c40: cmpwi 7, 0, 0          ; <= 0 -> skip
10224c48: cmpwi 7, 0, 0x400      ; > 1024 -> skip
10224c58: stw  0, 0x448(r3)      ; length
10224c5c: lwz  5, 0x38(r28)
10224c60: stw  9, 0x44(r3)       ; type word (bit 0x80 lives here)
10224c68: bl   memcpy            ; dest = r3+0x48, src = r28+0x3c
```

The source is a parsed descriptor `{id @+0x34, length @+0x38, data @+0x3c}`. The
same function copies the record's name with `strlcpy(..., 0x41)` at `0x10224c2c`,
so 64 bytes.

## Arithmetic

`64 + 7 + 1024 = 1095 <= 1112`. **Through this writer the sink is safe by 17
bytes**, which is why the missing check has not bitten. The 1112 sink only
overflows if either the name or the blob exceeds those caps.

Open item, and the reason the sink is not closed: the serializer's name is at
`r31+0` (`strlen` at `0x102198e0`), but the writer's `strlcpy` target is a
different base (`r3 = *(conn+0x5c1c)`, name at `r3+0`). `+0xE58` is never written
by a `stw`/`stb` in the whole `.text` (two hits, both `stw ..., 0xE58(r1)` on a
stack frame), so the serializer's name field is filled through some other base.
Until that is resolved the 64-byte bound cannot be assumed for it.

## Does a :1400 request reach it

Yes. The module is on the UPnP/XML response path, not the cloud path:

- `0x10081800` calls the module entry `0x1021df7c` at `0x10081814` and then emits
  `kStorageMedia val="NONE, NETWORK"/>` (`0x1045fe80`, used at `0x10081828`).
- A second caller, `0x10082e5c`, also calls `0x1021df7c`.
- The record-type dispatcher in the serializer switches on `-0x70, -0x6f, -0x6d,
  -0x77, -0xc7, -0x64` at `0x1021a1d8`–`0x1021a214` (indices 8..12) with a default
  of `0x15`, i.e. a small closed set of UPnP record kinds, not a caller-chosen
  string.

So a LAN `:1400` request that produces one of those record kinds reaches the
sink; the blob is whatever the parsed descriptor carries, capped at 1024.
