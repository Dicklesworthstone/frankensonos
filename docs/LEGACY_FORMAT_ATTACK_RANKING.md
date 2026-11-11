# Legacy-format attack ranking — media-payload path to root (2026-11-03, CalmHawk)

User directive: modern codecs came back clean because Sonos backports fixes;
the legacy/rarely-used formats that share the same pre-auth
`SetAVTransportURI`+`Play` delivery path are the unexplored surface. Ranked by
(bug density expectation × code age × audit scarcity × delivery certainty).
All parsers run in-process in anacapad (root) on 57.23/86.10.

Binary evidence base: `captures/firmware/57.23-74170-1-16/anacapad`,
`rootfs-extracted/lib/`, MIME registry + parser error strings below.

---

## 1. MP4/M4A + ALAC (hand-rolled Sonos demuxer) — TOP TARGET

**Evidence:** Sonos wrote their OWN ISO-BMFF demuxer, not ffmpeg's:
`(%d) corrupt file: no moov (t:%ld)`, `bad moof`, `no dat`, `required size:%zu
available:%zu`, `size:%zu pos:%zu`, `read:%zu/%zu pos:%zu`, `resync required` —
eight distinct bounds-check messages = a full hand-rolled box walker with
`ftyp`/`moov`/`mdat`/`moof` handling, `esds` elementary-stream descriptors,
`ilst` metadata, and an **ALAC in MP4** path (`m4a-alac`, `Failed to decode
ALAC frame`, `ALAC` magic). Plus `createTrialAccount`/audible tie-ins.

**Why it ranks first:** hand-rolled ISO-BMFF walkers are the classic overrun
farm — box size fields drive memcpy/seek arithmetic; `esds` alone (tag/length
TLV inside TLV) has fed a decade of CVEs in better-audited code; the error
strings show they parse `moof` (fragmented MP4) too, which doubles the box
surface. Zero public audit. Delivery: `audio/mp4`/`audio/x-m4a` in the
protocolInfo registry, same pre-auth path.

**Reference code to study:** mpi4/ffmpeg `libavformat/mov.c` (the reference
ISO-BMFF demuxer — study `mov_read_default`, `mov_read_esds`,
`mov_read_dref` for the exact bug classes), plus
`github.com/lixingye/MP4Parser`-style minimal walkers. For ALAC: the
`alac` decoder in ffmpeg `libavcodec/alac.c` (magic `ALAC` in `stsd`), and
Apple's own opensource ALAC (macports/distfiles/alac). Compare the corrupt-
file error sites in the binary against mov.c's failure paths — every place
mov.c has a hardening check that the Sonos binary lacks is a candidate.

**First experiment:** craft a minimal `ftyp+moov` with each box size field
lying (size < header, size 0 = to-EOF, size 0xFFFFFFFF), nested `moov` inside
`moov`, `esds` with deep TLV nesting, ALAC `stsd` with magic but truncated
cookie — feed via the proven hls-radio/`SetAVTransportURI` path, watch for the
eight corrupt-file messages as the oracle for which branch you reached.

## 2. WMA/ASF (MMS + HTTP) — the protocol handler nobody touches

**Evidence:** `sonos.com-mms:*:audio/x-ms-wma:*` — a full **MMS protocol
client** in-binary; `SASF` marker; ASF header parse + `WM/WMADRCAverage*`
metadata tags; `.wma`; `wmaradio`. The ASF header is object-TLV based
(guid+size), payloads can be multiple-per-packet, and the WMAv2/V3 codec
paths have historic CVEs.

**Why #2:** ASF is Microsoft's TLV-everywhere format — 32-bit and 64-bit size
fields, object counts, payload count/nibble encoding in the data packet
header. MMS-over-HTTP adds its own header parser. Nobody has audited Sonos's
take, and the `sonos.com-mms` scheme means it's reachable directly in
`SetAVTransportURI`.

