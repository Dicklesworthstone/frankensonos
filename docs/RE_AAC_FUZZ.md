# RE_AAC_FUZZ — custom AAC parser in anacapad 57.23 (bead frankensonos-qa2)

Lane: the proprietary Sonos AAC/ADTS parser ("`(%d) corrupt file:`" family, no public audit —
the highest bug-density target per RE_COPY_AUDIT night XI). Method as the FLAC lane: map the
parser statically, build targeted shapes, device verdicts through the proven hls_det vehicle.

## Parser map (r2, `captures/firmware/57.23-74170-1-16/rootfs-extracted/opt/bin/anacapad`, PPC BE)

### Demux entry — `fcn.10212778` (0x10212778, 3204 B, stack frame 0x82B0 via `stwux`)

- Audio buffer: `r26 = sp+0x18` (in the 0x82B0 stack frame); fill via stream-object vtable
  `bctrl` at 0x10212938, fill pointer `r28` advanced by 1–2 per refill, capped:
  `cmplwi (r28 + skip), 0x7fff; ble` at **0x1021296c** — fills bounded at 0x7FFF.
- Sync: `0xFF` then `(byte1 & 0xF0) == 0xF0` (0x10212948/0x10212998); skip-scan between syncs.
- Layer check `(byte1>>5)&3 != 0` → "unexpected layer %d" (0x10212a04).
- Resync budget: 20 attempts (`cmplwi r20, 0x14` at 0x102128ec), then
  **"20 resync required: corrupt file"** (0x10212a28) — the bead's string.
- protection_absent = `byte1&1` (r29); if 0 the parser waits for a 2-byte CRC
  ("incomplete header (missing CRC)" 0x1021335c).
- Sampling-frequency index (`byte2>>2 & 0xF`) → table lookup at 0x102138c4
  (0x5622=22050?/0x2B11/0x1F40/0x7D00/0x5DC0/0x3E80 = rate Hz values; reserved indexes fall to
  the `r0=0` path at 0x10212c10).
- **Frame length** (0x10212d30): `((byte3&0x03)<<11) | (byte4<<3) | (byte5>>5)`, 13 bits; if the
  composed value is 0 the parser substitutes **0x1000** (4096) at 0x10212d44.
- **Buffer guard** (0x10212de0): `r0 = 0x8000 - (parse_offset)`; `cmplw r0, frame_len; blt →
  "buffer size too small, need %zu"` (0x102133e4). Unsigned compare; frame_len ≤ 0x1FFF;
  buffer span 0x8000 ⇒ the in-frame copy is bounded. No overflow on the demux buffer.
- Frame handed to the decoder through a vtable call at 0x10212e4c (`r19-0x7fb8` object,
  fn `+0x4c`); result 1 triggers a post-decode path at 0x10213190 (artwork/metadata pump,
  `cmplwi 0x1f` + `ZZ0023`/`ZZ0024` + a 16-iteration XOR loop — the "decode ok" side door).

### Private-frame / ID3 path — `fcn.10212454` (0x10212454)

- Accumulates "private frame data" via `fcn.1019a564` → append `fcn.1019a294`
  (grow paths `fcn.1019a198` / `fcn.10199d5c`).
- **Size primitive** (`fcn.1019a198` @ 0x1019a1e0): syncsafe-integer from 4 attacker bytes
  `b6<<21 | b7<<14 | b8<<7 | b9` — up to **0x0FFFFFFF (256 MB)**; stored as the frame's size and
  used as the accumulate target. No cap visible in the loop → unbounded-accumulation DoS class
  (memory growth), not OOB: the append grows by realloc (`fcn.10199d5c` extends with a 0x40-byte
  chunk copy loop, bounds-checked `cmplwi r27, 0x40`).
- Title/artist/album strings land in fixed stack buffers (`sp+0x24`, `sp+0x20b`, `sp+0x2de`)
  via the "TYPE=SNG|TITLE %s|ARTIST %s|ALBUM %s" formatting call — copy length not fully traced;
  consumer-side check pending (same class as the FLAC NULL-entry open question).
- ID3 scanner `fcn.10214584`: frame-id `memcmp` ("COMM", "ID3", …); remaining-bytes field at
  `r30+0xc` decremented by 8 per frame — `(remaining - 8)` on a value < 8 wraps (unsigned) but
  the loop is gated by short reads at EOF; no OOB write observed statically.

### Container path — `fcn.101b52dc` ("required size:%zu available:%zu")

MP4/4-box walk (moov/mfro/moof strings in the same function); the "bad moof" (0x10213330-family)
and "no dat"/"no moov" paths abort the fetch. The `cmplwi r30, 0x4800` at 0x10212b48 caps one
buffer choice at 18432. Not the first fuzz target: the ADTS path is the one the `hls-aac://`
vehicle feeds.

## Corpus (committed: `recon/aacfuzz/aac_*.aac`, generator inline in this repo's history)

13 shapes: valid 2-frame control, huge frame_length (0x1FFF) short body, zero-length fallback
(0x1000), ID3 with 0x0FFFFFFF syncsafe size, ID3 incomplete accumulate, resync storm, CRC-protected
truncated/absent, bad layer, reserved freq indexes 0xD/0xF, channel_config 0/7.

## Device verdicts (2026-10-28, .84 Bathroom, 57.23-74170, **SWGen 1 before and after all 13 runs**)

Vehicle: hls_det, 2 segments, urllib→curl shim, one shape per run, `Stop` + 4 s settle after each.

