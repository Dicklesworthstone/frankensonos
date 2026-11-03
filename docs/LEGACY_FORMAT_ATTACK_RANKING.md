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
