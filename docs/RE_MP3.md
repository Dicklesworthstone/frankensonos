# RE_MP3 — mpg123 on 57.23 (CVE-2024-10573, Frankenstein resync)

Lane `frankensonos-w78i`. The rootfs `libmpg123.so.0` of `57.23-74170-1-16`
predates the 1.32.8 fix for the Frankenstein resync buffer overflow; the
library fault reproduces under ASan against a locally built 1.32.7 and is clean
on 1.32.8. On the device the shipped consumer (`anacapad`) does **not** reach
the bug, because it never scans or seeks and appears to disable Frankenstein
streams.

## 1. The shipped library

`captures/firmware/57.23-74170-1-16/rootfs-extracted/lib/libmpg123.so.0`
(md5 `b9e5a337717f1782a4919fd2b9d72844`, 107 660 B, ELF32 MSB PowerPC, stripped,
`.comment` = `GCC: (GNU) 3.4.4`, no build-id). The only consumer in the image is
`opt/bin/anacapad`.

### Version pin: bracketed 1.32.0 – 1.32.7, and the pre-fix code shape is present

- 102 exported symbols; `mpg123_safe_buffer` (0xb100) is exported, and that
  entry point only exists from **1.32.0** on.
- No exported API was added anywhere in 1.32.1 – 1.32.8 (for each step, the set
  of `mpg123_*` identifiers that the .so exports and that the newer tree newly
  introduces is empty), so symbols alone cannot narrow further — and this build has **no**
  `mpg123_version`/`mpg123_distribution`, so there is no self-reported version.
  Release-specific message strings do not exist either (all 27 string literals
  added in 1.32.8 are absent from the .so, but so are the 5 from 1.32.7 and the
  6 from 1.32.3).
- The structural proof is in the parser. In ≤ 1.32.7 `decode_header()` writes
  the parsed header fields **straight into the decoder handle**; 1.32.8 moved
  them into a caller-supplied `struct frame_header` and only copies them over
  (`apply_header()`) once the matching frame body has been read. The shipped
  binary does the former: at `0x38b8`–`0x3980` it computes `4 - layer` and
  stores the whole header block through the frame pointer (`r31`), e.g.

  ```
  38bc  54 80 7f be   rlwinm 0, 4, 15, 30, 31   ; layer field of newhead
  38c0  20 00 00 04   subfic 0, 0, 4            ; 4 - layer
  38c4  90 03 4a e0   stw    0, 19168(3)        ; fr->lay  (r3 = fr)
  ...
  3938-3980           stw    …, 19180/19192/19196/19204/19208/19212(31)
  ```

  (r3 is `fr` on entry and is copied to r31; both bases are the frame pointer.)
  Ten consecutive `int` fields (`lay`, `error_protection`, `bitrate_index`,
  `sampling_frequency`, `padding`, `extension`, `mode`, `mode_ext`, `copyright`,
  `original`, `emphasis`) are written into `fr` while the header is still being
  validated — the premature application the 1.32.8 refactor removed.

So: vulnerable code shape confirmed in the shipped binary; the exact patch level
inside 1.32.0–1.32.7 is not recoverable from the stripped artifact.

## 2. The fix (1.32.8, oss-security 2024-10-30, CVE-2024-10573)

Upstream announcement: *"There is possible buffer overflow (writing of decoded
PCM samples beyond allocated output buffer) for streams that change output
properties together with certain usage of libmpg123. This needed seeking around
in the stream (including scanning it before actual decoding) to trigger."*
Workarounds it names: `MPG123_NO_FRANKENSTEIN`, or a fixed output buffer of
1152 samples × 2 channels × the largest encoding.

`diff -ru mpg123-1.32.7 mpg123-1.32.8 src/libmpg123` is the fix, in three parts:

1. **`frame.h`** — new `struct frame_header` holding `lay/lsf/mpeg25/
   error_protection/bitrate_index/sampling_frequency/padding/extension/mode/
   mode_ext/copyright/original/emphasis/framesize/freeformat/
   freeformat_framesize/ssize`; `mpg123_handle_struct` gains `hdr` and loses the
   loose copies.
2. **`parse.c`** — `decode_header(fr, struct frame_header*, head, …)` and
   `skip_junk(…, struct frame_header*)` / `do_readahead(…, struct frame_header*)`
   operate on a local `struct frame_header nhdr = fr->hdr;`; `apply_header()`
   runs only after `read_frame_body()` succeeded; the error path no longer
   restores `fr->framesize` from `oldsize` (the header is simply not applied).