**Reference code:** ffmpeg `libavformat/asfdec_f.c` + `asfdec_o.c` (the two
generations of ASF demuxers, both with long CVE lists), `libavformat/mms.c`
(MMS-over-HTTP), and Wine's `dlls/winealsa.drv`/`wma` decoders. Study
`asf_read_header`'s object loop and `asf_read_payload`'s nibble/length
handling against the binary's `SASF`/MMS strings.

**First experiment:** minimal ASF with `HDRL` object declaring N objects but
supplying fewer; `DATA` object with packet-count lying; multiple-payload
packet with count nibble > actual; WMAv2 codec-specific data truncated. MMS
path: `mms://` + `sonos.com-mms:` URIs.

## 3. Audible (.aa/.aax proprietary container + DRM layer)

**Evidence:** `audio/audible`, `application/audible`,
`ERROR_AUDIBLE_BAD_CODEC`, `ERROR_AUDIBLE_DRM`, `ERROR_AUDIBLE_ZP_MISSING`,
`ERROR_WMP_NO_LICENSE`, `License Restriction` — an in-binary Audible
container parser AND DRM enforcement path. The `.aa` format is
length-table-driven with an activation/codec lookup.

**Why #3:** the .aa format is a table-of-offsets container (like CHM) —
offset+length pairs parsed before content; the DRM layer adds a second parse
of activation data. Essentially nobody has fuzzed the Sonos implementation,
and the error taxonomy shows codec selection + license checks run before
decoding — every one a parse surface.

**Reference code:** audible-cli / aax-to-mp3 ecosystem tooling documents the
.aa/.aax container layout (underscore bindings in `audible` python package);
the format spec is reverse-documented in the `gnuh`/`soundtaxi` era tools.
Study the offset-table parse and the codec-activation blob parse.

**First experiment:** minimal .aa with table entries pointing past EOF,
zero-length codec blob, activation data with mismatched lengths, each DRM
error string as the branch oracle.

## 4. MP3 via mpg123 (shared lib, version-pinnable) — the quiet workhorse

**Evidence:** `libmpg123.so.0` ships in the rootfs (dynamic, not static —
**version-pinnable from the .so**), `mpg123_open_feed` (feed mode = in-process
demux), plus "Big change (MPEG version, layer, rate). Frankenstein stream?"
warnings. mpg123 1.x has a long OSS-Fuzz history; older releases had
`frame` handling overflows.

**Why #4 (not higher):** mpg123 is well-fuzzed upstream, but **the version in
the rootfs is likely years old** — pin it (`strings libmpg123.so.0 | grep
'^1\.'`) and diff against the mpg123 changelog for every fix after. The
feed-mode API (open_feed/decode loop) is exactly the fuzzable surface.

**Reference code:** mpg123's own `src/libmpg123/` (parse.c, layer3.c) — study
every `check` in the version in the rootfs vs the current release; the
"Frankenstein stream" resync path is where the interesting state machinery
lives.

**First experiment:** pin the version; replay every mpg123 CVE/fuzz fix after
it as a crafted frame; Frankenstein streams (layer switches mid-stream).

## 5. AIFF/AIFC (ancient chunked format)

**Evidence:** `sonos.com-http:*:audio/aiff:*`, `.aiff`, `audio/x-aiff`,
`audio/aiff` in the registry.

**Why #5:** FORM/COMM/SSND chunk walkers are simple but rarely defensive;
AIFF-C (AIFC) adds compression-type dispatch ( sowt, fl32, ALAW...) = more
branches. Low bug density but near-zero audit.

**Reference code:** ffmpeg `libavformat/aiffdec.c`, SoX `aiff.c`. Study
COMM-chunk size handling and SSND offset/blockSize fields.

**First experiment:** COMM size lying, SSND offset past EOF, AIFC with exotic
compression types, chunk that claims to be the last one twice.

## 6. WAV/RIFF (already showed transport-level failures)

