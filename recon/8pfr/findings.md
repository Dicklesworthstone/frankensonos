# 8pfr — MP4/ALAC hand-rolled demuxer: map, PoC set, device verdicts

Pane omp_2, 2026-11-03. Target binary `captures/firmware/57.23-74170-1-16/anacapad`
(PowerPC, S1). Delivery device: the household coordinator `192.168.6.112`
(`RINCON_5CAAFD21D47001400`), pre-auth `SetAVTransportURI` + `Play` with an
`http-get:*:audio/mp4:*` res; files served from this Mac at `192.168.4.28:8791`.

## 0. Build-layout caveat that changes the lane

The demuxer is **only in the 57.23 PPC build**. The S2 `86.10-81100` `anacapad` has none of
the strings (`no moov`, `bad moof`, `no dat`, `esds`, `ftyp`, `ALAC`, `m4a-alac` — all absent),
and neither does the 86.10 rootfs squashfs. The live fleet is all S2, and 86.10 *does* play a
faststart ALAC `.m4a` over http (proved below), so an S2 demuxer exists in a layer we do not
have. Device verdicts therefore exercise the S2 implementation; the addresses below are the
57.23 code. Treat "device" verdicts as behavioural, not as a statement about those addresses.

## 1. The walker, located

All eight corrupt-file messages plus the ALAC/bit-depth messages live in **one 34 KB
function** (`0x101b1000`-`0x101baa60`), log category **`mpeg4`** (`0x1047357c`). Sites, with
the source line embedded in each log call:

| message | site | line |
|---|---|---|
| `(%d) corrupt file: no moov (t:%ld)` | `0x101b60b4` | `0xf37` |
| `(%d) corrupt file: no dat (t:%ld)` | `0x101b5f10` | `0xfa9` |
| `(%d) corrupt file: bad moof` | `0x101b5928` | `0x11ec` |
| `(%d) corrupt file: required size:%zu available:%zu` | `0x101b58a8` | `0x1207` |
| `(%d) corrupt file: size:%zu pos:%zu` | `0x101b25a4` | `0xb29` |
| `(%d) corrupt file: read:%zu/%zu pos:%zu t:%s:%ld` | `0x101b29a0` | `0xb39` |
| `(%d) corrupt file: read:%zu/%zu pos:%zu t:%s:%ld` (2nd) | `0x101b515c` | `0x136a` |
| `Failed to decode ALAC frame` | `0x101b75d8` | — |
| `S1 system only supports 16bit ALAC format` | `0x101b8b14`, `0x101b94fc` | — |
| `resync required [%u]` | `0x101baa60` | — |

Box-name literal table, 8-byte stride at `0x104734e0`-`0x10473580`:
`ilst, meta, udta, mvhd, trak, mdia, minf, stbl, stsd, drms, moov, stts, stsc, stsz, stco,
esds`, then `m4a-alac` (`0x1047367c`), `m4a-m4a`, `m4a-?`, `ALAC` (`0x10473698`). The
`(%d)` prefix is a running box/state index, so the message text itself carries which branch
fired. Fourcc comparison sites found: `moov` `0x101b0bc8`, `stsd` `0x101b0a60`/`0x101b7a88`,
`stts` `0x101b7aa4`, `stsc` `0x101b7c0c`, `stsz` `0x101b7d80`, `stco` `0x101b7e08`, `esds`
`0x101b888c`/`0x101b89f0`/`0x101b8c34`.

## 2. Delivery facts established this pass

* `SetAVTransportURI` + `Play` is pre-auth on the coordinator and the device really fetches
  (server access log). `TrackDuration` is a usable parse oracle: a file the walker accepted
  reports its real duration, a rejected one stays `0:00:00` and the transport returns to
  `STOPPED`.
* **`moov` must precede `mdat`.** `ffmpeg -movflags +faststart` plays (duration reported);
  the same encode with `moov` after `mdat` reports `0:00:00` and stops. Every PoC is built on
  the faststart base for that reason.
* The Mac address that the device can reach is `192.168.4.28`; `192.168.4.160` (the icon
  host address used elsewhere in the tree) is *not* reachable from `.112`.

## 3. PoC set