3. **`frame.c`** — `frame_reset` memsets `fr->hdr`, and every consumer
   (`frame_bitrate`, `frame_freq`, `frame_skip`, halfspeed, layer1/2/3) reads
   `fr->hdr.*`.

`NEWS` 1.32.8: *"Prevent premature application of header info into decoding
structure, at worst having triggered out-of-bounds writes of decoded PCM data
(bug 322, again)."*

## 3. Proof of concept

Raw logs: `captures/research/mpg123/evidence/`. `scripts/re-mp3/poc_frankenstein.py` regenerates the 839-byte file byte-exactly
(hexdump embedded, no binary in git). Structure: one MPEG-1 Layer III frame
(`ff fb 52 c4`) carrying an `Info`/Xing tag that announces **15 frames /
2506 samples**, then filler that keeps the frame-index scanner reading. It was
byte-delta-minimised (`ddmin`) from the upstream bug-322 attachment; a second
pass removed nothing more and no prefix shorter than 839 B still crashes.

Harness: stock mpg123 1.32.7 and 1.32.8, `./configure --disable-shared
--with-cpu=generic CFLAGS="-O1 -g -fsanitize=address"`, Apple clang ASan
(Homebrew clang's ASan hangs before `main` on this host).

```
mpg123 --index --skip 1 -t poc.mp3      # 1.32.7
==20712==ERROR: AddressSanitizer: heap-buffer-overflow
WRITE of size 4608 at 0x61b000000d8f thread T0
    #0 __asan_memset
    #1 decode_the_frame libmpg123.c:938
    #2 mpg123_decode_frame64 libmpg123.c:1067
0x61b000000d8f is located 0 bytes after 1551-byte region
    #1 INT123_frame_outbuffer frame.c:210
    #2 INT123_decode_update libmpg123.c:775
    #3 do_the_seek libmpg123.c:1399
    #4 mpg123_seek64 libmpg123.c:1456
SUMMARY: AddressSanitizer: heap-buffer-overflow libmpg123.c:938
```

The upstream reporter's variant trips the synth instead
(`INT123_synth_1to1_mono synth_mono.h:39` via `INT123_do_layer3 layer3.c:2023`,
783-byte region) — same root cause, reproduced locally on the same input with a
non-instrumented build. 1.32.8 on the same file: exit 0, no report, and it
completes with `This was a Frankenstein track.`

`--index` builds the frame index by scanning (each frame is parsed, no decode);
`--skip 1` then seeks back to frame 1. The header state applied during the scan
and the buffer sized for it are what the seek leaves behind, and the first
decoded frame after the seek needs 4608 bytes of a 1551-byte buffer.

## 4. Device shot — fetched, no crash, no audio

`192.168.4.84` (Play:5, `57.23-74170`, coordinator of the single-member zone
`X`) via `SetAVTransportURI` + `Play` with DIDL metadata, POC served from
`192.168.4.160:8098` (the unit cannot reach the Wi-Fi host `192.168.4.28:8099`).

| input | result |
|---|---|
| 10 s 440 Hz MP3 (control) | `PLAYING`, RelTime advances 1 s → 9 s, ends `STOPPED` |
| POC (839 B) | fetched twice, transport never leaves `STOPPED`, RelTime 0:00:00, daemon alive |
| POC long (Info frame repeated, 2718 B) | same: two GETs, `STOPPED`, daemon alive |
| plain Frankenstein stream (MPEG2.5 L3 8 kHz mono → MPEG1 L3 44.1 kHz stereo) | `PLAYING` to RelTime 0:00:03, then `STOPPED` — exactly at the switch |

### Why the device does not trip it

`anacapad` imports exactly `mpg123_init`, `mpg123_new`, `mpg123_param`,
`mpg123_open_feed`, `mpg123_decode`, `mpg123_getformat`, `mpg123_close`,
`mpg123_delete` — **no** `mpg123_scan`, no `mpg123_seek*`, no
`mpg123_feedseek`. Feed mode alone does not reach the state: a local
`open_feed` + `mpg123_decode` harness (both the dynamic-buffer and the fixed
4608-byte-buffer styles, chunks 1–full) runs the POC and all format-switch
variants without a report on the vulnerable build.

The third row above also shows the device stopping *at* a format change, which
is what `MPG123_NO_FRANKENSTEIN` does (and is the advisory's own workaround).

So: **the library is vulnerable and provably crashes; the shipped consumer is
not reachable with a stream-only PoC.** Anything that scans/seeks — the mpg123
CLI with `--index`/`--skip`, or a future consumer path that seeks — is exposed.