**Evidence:** `audio/wav`/`audio/x-wav` registry entries; fuzz round 1's
huge-RIFF killed the transport at 12s (error path, not daemon crash).

**Why #6:** same chunk-walker class as AIFF but already partially probed;
bump it only if the RIFF walker turns out to be shared with the AIFF one.

**Reference code:** ffmpeg `libavformat/wavdec.c` (incl. the `w64` variant),
SoX `wav.c`.

## 7. SBC (Bluetooth codec, libsbc.so.1 in rootfs)

**Evidence:** `libsbc.so.1` dynamic lib; `/proc/driver/audio/dsp`.

**Why #7 (only via AirPlay/Bluetooth transport):** SBC frame decode is
math-heavy with bit-slicing; the library is tiny and old. Delivery is via
BT/AirPlay, not the network URI path, so it ranks low for network-only reach.

**Reference code:** ffmpeg `libavcodec/sbcdec.c`, the libsbc reference.

## 8. playlists: M3U/PLS/ASX (audio/x-scpls, mpegurl)

**Evidence:** `audio/x-scpls`, `audio/x-mpegurl`, `Redirect audio/x-mpegurl
to %s`. The ASX/SCPLS parsers are additional text-format surfaces (the ASX
one is XML — expat-adjacent).

**Why #8:** text parsers, low memory-safety density, but ASX+SCPLS are
distinct parsers from the M3U one already exercised.

---

## Delivery vehicle facts (all proven)

- `SetAVTransportURI` + `Play` pre-auth on coordinators; the device fetches
  and decodes in-process in root's anacapad.
- The `hls-radio://` scheme is one of several (`hls-aac://`, `hls-static://`
  behave identically — scheme picks a service label).
- `sonos.com-http:`, `sonos.com-mms:`, `x-file-cifs:`, `http-get:` protocol
  prefixes select internal protocol handlers — `sonos.com-mms` is its own
  MMS client path (unexplored).
- **The transport-record deferred kill** (metadata + unfetchable URI, 16-195s,
  REPRODUCED) is the parallel carrier lane — the record blob is fully
  attacker-controlled and the serializer copy has no pre-check.
- MP3/wav/flac/ogg/aiff/wma/audible/mp4/m4a/aac are ALL in the
  protocolInfo registry, all reachable by the same pre-auth SOAP action.

## Execution plan (per lane: study → craft → fire)

For each format: (a) pin the reference decoder version that matches the
binary's behavior; (b) diff the binary's error strings against the
reference's failure paths to locate the parser; (c) craft minimal-bug PoCs
from the reference's own fuzz-history; (d) fire via hls_det /
SetAVTransportURI with the new-poller doctrine; (e) record device verdicts in
this doc.

---

## Reference-decoder mapping + known bug classes (researched 2026-11-03)

### #1 MP4/ALAC (Sonos hand-rolled demuxer)
**Reference:** ffmpeg `libavformat/mov.c` (the reference ISO-BMFF demuxer).
Compare mov.c's hardened failure paths against the binary's 8 corrupt-file
sites. **Known bug classes to replicate:** box-size lies (size < 8, size 0
to-EOF, size 0xFFFFFFFF), nested `moov` recursion, `esds` TLV-in-TLV depth,
`stsd` ALAC cookie truncation, `stsz`/`stsc` sample-table arithmetic.
**Live analogues from the last 6 months (all GPAC/MP4Box, the same bug
family):** CVE-2025-55652 (VP config), CVE-2025-55648 (Opus header from
corrupted `stsz`), CVE-2025-55664 (m2ts demux), CVE-2025-55640 (`stbl_AddSize`
on corrupted sample metadata) — every one is "crafted size/count field vs
allocation". **ALAC decode reference:** ffmpeg `libavcodec/alac.c` — its CVE
history (2013-0855 samples-per-frame integer overflow, 2013-0856
lpc_prediction, 2015-6823 allocate_buffers) maps to the three checks to test:
`S1 system only supports 16bit ALAC format` in the binary suggests a
bit-depth dispatch worth poking.

