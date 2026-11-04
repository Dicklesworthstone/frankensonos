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

## Update (same session, later) — E/F/G complete

| PoC | verdict |
|---|---|
| E giant offset | ACCEPT, healthy (no down within 40 s watch) |
| **F TOC self-overlap** | **ACCEPT → daemon down at 20.1 s, watchdog recovery 0.2 s** |
| G aax short activation | ACCEPT, healthy |

So the PoC set now holds **two independent kill classes** with distinct
signatures:

- **C activation-mismatch**: accept → full unit loss (no HTTP *and* no
  ICMP, >2 min, self-recovered ~4 min). Wedge-or-reboot, cause open.
- **F TOC-overlap**: accept → daemon down 20.1 s, fast watchdog recovery —
  the classic anacapad crash-restart shape, pointing at the TOC walk itself
  (offset pointing back into the TOC region being parsed).

`make_pocs.py` + this table satisfy the bead's "crafted .aa PoCs per TOC/
activation class + device verdicts" deliverable; the faulting-instruction
capture (devmode/qemu) remains the open mechanistic step.

## 2026-11-04 (SageElm) — one-field bisection: the kill signature is unclamped overshoot reads

15 variants fired on `.84` (order: F-variants first, then C-variants; all
healthy before each shot; unit healthy at the end):

| variant | change from baseline | verdict | down |
|---|---|---|---|
| F_base | off=1, len=1 | ACCEPT | **18.4 s** |
| f1 | off=0 (reads magic) | ACCEPT | no down |
| f2 | off=5 (reads own TOC entry) | ACCEPT | **29.3 s** |
| f3 | len=4096 | ACCEPT | **36.2 s** |
| f4 | off=100 (small overshoot) | ACCEPT | **13.9 s** |
| C_base | act off=82, len=64, 3 B present | ACCEPT | **17.9 s** |
| v1 | act len exact (3) | ACCEPT | no down |
| v2 | act len 0 | ACCEPT | no down |
| v3 | act off 82→83 | ACCEPT | no down |
| v4 | act off 200 (past EOF) | ACCEPT | no down |
| v5 | act len 0xFFFFFFFF | ACCEPT | no down |
| v6 | full 64 B activation | ACCEPT | no down |
| v7 | no activation entry | ACCEPT | no down |
| v9 | codec = zeros | ACCEPT | no down |
| v10 | codec = `mp3` | ACCEPT | no down |

**The mechanism, bisection-proven:** the kill requires an activation/TOC read
at an offset that is **valid (inside the file)** whose declared length
**overshoots EOF**. Every variant that removes the overshoot (exact length,
zero length, offset past EOF, no activation) parses clean; every variant that
keeps a valid-offset + overshooting-length combination kills (F 13.9–36.2 s,
C 17.9 s). Length magnitude barely matters (f3: 4096 → 36 s); the offset being
in-file is what gates it.

**Oracle finding:** the SOAP `SetAVTransportURI` verdict is binary
(ACCEPT/REJ-1023) — the `ERROR_AUDIBLE_*` codes (25/26/27/29) are **not**
returned synchronously; they surface on the async transport-error path, which
is not observable without GENA event subscription. The device-observable
oracles are: accept/reject + the down/recovery timing signature.

**C-class reproducibility note:** today `C_base` killed at 17.9 s with instant
watchdog recovery — same signature as F, *not* the >2 min unit loss from the
earlier session. The wedge severity is state-dependent (earlier run followed
five prior PoC acceptances). The overshoot-read mechanism stands either way.

**Parse-site status (r2):** the audible chain is
dispatcher `0x10123f84` → handler `0x10118ad8` (sub-object at `this+6340`;
string fields `+17928/+19488/+21048`) → transport open `0x10130c58` →
`0x10130a24` (transport-type switch; kind from vtable slot 24 via
`0x102b263c`). The magic `57 90 75 36` does **not** appear in the binary
(nor in any rootfs .so; 86.10 anacapad has no audible at all), so the header
check is computed byte-wise; no `li 25/26/27` clusters exist (codes are
computed/derived). Faulting-instruction capture therefore needs the devmode
console or qemu instrumentation — static-only anchoring has hit its limit;
the overshoot-read signature above is the search image for that step.

## 2026-11-04 (SageElm) — Unicorn PPC32 harness (`recon/tnzm_audible/ppc_emu.py`)

Base built per the hhid_emu.py template; running state:

- **Works:** PT_LOAD mapping; the 478 DT_JMPREL PLT slots resolved by name
  (ELF32 PPC `r_info` sym is `info>>8` — the first parse used `&0xFFFFFF`
  and produced garbage); `.plt` slots patched with `b` into an emulation
  region at **0x1052d000** (the free gap between PT_LOAD1's page end
  0x1052d000 and PT_LOAD2's page start 0x1053c000 — the first EMU choice
  0x50000000 was outside the PPC ±32 MB branch range and left slots as
  zero words, which sent execution into the weeds at 0x10000544);
  `GetErrorString`-style leaf calls and strlcpy/memcpy/memset/strlen/
  malloc/free/pthread_* hooks; SDA/TOC base regs (r13 = .sdata+0x8000,
  r2 = .got2+0x8000); RET-filled stack so epilogues restore LR cleanly.
- **Verified:** handler `0x10118ad8` prologue + save/restore + first call
  into `0x10371fc0` executes; PLT hooking intercepts
  pthread_mutex/getspecific traffic (20+ calls emulated).
- **Blocked at:** the transport-type virtual call at `0x102b263c` —
  `lwz r9,24(r9)` on the object at `this+1656` needs a *constructed*
  transport object (vtable + state), which the synthetic zeroed `this`
  doesn't provide; emulation dies at pc=0x4 on `bctrl` to a garbage slot.
  The overshoot read sits downstream of that open.

**Next emulator step:** construct the minimal transport/stream object graph
(the +1656 object's vtable comes from the class whose ctor is reachable from
the audible chain), or bypass the open entirely by driving the *stream-data*
consumer functions directly with the PoC mapped at POC (0x55000000). The
destination answer the overshoot read needs (which buffer, which offset) is
one object-construction session away; the harness removes all loader/PLT
friction from that work.