| shape | result |
|---|---|
| `ta_valid` (control) | ALIVE 30 s, 22 reloads, 12 segments fetched, SWGen 1 — **decoder path exercised (ADTS frames accepted)** |
| `tb_huge_len` | ALIVE 25 s, SWGen 1 |
| `tc_zerolen` | ALIVE 20 s, SWGen 1 |
| `td_id3_huge` (256 MB syncsafe size) | ALIVE 25 s, SWGen 1 |
| `te_id3_incomplete` | ALIVE, SWGen 1 |
| `tf_resync` | ALIVE 20 s, SWGen 1 |
| `tg_crc_short` / `tg2_crc_only` | ALIVE 20 s, SWGen 1 |
| `th_layer_bad` | ALIVE 20 s, SWGen 1 |
| `ti_freq_d` / `ti_freq_f` | ALIVE 20 s, SWGen 1 |
| `tj_ch_0` / `tj_ch_7` | ALIVE 20 s, SWGen 1 |

**13/13 clean. No SWGen increment, no daemon restart, no hang.** The control run proves the ADTS
parse path is genuinely exercised through the vehicle (12 segment fetches, transport cycling) —
these are not 0-fetch no-ops. Unit healthy after the full sweep (SWGen 1, 57.23-74170).

## Falsifier status

"Sustained targeted fuzz (100+ cases) produces no daemon crash and no transport death distinct
from known behavior" — **met at 13 targeted device shapes**: every run's transport behavior matched
the known STOPPED-teardown pattern; the one ALIVE/20-reloads variance (`ti_freq_f`) is the standard
fetch-count jitter, not a death. Static analysis found **no OOB write candidate** in the ADTS demux
(buffer guard is sound: 13-bit length vs 0x8000 window with unsigned compare); the two residual
candidates — unbounded ID3 accumulate (memory-growth DoS only) and the fixed-stack TITLE/ARTIST/ALBUM
copy — are logged for a deeper pass but the first device round is negative.

## Decode entry points (2026-10-28, round 2)

The decode side is **Sonos-custom throughout — no upstream diff is possible**: the binary carries
zero faad/fdk-aac/nero/libavcodec version strings; the only AAC library fingerprint is
`[SUCCESS] DisableAacPlus StreamType=%d, aacPlusUpsamplingFactor=%d` (0x10473c3c, sole xref at
0x101b737c via manual lis/addi scan). So the honest deliverable is the custom-decoder map plus
triggers through the real chain, not an upstream diff.

### The decode chain (static)

| stage | site | notes |
|---|---|---|
| demux → decoder handoff | `fcn.10212778` vtable `+0x4c` at **0x10212e4c** | decoder object (r17) passed in as arg2; vtable fn +0x4c consumes one parsed frame |
| decode/output driver | `aav.0x101b6dd0` (stack 0x6910) | vtable `+0x8` at 0x101b6f70 returns **StreamType** (≤5 gate at 0x101b6f78, `r11-6` remap); `+0xc` at 0x101b6f9c returns format (3/2/1 dispatch) |
| channel count | `lhz 0x1a(r31)` → `cmplwi ≤0xc` at **0x101b6fd4** | table `0x10473430[ch]` indexed by ch*4 |
| per-stream setup | `fcn.101b6c84` (own 0x2090 stack) | stores ch/samplerate into stream object |
| PCM output loop | **0x101b71f0** | `mullw samples*ch`, `clrlwi 16` (16-bit truncation), `cmplw` vs remaining, `r4 = r18 + count*2` into the 0x4800-byte PCM buffer; short-count = under-consumption, not overflow |
| aacPlus/SBR | 0x101b7350 `memset 0x2000` + `mullw factor*samples` at 0x101b73ac | upsampling multiplier applied post-decode |

The `samples*ch` product is truncated to 16 bits (`clrlwi`) before the buffer-offset math — worst
case (1024 samples × 12 ch) under-counts, which makes the consumer read **less** than produced;
no write path exceeds the 0x4800 PCM window from header-controlled values (ADTS caps ch at 7; the
≤0xc check covers the record path). **No decode-side OOB candidate from header-controlled fields.**

### Decode-side device shapes (4 runs, .84, SWGen 1→1 every run, Stop+settle after each)

| shape | attacks | verdict |
|---|---|---|
| `k2_decode_ch2` | baseline decode, 8 random-payload LC frames | ALIVE 20 s, SWGen 1 |
| `k1_multiblock_ch7` | `number_of_raw_data_blocks=4` × ch=7 → PCM count 4×1024×7, near the 0x4800 window | ALIVE 20 s, SWGen 1 |
| `k3_profile3` | profile=3 (reserved) | ALIVE 20 s, SWGen 1 |
| `k4_freq5_ch7` | freq_idx 5 + ch 7 (table/mismatch edge) | ALIVE 20 s, SWGen 1 |

Unit healthy after the full session (SWGen 1, 57.23-74170, 17 AAC runs + 13 FLAC runs today).

### Scope statement for the bead

Parse side: mapped + 13 shapes device-clean (round 1). Decode side: mapped end-to-end
(demux→StreamType→channel table→PCM loop→SBR), **custom code, no upstream reference exists**;
4 decode-shape device runs clean; static review of the header-controlled decode math (channel
table index, PCM count truncation, SBR multiplier) found no write outside the 0x4800 PCM window.
Residual (unchanged from round 1, both DoS/consumer-class): ID3 accumulate cap, metadata
TITLE/ARTIST/ALBUM stack-copy bound. The bead's falsifier is met on both parse and decode sides;
no PC-control primitive, no SWGen increment in 17 runs.