### #2 WMA/ASF + MMS
**Reference:** ffmpeg `libavformat/asfdec_f.c`/`asfdec_o.c` (two demuxer
generations, both with CVE histories incl. CVE-2018-1999011 heap overflow in
asf_o) + `libavformat/mms.c`. **Live analogue:** CVE-2026-19389 (GStreamer
asfdemux, Aug 2026): "multiple integer overflow and underflow when parsing
header objects from crafted ASF/WMV/WMA" — the exact class. The ASF header is
a guid+size object list; the data packets carry a multiple-payload count
nibble; the MMS client adds its own header translate layer. Sonos's MMS
client (`sonos.com-mms:` scheme) is in-binary and unexplored.

### #3 Audible .aa/.aax
**Reference:** ffmpeg `libavformat/aadec.c` (header parsing borrowed from
jteeuwen/audible) — the .aa format is a TOC of (offset, length) pairs +
activation/codec blobs. Error taxonomy in the binary
(BAD_CODEC/DRM/ZP_MISSING/WMP_NO_LICENSE) shows the parse order: container →
codec selection → DRM/license → decode. Each gate is a parser.

### #4 MP3 via mpg123 — VERSION BRACKETED
The rootfs `libmpg123.so.0` exports `mpg123_safe_buffer` (added 1.32.0) and
102 exported symbols → **1.32.0–1.32.7**. The shipped "Frankenstein" resync
strings match the **1.32.8 fix window** (oss-security 2024-10-30: buffer
overflow in versions BEFORE 1.32.8 in the Frankenstein resync path — exactly
the "Big change (MPEG version, layer, rate). Frankenstein stream?" machinery
in the shipped lib). **If the lib predates 1.32.8 this is a LIVE, KNOWN,
PATCHED-UPSTREAM bug in the device.** Reference code: mpg123
`src/libmpg123/parse.c` (the resync loop) + `layer3.c`. Older classics:
CVE-2017-12797 (INT123_parse_new_id3 integer overflow — ID3v2 is parsed from
the same feed), GLSA-200501-14 (MP2/MP3 crafted-file overflow).
**First experiment: determine the exact minor version (compare more symbols),
then craft the Frankenstein resync PoC from the 1.32.8 fix commit.**

### #5-8
AIFF: ffmpeg `aiffdec.c` (COMM size, SSND offset past EOF, AIFC compression
dispatch). WAV/RIFF: ffmpeg `wavdec.c` (huge-RIFF already showed the
transport error path). SBC: `libsbc.so.1` (BT-transport only, low priority).
Playlists: ASX (XML → expat-adjacent) + SCPLS + M3U.

## Delivery vehicles per format (all proven pre-auth)

| Format | URI scheme / protocolInfo | Vehicle |
|---|---|---|
| MP4/M4A/ALAC | `http-get:*:audio/mp4:*`, `audio/x-m4a` | SetAVTransportURI + Play |
| WMA/ASF | `sonos.com-mms:*:audio/x-ms-wma:*`, `audio/wma` | SetAVTransportURI (MMS client in-binary) |
| Audible | `audio/audible`, `application/audible` | SetAVTransportURI |
| MP3 | `audio/mp3`/`audio/mpeg` | SetAVTransportURI + hls-radio |
| AIFF | `sonos.com-http:*:audio/aiff:*` | SetAVTransportURI |
| WAV | `audio/wav`/`audio/x-wav` | SetAVTransportURI (+ hls-radio TS wrap proven) |

## Playability gate on `.137` (2026-11-11) — what actually decodes

Owner's gate: a format only counts if a **real sample plays and is audible**.