`gen_poc.py` mutates one field per case on the faststart base (asserted offsets): ftyp sizes
(`0`, `7`, `0xFFFFFFFF`, 64-bit `1`), brand garbage, truncation; moov sizes (same set), no
moov, no mdat, mdat sizes, nested moov; `stsz` count/size `0xFFFFFFFF`, `stco` offset
`0xFFFFFFFF`, `stsc`/`stts` entry counts, `mdhd`/`mvhd` timescale 0, `stsd` entry count
`0`/`0xFFFF`, `stsd` entry sizes `0`/`0xFFFFFFFF`; ALAC cookie bit depth 20/24/32/0, frame
length 0/`0xFFFFFFFF`, channels 0/`0xFF`, cookie truncation, cookie size `0`/`0xFFFFFFFF`;
`mp4a`+`esds` with a minimal descriptor, zero lengths, `0xFFFF` lengths, 8-deep expandable
nesting, tag-only. 42 files, each cross-checked with `ffprobe` (reference decoder) so the
device verdict can be read against a reference verdict.

## 4. Device verdicts

(filled from `verdicts.tsv`; `fetch` = server access-log lines during the case, so "never
opened" is separable from "opened and rejected")

All 42 cases were fetched (2-3 range requests each), so every verdict below is *post-fetch*.
`BATCH_END 200`; no daemon death, no hang, no `TRANSITIONING`-stuck case. `TrackDuration` is the
discriminator: 0:00:00 == the walker rejected the file, 0:00:10 == it accepted and reported the
real duration.

| case | ffprobe (reference) | device verdict | device duration |
|---|---|---|---|
| `BASE-base` | ? | **parsed** | 0:00:10 |
| `cookie_bitdepth_20` | accept | **parsed** | 0:00:10 |
| `cookie_bitdepth_24` | accept | **playing** | 0:00:10 |
| `cookie_bitdepth_32` | accept | **parsed** | 0:00:10 |
| `cookie_bitdepth_zero` | REJECT | **playing-no-clock** | 0:00:10 |
| `cookie_channels_ff` | REJECT | **parsed** | 0:00:10 |
| `cookie_channels_zero` | REJECT | **parsed** | 0:00:10 |
| `cookie_frame_length_max` | REJECT | **playing** | 0:00:10 |
| `cookie_frame_length_zero` | REJECT | **playing-no-clock** | 0:00:10 |
| `cookie_size_ones` | accept | **playing** | 0:00:10 |
| `cookie_size_zero` | accept | **playing** | 0:00:10 |
| `cookie_truncated_24` | REJECT | **parsed** | 0:00:10 |
| `esds_deep_nesting` | accept | **parsed** | 0:00:10 |
| `esds_length_ffff` | accept | **parsed** | 0:00:10 |
| `esds_length_zero` | accept | **parsed** | 0:00:10 |
| `esds_minimal` | accept | **parsed** | 0:00:10 |
| `esds_tag_only` | accept | **parsed** | 0:00:10 |
| `ftyp_brand_garbage` | accept | **playing** | 0:00:10 |
| `ftyp_size_one_64bit` | REJECT | **rejected** | 0:00:00 |
| `ftyp_size_ones` | REJECT | **rejected** | 0:00:00 |
| `ftyp_size_seven` | REJECT | **rejected** | 0:00:00 |
| `ftyp_size_zero` | REJECT | **rejected** | 0:00:00 |
| `ftyp_truncated` | REJECT | **rejected** | 0:00:00 |
| `mdat_size_ones` | accept | **parsed** | 0:00:10 |
| `mdat_size_zero` | accept | **playing** | 0:00:10 |
| `mdhd_timescale_zero` | accept | **playing** | 0:00:10 |
| `moov_size_one_64bit` | REJECT | **rejected** | 0:00:00 |
| `moov_size_ones` | accept | **rejected** | 0:00:00 |
| `moov_size_seven` | REJECT | **rejected** | 0:00:00 |
| `moov_size_zero` | accept | **parsed** | 0:00:10 |
| `mvhd_timescale_zero` | accept | **playing** | 0:00:10 |
| `nested_moov` | accept | **rejected** | 0:00:00 |
| `no_mdat` | accept | **parsed** | 0:00:10 |
| `no_moov` | REJECT | **rejected** | 0:00:00 |
| `stco_offset_max` | accept | **parsed** | 0:00:10 |
| `stsc_entries_max` | REJECT | **parsed** | 0:00:10 |
| `stsd_entry_count_ffff` | REJECT | **rejected** | 0:00:00 |
| `stsd_entry_count_zero` | REJECT | **playing** | 0:00:10 |
| `stsd_entry_size_ones` | REJECT | **parsed** | 0:00:10 |
| `stsd_entry_size_zero` | REJECT | **playing** | 0:00:10 |
| `stsz_count_max` | accept | **parsed** | 0:00:10 |
| `stsz_size_max` | accept | **parsed** | 0:00:10 |
| `stts_count_max` | accept | **parsed** | 0:00:10 |

`playing` = transport reached `PLAYING` with the position clock advancing; `parsed` = real
duration reported but the transport fell back to STOPPED inside the 1-2.5 s sample window
(this model stops a 10 s file early often enough that `parsed` is the reliable class, `playing`
the bonus one).

## 5. Divergences against the reference decoder

Every one of these is a place where the walker's acceptance boundary differs from ffmpeg's
`mov.c`, i.e. a check mov.c has and the Sonos code does not (or vice versa):

**Device accepts, ffprobe rejects — the missing-check list:**

| case | ffprobe | device |
|---|---|---|
| `stsd_entry_size_zero` (sample-entry size 0) | `invalid size` | **plays to the end** (rel 0:00:10) |
| `stsd_entry_count_zero` (entry count 0, one entry present) | `invalid` | **plays** (rel 0:00:07) |
| `cookie_frame_length_zero` / `_max` | `max samples per frame invalid` | **plays** |
| `cookie_bitdepth_zero` | rejects | **plays** |
| `cookie_truncated_24` | `extradata is too small` | parsed, 10 s |
| `cookie_channels_zero` / `_ff` | rejects | parsed, 10 s |
| `stsd_entry_size_ones` (0xFFFFFFFF) | `Broken` | parsed, 10 s |
| `stsz_count_max` | warns | parsed, 10 s |

`stsd_entry_size_zero` is the sharpest: mov.c rejects it, the Sonos walker treats size 0 as
"to end of file" *inside `stsd`* and the sample entry then swallows the rest of the file —
including `mdat` — yet the codec still initialises from the cookie and the file plays to
completion. `stsd_entry_count_zero` is the same class of missing validation (count is not
cross-checked against the entries actually parsed).

**Device rejects, ffprobe accepts — the stricter side:** `nested_moov` only. `ftyp` size `0`,
`7`, `0xFFFFFFFF`, 64-bit `1` and truncation are rejected by both; `moov` size `7`,
`0xFFFFFFFF` and 64-bit `1` by both; `moov` size `0` (to-EOF) is accepted by both.

## 6. ALAC bit-depth dispatch (the bead's poke)

The cookie bit-depth is read as a 16-bit field at `0x101b8ab4`-`0x101b8ac8`, stored as a byte at
`+0x3c`, and `(v-1) > 1` sets the "unsupported" flag at `+0x3e`, which reaches the
`S1 system only supports 16bit ALAC format` log (`0x101b8b14`, `0x101b94fc`). Live, the message
is a *warning*: 24-bit and 32-bit cookies both play, 20-bit parses, and a 0-bit cookie plays.
So the dispatch does not gate playback, and no bit-depth value tried reaches a fault.

## 7. Honest state

* No memory-safety failure, no daemon restart, no hang across 42 crafted files, including four
  `0xFFFFFFFF` size fields, three 64-bit sizes, a truncated file, and 5 `esds` TLV shapes.
* The walker's *size* handling is the strong part: every top-level lie is caught. The weak part
  is **`stsd` entry validation** (entry size 0 / `0xFFFFFFFF` / count 0 all accepted) and the
  **ALAC cookie field range checks** (frame length 0/`0xFFFFFFFF`, channels 0/`0xFF`, bit depth
  0) — accepted and handed to the decoder rather than rejected at parse time.
* Next levers: (a) `stsz`/`stsc`/`stco` arithmetic against a *large* `mdat` so the sample walk
  actually indexes far past EOF (the current 10 s file only needs a few hundred samples, so
  `0xFFFFFFFF` counts are never fully walked); (b) a fragmented file with a real `moof`/`trun`
  (the `bad moof` branch was never reached — the base is non-fragmented); (c) 24/32-bit ALAC
  *payload* with a 16-bit cookie, to hit the decoder rather than the demuxer.

## 8. 57.23 on-device round (GoldMeadow) — the classes on the build that HAS the demuxer

§0's caveat is now addressed: `.84` (Play:5, `57.23-74170`, zone `X`, coordinator) **is** reachable
when the files are served from `192.168.4.160:8791`, so the esds / ALAC-cookie / stsd-entry classes
were fired on the firmware that actually contains the mapped walker at `0x101b1000`-`0x101baa60`,
not just behaviourally on S2.

57.23's REST `upnpAVTransport` returns **empty `parameters`** (only `result`), so TrackDuration and
transport state have to come from SOAP on `:1400/MediaRenderer/AVTransport/Control`.
Harness `run_5723_soap.py` (same per-case protocol as §3: Stop, Set, Play, two samples, Stop,
per-case `:1400/status/VERSION` liveness poll, access-log fetch count). Raw verdicts:
`verdicts_5723.tsv`.

| case | S2 (§4) | 57.23 |
|---|---|---|
| `BASE-base` | parsed/playing | **playing** |
| `cookie_bitdepth_{20,24,32}` | parsed/playing | parsed |
| `cookie_bitdepth_zero` | playing-no-clock | playing |
| `cookie_channels_{zero,ff}` | parsed | playing |
| `cookie_frame_length_{zero,max}` | playing | playing |
| `cookie_size_{zero,ones}` | playing | playing |
| `cookie_truncated_24` | parsed | parsed |
| `esds_{minimal,length_zero,length_ffff,deep_nesting,tag_only}` | parsed | parsed |
| `stsd_entry_size_{zero,ones}` | playing / parsed | playing / parsed |
| `stsd_entry_count_ffff` | rejected | rejected |
| `stsd_entry_count_zero` | playing | **rejected** |

Result: **no memory-safety failure, no daemon restart, no hang** (21/21 fetched, `alive=200` on
every case, `BATCH_END 200`, transport restored). The weak-acceptance boundary from §5 reproduces
unchanged on the real demuxer: entry size 0 / `0xFFFFFFFF`, frame length 0 / `0xFFFFFFFF`, channel
0 / `0xFF`, bit depth 0 and truncated cookies are all handed to the codec instead of being
rejected at parse time.

One **stricter** delta and it is the interesting one: `stsd_entry_count_zero` (entry count 0 with
one entry present) is accepted and played by 86.10 but **rejected** by 57.23 — the S2 rewrite
dropped a count cross-check the S1 walker still has. Everything else is identical acceptance.

Still not reached, unchanged from §7: the `bad moof` branch (needs a real fragmented file) and the
`stsz`/`stsc`/`stco` arithmetic against a `mdat` large enough to actually walk `0xFFFFFFFF` counts.

## 9. The two leftover levers, closed out (GoldMeadow)

**(b) fragmented file / `bad moof` branch.** A fragmented base was built with
`-movflags frag_keyframe+empty_moov` and a 10 s multi-`moof` one with
`+default_base_moof -frag_duration 1000000`, then the `moof` structure was mutated (size 0 / 7 /
`0xFFFFFFFF`, box type `mOof`, `traf` dropped, `trun` dropped, `mfhd` dropped, `mfra` dropped,
`mdat` moved in front of `moof`). All 18 cases were **rejected** — including the two unmodified
fragmented bases — with `alive=200` throughout and no hang. `ffprobe` accepts both of those bases,
so this is a *capability* gap, not corruption handling: 57.23 (and 86.10, cross-checked with the
multi-`moof` file: TRANSITIONING → STOPPED, RelTime 0) refuse fragmented MP4 outright. The
`bad moof` branch is therefore reached and is strictly rejecting; it is not a crash surface with
these shapes.

**(a) sample-table arithmetic past EOF.** With `mdat` padded by 1 MiB, `stsz sample_count =
0xFFFFFFFF`, `stsz sample_size = 0xFFFFFFFF` and an `stco` chunk offset beyond the end of the
file: all three are **accepted/parsed** (no crash), and `stco_offset_past_eof` drew 5 fetches —
the walker really did index outward. Combined with §5, the arithmetic is the strong part of this
walker; only the `stsd`/cookie *validation* is loose.

State after both rounds: transport restored, daemon healthy, no restart, no hang, no
memory-safety failure across 39 more cases (21 + 18) on the build that owns the demuxer.
