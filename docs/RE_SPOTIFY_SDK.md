# The 1112-byte record sink is the Spotify SDK track pipeline

Static read, S1 57.23-74170-1-16 `anacapad` (PPC). File offset = vaddr -
`0x10000000`. Answering the "1112-byte sink" question: what writes it, and can
a `:1400` request reach it.

## The object and the sink

`0x1021fb60` is a constructor. It allocates `0x14f8` bytes (`0x10222778`),
`memset`s them, and installs two callbacks:

```
0x1021fb9c  lis r9,0x1022 ; addi r9,r9,-0x708   ; 0x1022f8f8 -> obj+0x10
0x1021fbc0  lis r9,0x1022 ; addi r9,r9,-0x6308  ; 0x10229cf8 -> obj+0x14
0x1021fbd8  stw r0,0x1404(r10)                  ; 0xa
0x1021fbe4  stw r10,0x5d78(r9)                  ; singleton 0x10545d78
```

`obj+0x14` = `0x10229cf8` is the sink. It is a *resume* entry inside the SDK
state machine, not a function prologue: it sets `r26 = 2` and branches back to
`0x10229318` (`b 0x10229318` at `0x10229d0c`), so the caller must arrive with
the object in `r31` and the machine's state already loaded.

## The record is a track, not a generic blob

The size function `0x102490c8` is `li r3,0x458; blr` — 1112 bytes exactly. Its
caller `0x102490d0` compares the requested size against it (`cmplw r7,r3` at
`0x102490f0`, `bge` to the error at `0x102490fc`) and allocates through
`0x102194a0`. The strings in that range name the source files:

- `.../esdk/src/track_pipeline.c` (`0x10489a88`)
- `.../esdk/src/track.c` (`0x10489818`)
- `id: %u : -> UPCOMING` / `UPCOMING -> PLAYING` / `PLAYING->` (`0x10489af4`,
  `0x10489b0c`, `0x10489b2c`)

So the record is the SDK's per-track pipeline state (upcoming/playing/download),
and the sink is its state advance.

## Reachability: no `:1400` request reaches it

- Every caller of the `0x1022xxxx` range is itself in the SDK: the call graph
  out of `0x1022xxxx` never leaves `0x102xxxxx`.
- The constructor `0x1021fb60` is materialized at exactly one site,
  `0x102242b8` (`lis r8,0x1048 ; addi r8,r8,-0x4a0`), inside `0x10224254`.
  `0x10224254` has no direct `bl` and no pointer word — it is registered as a
  callback, like the rest of the module.
- The `:1400` route handlers all live at `0x4axxxx`/`0x4fxxxx`; the URI
  classifier that tests `x-sonos-spotify:` (`0x100a6a10`, `strncmp` at
  `0x100a6a2c`) is a 0x20-byte leaf with no SDK call.

The daemon therefore reaches the sink only through its Spotify glue -> the
SDK's module manager/event pump (`korn`, `.../esdk/src/korn/korn.c`), never
straight from a request. A `:1400` request can only influence it indirectly, by
changing what the daemon asks the SDK to play.

## Consequence for the exploit lane

A bug in this sink is reachable from the LAN only in the sense that the LAN can
choose the *track*; the corrupting bytes have to come from the Spotify side
(AP connection, OGG page, state response), not from the request body. The
neighbouring parser with a real length check is the OGG path: `0x1022abac`
copies `*(obj+6008)` bytes with no check in that function, and the module's own
string is `ogg page size %d is larger than buffer %d` (`0x10481020`).
