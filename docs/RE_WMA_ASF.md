# RE_WMA_ASF — live lane log

Lane frankensonos-63c6 (#2 in `LEGACY_FORMAT_ATTACK_RANKING.md`). Binary: 57.23-74170-1-16
`anacapad` (PowerPC, stripped). All addresses are runtime vaddrs; the Ghidra project is
`captures/research/ghidra-ppc/proj` (program `anacapad`). Device verdicts from the S1
household, 2026-11-03.

## The ASF/MMS stack in 57.23 anacapad — full map

Entry chain (callers traced via Ghidra `Callers`):

```
rodata fn-table 0x1047b19c -> FUN_10201e58  (URI demux entry)
  FUN_10201c84: strncasecmp http:// -> FUN_102008d0 (HTTP/MMS-over-HTTP client)
                strncasecmp mms://  -> FUN_102008d0 (same client; "mms://" stripped
                                      by FUN_1035d474 and the request is plain GET
                                      with NSPlayer Pragma headers)
                strncasecmp rtsp:// -> FUN_10201580 (RTSP variant)
```

The HTTP client (`FUN_102008d0`):

1. `FUN_10202f40` resolve+redirect loop (max 5 redirects, 10 s timeout each).
2. `FUN_10203254` builds the MMS-over-HTTP GET:
   `GET %s HTTP/1.1` with `User-Agent: NSPlayer/11.0.5721.5145` (or 7.1.0.1956 for
   non-version11), `Pragma: xPlayStrm=1`, `Pragma: xClientGuid={uuid}`,
   `Pragma: stream-switch-count=%d`, `Pragma: stream-switch-entry=ffff:%u:0` — the
   `ffff` literals at `0x1047b5xx`. Fast-start attempt, then normal-start fallback
   (`Fast start failed: 0x%x: http://%s%s` / `Normal start failed too`).
3. Response parsed by `FUN_10203168`/`FUN_102030c8`, then the ASF walker runs on the body.

ASF reader primitives (all buffer+cursor based, all big-endian, **GUIDs are compared as
raw big-endian bytes** — `FUN_101fbde0` byte-swaps each 16-byte GUID into BE layout):

| fn | reads | bound check |
|---|---|---|
| `FUN_101fbde0` | 16-byte GUID | via vtable read callback |
| `FUN_101fbfdc` | u32 BE | `*pos+4 <= len` inside callback |
| `FUN_101fc064` | u16 BE | idem |
| `FUN_101fbe90` | u64 BE | idem |
| `FUN_101fc138` | skip N | via vtable |
| `FUN_101fda40/70/abc` | u8/u16/u32 from a buffered packet | `pos+N <= pkt_len` |
| `FUN_101fdb10` | variable-length field by size 0..3 | dispatch to the above |

**Header walker** `FUN_101ff46c` (RTSP/stack variant) and the MMS client loop:

- `FUN_101fc4a8` parses the stream preamble: memcmp header GUID `75B22630…`, u64 size,
  u32 object count (`ctx+3`), then TWO bytes that must both equal 1
  (`FUN_101fc0dc` returns `0x80000003` unless byte==1 — this is the MMS header-push
  framing, not plain .wma).
- `FUN_101fc6b0` = next-object: reads GUID + u64 size into `ctx+6/7/8`, increments the
  object counter, and **errors out (code 7) once the count from the preamble is
  exhausted** — the count is a hard bound, but each object's *size* is trusted.
- `FUN_101fd2e0` (payload-variant walker): loops while `remaining >= 0x18`; after each
  object seeks `ctx+0x10` by the object's u64 size and subtracts it from `remaining`.
  A huge size (e.g. `0xFFFF…`) wraps `remaining` (uint) and keeps the loop alive —
  the classic size-trust pattern.
- Object dispatch (GUIDs, BE, from `0x1047b1e0`–`0x1047b290`):
  - `B7DC0791…` stream properties → `FUN_101fcc64` (audio/vs video spread, `0x161`
    WMAv2 branch, format-tag xor 0x1c trick, optional-stream log
    `found optional stream %d` / `optional stream invalid %u`)
  - `8CABDCA1…` file properties → `FUN_101fd0f4` (play/send duration + preroll
    arithmetic, min/max packet size + max bitrate; the min-packet-size drives the
    framer's buffer size)
  - `75B22633…` content description → five u16 string lengths, each read by
    `FUN_101fc17c` (UTF-16 → UTF-8 converter `FUN_10317cf4`, bounded by an end pointer)
  - `D2D0A440…` (names/values loop, `WM/WMADRCAverageReference` special-cased into
    dB math) and `C5F8CBEA…` extended content description
  - `44231C94…` DRM v3 header, `2211B3FB…` (loop that parks at `LAB_101ff550`),
    `5FBF03B5…` header extension (`0x17 < len` branch), `59DACFC0…` audio media,
    `F8699E40…` video media.

**Data-packet parser** `FUN_101fdc28` (called from framer `FUN_10200190` on a fully
buffered packet of `min-packet-size` bytes; short reads are zero-padded to that size):

- error-correction byte: bit7 = present; if present `&0x60` must be 0 and `&0xf==2`,
  then two more EC bytes + one byte.
- property byte `a5`: `>>2&3` = payload-length field size, `>>4&3` = offset/duration
  size, `&3` = media-object-number size, `>>6` = sequence type.
- multiple payloads: a byte `&0x3f` = payload count; each payload is
  stream-id (u8), offset-dur, length, and the payload bytes; every length is checked
  against `pkt_len - cursor` before use (`param_2 - local_a0 < local_74 + local_94`
  style checks) — bounded.
- the one **unguarded** site: the "sequence/duration" decode
  `iVar9 = param_4 + (((u24) << 8) >> (0x1d - param_4 & 0x3f)) + 0xb` followed by
  `FUN_101fdbe8(param_1 + cursor - 4, iVar9, param_5)` — a bitfield probe that reads
  4 bytes at `packet + cursor - 4 + (iVar9 >> 3)`. `iVar9` derives from three
  attacker-controlled u8s; no check ties it to `pkt_len`. A far-offset read past the
  framer buffer is in-range theory; the craft below (class H) did not reach a fault
  on-device yet (verdict STOPPED, see below).
- `FUN_10202ab4` scans payload bytes for the `WM/Picture` marker
  (`DAT_1047b4e0`, 10 bytes), caps the UTF-16 copy at 0x100 ushorts into a 256-ushort
  stack buffer — exact fit, defensive.

## Device verdicts (2026-11-03, S1 household 57.23-74170)

Coordinator: `RINCON_000E5852906E` at `192.168.5.164` (the stereo pair "Jeff's Office"
lead; `SetAVTransportURI` to the pair member `.202` alone answers UPnP 402/1023 —
**URI/MIME 1023s below were a coordinator-targeting mistake, not a policy wall**).
Delivery: `POST /MediaRenderer/AVTransport/Control` (short `/AVTransport/Control`
405s) with DIDL `object.item.audioItem.musicTrack` + `res protocolInfo=
http-get:*:audio/x-ms-wma:*`, plain `http://` URI. `mms://` URIs are **accepted**
by SetAVTransportURI (200) and route into the same ASF client.

Server side: `:8620` (PoC files, `audio/x-ms-wma`), `:8621` (raw response-header
shapes). Kit in `/tmp/wma63c6/` (`gen_asf.py`, `gen2.py`, `srv.py`, `srv2.py`,
`play2.sh`); the fetch log (`serve.log`) is the attacker-side oracle — every device
fetch appears there with byte counts.

| PoC | shape | verdict |
|---|---|---|
| plain | minimal header (2 objects) + data | fetched 1066 B, parsed, STOPPED, device up |
| A_count | header count=5, 3 objects present | fetched, STOPPED, up (walker errors code 7 — count enforced) |
| B_strlen / C_xdesc / E2 / E3 / E3b | content-desc & ext-desc length lies (0xFFFF lens, 0xFFFF entry count, 1 entry) | fetched, STOPPED, up (converter bounds hold) |
| E1_hdrsize | header u64 size = 0xFFFFFFFFFFFFFFFF | fetched, STOPPED, up |
| F_datacount | data object count=100, 1 packet | fetched, STOPPED, up |
| G_multi | multi-payload count nibble 0x3F, 1 payload | fetched, STOPPED, up |
| H_scramble | huge scramble/duration fields (class: unguarded `FUN_101fdbe8` windowed read) | fetched, STOPPED, up — no fault reached yet |
| mms hdr_big_ct / hdr_dup / hdr_neglen / hdr_nolen / hdr_badver / hdr_longname | response-header abuse toward `FUN_102030c8` | all Set 200, device up, STOPPED |

Denominator: 11 device-verdict PoCs this round (4 round-1 + 7 round-2/3), all one-bit
STOPPED verdicts, zero crashes — the parse paths are reached (fetches observed) but the
bounds checks hold on these shapes.

## Next

1. The `FUN_101fdbe8` windowed read needs `iVar9 >> 3` inside the packet to stay
   mapped but land within ±4 KiB of an unmapped guard — craft per-packet values
   (needs the framer's real buffer layout: `min-packet-size` from file props is the
   buffer size; set it huge vs tiny to move the heap layout).
2. The `remaining`-wrap class (E1) needs a follow-up object AFTER the huge-size one so
   the wrapped loop actually walks into attacker bytes; round-2 E1 had none.
3. RTSP variant `FUN_10201580` + SDP path (`processSDPResponse`,
   `a=pgmpu:data:application/vnd.ms.wms-hdr.asfv1;base64,`) — untested surface, the
   base64 ASF blob lands in the same walker.
4. Local-file (`x-file-cifs`) WMA via the music library share — the same parsers, a
   second delivery channel that does not need our HTTP server.
