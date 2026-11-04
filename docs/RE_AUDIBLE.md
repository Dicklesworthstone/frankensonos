# RE_AUDIBLE — bead frankensonos-tnzm (lane Audible .aa/.aax + DRM gates)

Pre-auth reach: `SetAVTransportURI` with `protocolInfo` `http-get:*:audio/audible:*`
(or `x-file-cifs:*:audio/audible:*`) — no auth on :1400. Reference container
format: ffmpeg `libavformat/aadec.c` (magic `57 90 75 36`, TOC = repeated
`{u8 type; u32 offset; u32 length}` terminated by `type=0`; 1 = codec blob,
2 = activation, 3 = metadata).

## Static map (57.23-74170 anacapad, PPC)

- Scheme dispatcher `0x10123f84`: compares the URI scheme against `"audible"`
  (`0x10461c28`) via `0x1009ece8`; match → handler `0x10118ad8`; miss →
  `snprintf` + **error 1023** (`li r3,1023` at `0x10123ffc`).
- Handler `0x10118ad8`: ~105 KB stack frame (`stwux`, `lis r0,-2/ori 0x58c0`),
  audible stream setup on `this+6152`; copies three session string fields
  (`+17928`, `+19488`, `+21048`) through ctor `0x100ac178`.
- Error taxonomy: string table at `0x10543600` (index = code):
  `ERROR_AUDIBLE_BAD_CODEC` = code **25** (`0x10543664`),
  `ERROR_AUDIBLE_DRM` = **26**, `ERROR_AUDIBLE_ZP_MISSING` = **27**,
  `ERROR_WMP_NO_LICENSE` = **29**. `GetErrorString` at `0x10385dc0`.
- Codec/MIME strings `audio/audible` (`0x10471c12`) and
  `application/audible` (`0x10471c28`) in the .rodata string pool; the
  protocolInfo registry strings (`0x10465c64`,
  `audible:*,x-file-cifs:*:audio/audible:*`) confirm the pre-auth lane.
- No raw `57 90 75 36` constant in the binary: the header check is table- or
  byte-wise; exact parse site still open (the handler's call tree is large —
  `0x10111824` turned out to be a logging helper, not the TOC reader).

## PoCs (recon/tnzm_audible/make_pocs.py)

Eight classes built from the aadec.c layout — A TOC-past-EOF, B zero-length
codec, C activation-length mismatch, D truncated magic, E giant offset
(`0xFFFFFFFF`), F TOC self-overlap, G aax-style short activation blob,
H valid minimal control.

## Device verdicts (2026-11-04)

`.202` (Play:1, **stereo-pair slave**) rejects every `SetAVTransportURI` with
`errorCode 1023` for all schemes (mp3 control also 1023) — slaves don't accept
URIs; coordinator required. `.84` ("X", stand-alone coordinator, Play:1) is the
verdict unit.

| PoC | verdict |
|---|---|
| H valid | **ACCEPT** (URI set, `GetMediaInfo` shows it) |
| D trunc magic | ACCEPT, healthy |
| A TOC past EOF | ACCEPT, healthy |
| B zero-len codec | ACCEPT, healthy |
| **C activation mismatch** | **ACCEPT → daemon down; unit stayed off-air >2 min (no :1400, no ping)** |
| E giant offset | (queued; unit down from C) |
| F TOC overlap | (queued) |
| G aax short activation | (queued) |

C is the strongest signal of the session: the unit accepted the URI and then
**stopped answering entirely** (no HTTP, no ICMP) — a wedge/reboot, not a clean
parse-error, and far beyond the fbkv watchdog's 2–25 s window. Everything after
C is queued behind recovery. `.81`/`.86` are stereo-pair members (coordinator
`RINCON_5CAAFD21D47001400` not answering on :1400 from the workstation).

Next: (1) finish E/F/G on `.84` after recovery; (2) re-fire C alone with a
200 ms oracle to time the down window and distinguish wedge vs reboot;
(3) locate the exact activation-parse site (branch oracle = 1023 vs
ERROR_AUDIBLE_* codes) — candidates are the three session string fields the
handler copies (`+17928/+19488/+21048`); (4) if C reproduces on `.86`'s
coordinator, it's a second unit confirmation.