| # | format | real sample | verdict |
|---|---|---|---|
| 1 | MP4/ALAC | ffmpeg-made m4a | **does not play**; hand-built box-lie files also 0:00:00, so this needs a real seed before it can be judged |
| 3 | Audible `.aa` | genuine `samples.ffmpeg.org/audible/…acelp85_maihde.aa` (5.7 MB, 89 min, ffmpeg parses it) | **does not play** — 2004-vintage `.aa` is DRM'd; `SetAVTransportURI` answers 500 plain / 200 via the radio shim, `RelTime` stays 0 |
| 5 | AIFF | ffmpeg `pcm_s16be` | **PLAYS** — duration 0:00:20, `RelTime` advancing, tone audible |
| 6 | WAV | ffmpeg | plays (`RelTime` advances; duration field unreliable) |
| 4 | MP3 | ffmpeg | plays (owner-confirmed) |
| 2 | WMA | genuine Windows 7 sample | **does not play**, three transports |
| 7 | SBC | — | not reachable by `SetAVTransportURI` at all: SBC is the Bluetooth transport codec, so it needs a bonded BT source, not a URI |

So of the ranked list, the only **newly viable** parser target is **AIFF**: it is
entry #5, a FORM/COMM/SSND chunk walker (same under-defended class as WAV), it
decodes on hardware, and the oracle is unambiguous.

`.aa` is not dead, just blocked on content: a **DRM-free** `.aa` would settle it,
since the codec handler clearly exists (`audio/audible`, `.aa` strings) and the
S2 daemon's startup `abort()` sits in the very codec-id switch that would
dispatch it.

MP4 remains the ranking's #1 on bug-density grounds, but our hand-built boxes
never reached the decoder, so the immediate task there is a valid minimal
baseline (demuxer-only would do), not more mutating.

### `.aa` delivery grammar, recovered from the app (2026-11-11)

Playability gate said `.aa` doesn't play; the reason is now known and it is a
**service** problem, not a codec one.

The unit advertises `audio/audible` in `ConnectionManager.GetProtocolInfo`
(alongside `audio/mp4`, `audio/x-ms-wma`, `audio/aiff`, `audio/flac`), so the
format is supported. Plain `http://…/x.aa` is fetched and then dropped
(broken pipe, `Play` → UPnP **701**), with or without DIDL, correct
`Content-Type`, Range support, magic `0x57955A16`, or `acelp85`→`acelp16`.

The app (`com.sonos.acr2`, jadx) gives the real grammar —
`TrackSonosItem.getEnqueuedUri()`:

```java
new EnqueuedUri("x-sonos-http:".concat(itemId.addQueryParams(toFlags).build()))
```

and `SonosItemId.build()` produces

```
<legacyFlagsHex><urlencoded objectId>?sid=<serviceId>&flags=<flags>&sn=<accountId>
```

so a full URI is `x-sonos-http:<flagsHex><objectId>?sid=N&flags=N&sn=N` (the
legacy flag hex is *prepended*, the id URL-encoded, `sn` is the account id).

The daemon agrees: `x-sonos-http:` and `x-sonos-mms:` prefixes,
`x-sonosapi-radio:ST:%s?sid=%d&flags=%x`, and the errors
**`No descriptor for sid=%u`** / **`No account for sid=%u`**. So the sid must
resolve to a *registered service descriptor*, which only exists if an Audible
account is linked to the household.

Tested shapes: `x-sonos-http://…` (accepted, never fetched), `x-sonos-http:<enc
url>?sid=N&flags=8224&sn=1` (rejected 500 for sids 1/2/27). Remaining gate: a
real sid for Audible in this household — obtainable from the app's service
list / the household's service registry, not guessable.

Everything downstream of that is already built: the `.aa` fire rig exists, the
container parser is mapped (`aadec`-equivalent: size, magic, TOC, dictionary,
`codec`/`HeaderSeed`/`HeaderKey`, TEA-derived file key), and the S2 daemon's
startup `abort()` sits in the codec-id switch that dispatches it.
