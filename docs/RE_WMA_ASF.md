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

## Round 2 (2026-11-04, DarkForest, omp_4)

Delivery notes confirmed live: `SetAVTransportURI` must go to the **group coordinator**
(`RINCON_000E5852906E` @ 192.168.5.164) — sending to the stereo-pair member `.202`
yields UPnP 402/1023 regardless of payload; and `mms://` URIs fetch only after an
explicit `Play` (Set alone does not connect).

### Header-extension walker identified

`FUN_101fd2e0` runs on the **header-extension object** (`5FBF03B5…`, memcmp
`DAT_1047b200` in `FUN_102008d0`): body = reserved u16 + datasize u32 + sub-objects,
each sub-object = GUID + u64 size + body; `remaining` (u32) decremented by each
sub-object size — the wrap target.

| PoC | shape | verdict |
|---|---|---|
| poc_I1_he_wrap | header-ext sub-object with u64 size `0xFFFF…FFF0` (wrap) | fetched, STOPPED, up |
| poc_I2_he_over | datasize claims 1 sub-object, 3 present | fetched, STOPPED, up |
| poc_J1/J2/J3 | min-packet-size 0x40 / 0x4000 / 0x800 + scramble u24 `0xFFFFFF`/`0x400000` (the `FUN_101fdbe8` windowed-read layout control) | fetched, STOPPED, up — probe reached, no fault |
| rtsp variants A/B/C | DESCRIBE → SDP with `a=pgmpu` base64 ASF header (orders: control-before/after `b=AS`, `b=AS` first) | DESCRIBE reaches the device (`WMPlayer/11.0.5721.5145` UA), SDP body delivered with exact Content-Length; **no SETUP ever sent, connection held open** — the device blocks after DESCRIBE |

Round-2 denominator: 5 new ASF PoCs + 3 RTSP SDP variants, all STOPPED/up, zero crashes.

### RTSP static findings (for the next attempt)

- `FUN_101d65fc` sends `DESCRIBE %s RTSP/1.0` CSeq 1; `FUN_101d610c` = connect+send+
  `FUN_101d55f0` (read-loop with 10 s timeout), header callbacks table `0x1047688c`
  = {`FUN_101d52b4` (Content-Length → `+0x4c58`), `FUN_101d5350` (`rtspSetupResp`:
  Session/timeout)}.
- `readResponseHeaders` requires the status line to `strncmp` `RTSP/1.0 200` (404 is
  special-cased to `0x80000024`), terminates on the first empty line.
- `processSDPResponse` (`FUN_101d58a8`) consumes exactly **Content-Length** bytes
  (`+0x4c58` quota) and requires all three flags: `a=control:rtsp://` (iVar12),
  the `a=control:` data capture **after a `b=AS:` line** (iVar11, gated on
  `uVar9 < uVar10` where `uVar10` = 0x40 set by `b=AS`), and the `a=pgmpu:…base64,`
  line at exactly char 0x35 followed by streamed base64 (iVar15). Order
  `m=audio 0 RTP/AVP 96` → `b=AS:` → `a=control:rtsp://…` → `a=pgmpu…` is the
  parser-accepted shape; all three orders were tried live.
- The post-DESCRIBE stall (no SETUP, connection held) reproduced across 5 SDP shapes
  and with/without body padding and SHUT_WR. Next lever: the SETUP sender is
  `FUN_101d68a8` (called from `FUN_10201580` at 0x10201ba4/0x10201c7c and from
  `FUN_101bbfd4`) — read what precondition gates it (likely the parse verdict or a
  field the SDP parser must fill: control URL at `+0x811`, `b=AS` bitrate at
  `+0x40`-ish). Device-side logs (`rtspSetupResp`, `processSDPResponse: didn't see
  %d/%d/%d`) are devmode-gated on 57.23 — same oracle wall as the token lane.

### Round 4 (2026-11-11): generator rebuilt, two real constraints found

The round-2 kit had been lost with `/tmp`, so `recon/gen_asf.py` is a fresh
minimal generator (spec-correct header, file properties, stream properties,
one data object, `--wrap` / `--follow` / `--min-packet` / `--payload`). Two of
my own bugs here are worth keeping, because both are parser constraints the
lane notes already documented and both silently mask a test:

1. **The preamble object count is a hard bound** — `FUN_101fc6b0` errors out
   with code 7 once it is exhausted. A file that emits seven objects while the
   preamble says 2 never reaches the objects after the second, so a wrap test
   looks like "no fault" when the walker simply stopped. The count must equal
   the objects emitted.
2. **The two preamble bytes must both be `0x01`** (`FUN_101fc0dc` returns
   `0x80000003` unless byte == 1). Writing `01 02` rejects the header at the
   preamble.

Delivery confirmed on `.84`: `SetAVTransportURI` (`x-rincon-mp3radio://http://…`,
URI `audio/x-ms-wma` DIDL) → 200, and the device **fetches** the file
(repeat GETs in the server log are the fetch oracle). `Play` answers 500 on
this unit as a transport quirk but the ASF client runs anyway.

Verdicts so far this round: well-formed baseline, wrap-with-follows (count
fixed), wrap-with-follows (preamble fixed) — all fetched, all parsed, device
up, **no fault**. Consistent with the lane's 16/16 clean history: the wrap keeps
the loop alive but the bytes consumed after the wrap are not yet landing
somewhere fatal.

### Round 4 sweep results (`.84`, 57.23) — no fault, but the MMS path is the live one

Generator now takes `--scramble` (three attacker bytes in the stream-properties
time offset, the `iVar9` source for `FUN_101fdbe8`) and
`--follow-kind streamprop` (followers shaped like stream-properties objects so
the wrapped walker drives the real dispatcher `FUN_101fcc64` instead of benign
file-properties bodies).

| PoC | shape | over http | over mms |
|---|---|---|---|
| j1 | min-packet `0x40`, scramble `0xFFFFFF` | fetched, alive | fetched, alive |
| j2 | min-packet `0x4000`, scramble `0xFFFFFF` | fetched, alive | fetched, alive |
| j3 | min-packet `0x800`, scramble `0x400000` | fetched, alive | fetched, alive |
| e1s | wrap + 3 stream-properties followers, min-packet `0x40`, scramble `0xFFFFFF` | fetched, alive | fetched, alive |

**Transport difference worth keeping**: over `http://` the `Play` action answers
500; over `mms://` it answers **200** and the device behaves normally. `mms://`
is therefore the transport to use for anything that must reach the header
walker — the plain-http path reaches the framer but not obviously the
`FUN_101fc4a8` MMS preamble, which is where the count/`0x01`/`0x01`
requirements actually live. Every file was fetched (>=3 GETs each), so the
client is genuinely receiving and parsing them.

Still zero faults: `iVar9` extremes with a correct preamble and count do not
push the `FUN_101fdbe8` read out of the mapped buffer, and the wrapped walk
consumes the stream-properties followers without tripping anything.
