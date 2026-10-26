# The 1112-byte record sink: which record carries a blob, and how it is reached

Singleton `0x10545d78`, constructor `0x1021fb60`, sink pointer stored at
`obj+0x14` = `0x10219cf8` (ctor `0x1021fbd4`). The sink switches on
`*(record)` (types 2..77).

## The blob-carrying record is type 77

Case `0x1021a9f4` (entered from the type-77 compare at `0x10219d18`):

```
0x1021a9f8  lwz 29, 23928(30)      ; the singleton itself
0x1021aa00  lwz 31, 1744(29)       ; second blob = a pointer field
0x1021aa04  addi 28, 29, 702       ; first blob = inline staging slot
0x1021aa0c  cmpwi 7, 31, 0         ; no pointer -> skip
0x1021aa58  bl 0x10546ad8          ; memcpy(obj+5133, obj+702, 33)
0x1021aa6c  bl 0x1023b55c          ; strlcpy(obj+5166, *(obj+1744), 128)
```

So type 77 carries a **33-byte inline blob** and a **128-byte string via a
pointer**. The staging slot is one of two 98-byte slots selected by
`0x10069550` (`+702` / `+800`, chosen by a bool).

## How the blob gets there

`0x100b7c14` is the filler: it reads from a stream virtual in
`min(256, remaining)` chunks into a 256-byte stack buffer (`0x100b7c60`,
`0x100b7c94`) and hands each chunk to `0x1019d068`, which writes it into the
staging slot; the loop ends when the reader is drained. Its callers are the
record parser `0x100b7d30` (called at `0x100b8004` and `0x100b80c0` with the
slot and length in r6/r7).

That parser is dispatched by **URI scheme**: `0x101a7560` compares the scheme
against `wav` (`0x10471cac`) and `wma` (`0x10471d40`) and calls the decoder
`0x101a73b8`; the surrounding string block is the scheme table
(`x-sonosprog-http:`, `x-sonosapi-hls-static:`, `x-sonos-spotify:`, `file://`,
`sirradio:`, …) with the error string `bad encaps: %s (%s)` at `0x10453a60`.

## Does a :1400 request reach it

Yes, indirectly: the chain is the **stream reader for a playback source**, and
a source is started by `SetAVTransportURI`/`Play` on `:1400`. The caller of the
decoder is `0x100a84f4` inside `0x100a80f4` (a 1280-byte frame that also sets a
10000 ms timeout and calls the stream opener `0x101756f4`); it is a virtual
method (no direct `bl`, no stored pointer), so the exact vtable slot is the
next read. The blob the sink receives is the **stream's own bytes**, not a
request body: a caller controls it by pointing the player at a URI whose bytes
it serves.

## Consequence

Type 77 is the only sink case with a copy from the staging area, and the copy
is a fixed 33 bytes into `obj+5133` — so the sink itself is not the overflow;
the interesting surface is the 256-byte chunk buffer in `0x100b7c14` and the
`0x1019d068` consumer, which is where a chunk-length bug would live.
