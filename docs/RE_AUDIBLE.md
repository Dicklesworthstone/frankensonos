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

### Emulation session 2 — demuxer chain anchored

Followed the audible module's internal call tree under the harness:

- `0x1011d2e8` (the audible path's main function) calls into the
  **`0x10102xxx`–`0x10106xxx` stream/parse module**:
  `0x10104dcc` (state reset), `0x1010537c`, `0x101068f4`, `0x10106a64`,
  and — on reopen — `0x10104df0`.
- **`0x10104df0(r30=audible-session, r4=stream)`** = open-by-format: it calls
  `0x1030e6e8` (stream-flags check, returns `[r3+356]` when `byte[0]` set)
  and then the **demuxer probe chain `0x10104360`**, which tries eight
  format probes in order: `0x102af488`, `0x10104264`, `0x100a6f34`,
  `0x100a679c`, `0x102c8af4`, `0x102c8b68`, `0x102af4ac`, `0x102c8db4`.
- `0x10104264` is an HTTP-header probe (`strncmp` 9 bytes against
  `"nos-Playback-Id"` at `0x1046c804`) — the demuxer chain includes
  header-sniffing probes, so the `.aa`/`.aax` signature check (magic absent
  from .rodata) is inside one of the remaining probe bodies or computed.
- The stream object passed as `r4` originates in the caller chain
  (`0x10103598` = "start stream" virtual, `r24=r8` — the object is built by
  the handler's caller, not the handler). Emulating past it requires
  fabricating that object's vtable or hooking `0x10104df0` with a fake
  stream whose read/seek serve PoC bytes.

**Concrete next emulator step:** hook `0x10104df0` — replace it with a stub
that returns success, and drive `0x10104360`'s probe chain + the subsequent
TOC/activation parse against a fake stream object whose vtable slots are
Python-served reads from POC (0x55000000). That bypasses the transport
construction entirely and lands the overshoot-read destination address.

### Emulation session 3 — bypass-open run + two load-bearing findings

With `--bypass-open` (transport classifier `0x102b263c` → `li r3,1`, open
`0x10104df0` → `li r3,1`) the handler `0x10118ad8` runs to **clean return:
16,283 PLT calls**. Read-region instrumentation shows zero reads from the
PoC page and zero from heap — the stream-consume phase never runs: it lives
downstream in the session state machine (`0x1011d2e8` family), gated on a
real opened-stream object, not reachable by stubbing the open alone.

**Finding 1 — no magic gate in the Sonos audible path.** `D_trunc_magic.aa`
(3-byte magic) was ACCEPTED on-device, and the demuxer probe chain
(`0x10104360`) contains HTTP-header sniffers (`strncmp
"nos-Playback-Id"`), i.e. the Sonos audible probe does **not** require the
ffmpeg `57 90 75 36` magic. TOC entries are parsed from the stream without a
magic precondition — which is why every TOC-shaped PoC was accepted.

**Finding 2 — overshoot destination (device-logic + emulation-logic).** The
overshoot bytes land in the **demuxer input buffer** allocated by the
accepted-open path (heap), not at a fixed .bss address: the parse reads
TOC-driven ranges from the stream object into that buffer, and the
overshoot read consumes the buffer tail + neighboring heap. The exact
buffer address is per-boot; the *mechanism* (TOC length trusted over actual
stream bytes, valid-offset + overshoot-length read) is the bisection-proven
signature. Byte-exact destination requires the fake-stream harness step:
build a stream object whose vtable read/seek slots serve PoC bytes, then
`0x10104df0`'s real code runs and the demuxer buffer + overshoot
destination become observable in the Unicorn map.

### Emulation session 4 — the parse loop executed; destination captured for the codec class

Driving `0x101152f8` (the container parse, 19856-B frame) directly with fake
IO (`recon/tnzm_audible/parse_drive.py`):

- **Structure decoded:** `0x101152f8` → read-loop `0x101049f4(session,
  &stream, buf, 32, out, &out2, 257, &flag)`:
  - `0x100149a8(stream, buf, …)` reads a **12-byte TOC entry** into the
    caller frame (buf at r1+64);
  - `0x1038237c`/`0x1036e044` extract fields; `0x10341b00` builds a node;
  - **`0x10346d1c` = the entry DATA read** — args observed live:
    `r4=0x602f9788` (**destination = parse-frame buffer at r1-0x6878**),
    `r6=0x80` (128-B chunk cap), `r10=0x101`; return code compared against
    **1045 / 1042** (`xori` at `0x10104c14`/`0x10104c20`) — the audible
    error codes are returned by this reader, confirming the taxonomy;
  - one entry per `0x101049f4` call; `0x101152f8` re-calls it bounded by a
    counter at session+30028 (max 2 observed).
- **Session inline buffers confirmed as activation/content stores:**
  `0x10104e5c` strlcps `+6380 → +16626` (cap 1025) and `+7405 → +17651`
  (cap 4097); the parse consumer is `0x10115d6c` → `0x101152f8(r4=+16626,
  r5=+17651)`. **Overshoot destination for the activation class = the
  `+17651` (4097 B) inline buffer** — a TOC length larger than the blob
  reads past the activation bytes into the adjacent 4097-B content buffer
  (same object, in-bounds of the object, out-of-bounds of the logical blob);
  much larger lengths run past the object into the heap (the C-class wedge).
- **Emulation blocker (honest):** with synthetic session data the recursive
  node parser (`0x10340640`/`0x1041d914` family, 64-bit size arithmetic at
  +496) diverges and exhausts the 4 MB stack at `0x1041d930`; capturing the
  activation-entry overshoot read live needs real session-state seeding
  (a valid first entry so the recursive parse terminates) — the harness,
  hooks and instrumentation for that run are all in place in
  `parse_drive.py`.

## 2026-11-04 (SageElm, mgs7) — session map past +21748; observer cell = the PC-control candidate; reach-PoC falsifies naive far-write

### Field map (audible session, verified via accessor cross-refs)

| offset | content | accessors |
|---|---|---|
| +12 | notifier object ptr — vcall `[ [sess+12]+12 ]` on every state change | 0x101060b8, 0x1011a050 |
| +6380 | URI string (1025 B) → strlcpy'd to +16626 | 0x10104e5c |
| +7405 | source string (4097 B) → strlcpy'd to +17651 | 0x10104e5c |
| +17651 | **content buffer (4097 B) — overflow start**; cap ends +21748 | 0x10104e5c |
| +21748 | state flag (`== 1` checks) | 0x10118b58, 0x1011a000 |
| +21752..25815 | no direct-imm accesses (pointer-reached data) | — |
| +25816..25860 | 10-field block (0x1010dcb4-e30) — sibling object, not audible | — |
| +28012/28020/28024 | strings stored at stream-start, read into URL/log assembly | 0x101037a0/b0/b4 → 0x10103244 |
| +28016 | byte flag | 0x1011a91c |
| +29476/+29584/+29605/+29734 | state fields | 0x10105474/0x10118bc0/0x10105478/0x101037d0 |
| +29992/+29996 | byte flag / `0x7FFFFFFF` limit constant | 0x101037d4/0x101037c8 |
| **+30000** | **observer cell {+0: handler pointer (ctor 0x101e394c), +4: generation, +8: changed bool}** | ctor 0x101e394c; loader **0x101e3a44** `lwz r3,0(r3)`; consumer **0x102bf778** |
| +30012/30013/30014/30020/30024/30028/30032 | state-machine scalars / parse counter | 0x10104ebc ff., 0x1011537c |

### The PC-control chain (mgs7's function pointer)

`[session+30000]` is the **observer/handler pointer**:

```
0x101e3a44:  lwz  r3,0(r3)        ; load observer ptr from the cell
             bl   0x102bf778      ; pass as OBJECT
0x102bf778:  ... r9 = [obj+6100] / derived handlers ...
0x102bf890:  lwz  r9,0(r9)        ; vtable
             lwz  r9,0x20(r9)     ; slot 8
             mtctr ; bctrl        ← PC control, r3 = fake object
0x102bf914:  lwz  r9,0x68(r9)     ; slot 26; bctrl          ← second vcall
```

Overflow reach: from +17651 with TOC-supplied length L (bisection-proven
untrusted), **L ≈ 12349** lands on +30000..+3000B — and the overflow bytes
themselves can be the fake object + fake vtable (attacker-controlled pointer
pointing back into the same overflow = self-contained fake-object setup).

### Reach-PoC result (falsifies the naive far-write)

`M_reach_30000.aa` (activation entry, length 12400, marker pattern with
`0xBAADC0DE` at the computed +30000 landing): **ACCEPT, no down, daemon
healthy**. A large TOC length alone does **not** linearly overwrite the
session — the length-driven reads are chunked (128 B) into the parse-frame
buffer, and the session write is the capped strlcpy. The C/F kill mechanism
is therefore the **short-read/long-expect mismatch** (bisection-proven), not
a far linear overwrite. Reaching +30000 needs an entry type whose handler
writes length-driven into the session (the per-type handlers
`0x10105xxx`/`0x10106xxx` — next mapping step), or a cap-bypass shape.

Device stress: one `SetAVTransportURI` shot; unit healthy after.

### 2026-11-04 (SageElm, mgs7) — RCE shot fired; correction to the reach claim

`R_root_rce.aa` (activation len 12400, fake observer ptr `0xBAADC0DE` +
generation/flag markers at the computed +30000 landing, per the user's
fake-vtable model): **ACCEPT, no down, daemon healthy** — same as
`M_reach_30000.aa`. Two independent reach-PoCs now falsify the far-write
model.

**Correction to my earlier reach claim:** the "L ≈ 12349 lands on +30000"
arithmetic conflated the *read* length (parse-frame, 128-B chunks via
`0x10346d1c`) with the *session write*. The session write is
`strlcpy(+17651, +7405, 4097)` — hard-capped; last written byte = session
**+21747** (the NUL). **The activation overflow reaches no pointer field**:
+21748 (the state flag) itself is one byte past the NUL. mgs7's falsifier
branch fires for the +30000 cell via this path.

What remains true and useful:
- The observer cell +30000 and its two bctrl vcalls (slots 8/26 via
  0x102bf778) are real and remain the in-object PC-control candidate — but
  reaching it needs a different write primitive: an entry type whose handler
  copies TOC-length bytes into session fields (per-type handlers
  `0x10105xxx`/`0x10106xxx`, unmapped), or the +28012/28020/28024 string
  fields if any is length-filled (current evidence: scalar inits).
- The C/F kill mechanism (short-read/long-expect mismatch in the chunked
  reader) stands, bisection-proven.

Device stress this session: 1 shot (`R_root_rce`); unit healthy after.

## 2026-11-04 (SageElm, mgs7) — /notify dispatch emulation: N-independent, shaped by the FIRST request allocation

Emulated the arena-allocation arithmetic (`0x10222778` bump, no re-align;
`0x10222314` aligned) plus the `/notify` dispatch
(`vt = *(singleton); fn = *(vt+0x2c)`) —
`recon/tnzm_audible/arena_dispatch_model.py`.

**Result: the dispatch is N-independent.** The SocketManager arena is
bump-only; the singleton's `vt`/`fn` are read from arena+5369 **forever** —
only the **FIRST post-boot request-path allocation** shapes them:

| scenario | vt | fn | outcome |
|---|---|---|---|
| generic HTTP requests (any N ≥ 1) | `'GET '` = 0x47455420 | wild (read at 0x4745544C) | the device's 14–36 s kill signature — the model reproduces it |
| shaped FIRST request (attacker bytes; self-referential fake vtable inside the same allocation) | fake-vtable ptr | attacker-chosen (model: 0x101e3990 `li r3,1; blr`) | **CONTROL=YES at every N** |

The shaped request needs **one known address**: the arena base
(deterministic per boot — early allocations; recover via info leak, boot
replay in the emulator, or low-bits brute). With it, one shaped socket
request (bytes 0–3 = fake-vtable ptr, bytes 0x20–0x24 of the vtable region =
target fn) followed by `/notify` = controlled PC in the root daemon.

Device confirmation of the *kill-shape* prediction is already on file
(bisection: any /notify kills, N-independent). The control-shot needs the
arena-base leak — queued as the next step.

### 2026-11-04 (SageElm, mgs7) — race attempt + unit-state discovery: the /notify kill is state-gated

Fresh-arena race attempt on `.84` (57.23): shaped-request spam during watchdog
recovery + `/notify` → **405, alive** (also plain curl: 405 + alive, 3×).
Same on `.202`: today the 10/10-kill shape (identical GET) answers **405 +
healthy** — the kill that fired repeatedly on 2026-11-03 does not reproduce.

**The /notify kill is unit-state-gated.** The 405 paths in the handler
(`li r9,0x195` at 0x1028a64c/0x1028a668) fire before the singleton dispatch
when a request-object field ([r31+0] compared against 9) or a param
null-check fails. The kill-day requests were byte-identical — so the state
delta is on the daemon side (notification-subsystem init state after
multiple watchdog cycles, or the singleton cell being NULL on today's boot,
which would skip the vcall and fall to 405).

Consequences for the race:
- The fresh-arena race (C-kill → shaped spam → /notify) cannot proceed until
  the kill reproduces — the dispatch itself is what's state-gated.
- The arena model + shaped-request layout stand (committed,
  `arena_dispatch_model.py`); the missing inputs are now (a) the arena base
  AND (b) the state gate that re-arms the dispatch.
- The state gate is observable via the emulator: run the handler
  0x1028a5b8 head under Unicorn with today's request and check which 405
  branch fires — but it needs the parsed-request object shape, which is
  transport-layer state.

Next session: root-cause the 405 state gate (compare a kill-day /notify vs
today's on the emulator, or re-arm via GENA subscription — subscribe a
control point to the unit's UPnP events, then retry /notify).

### 2026-11-04 (SageElm, mgs7) — THE UNCHECKED COPY FOUND: `strcpy(session+6380, node)` at 0x10115478

Per-type handler mapping (this was the last analytical piece):

- The container parse `0x101152f8` re-calls the read loop `0x101049f4` and
  then branches on the per-entry result (`r28`) and node state (`r22`, `[r26]`)
  to per-type handlers:
  - **`0x10115478`: `strcpy(session+6380, [r1+19776])` — PLAIN strcpy, no
    cap** (PLT `strcpy@plt` at 0x10115480). The source is the parsed entry
    node string — attacker bytes from the stream, length = the node string.
  - The neighboring copies ARE bounded: `strlcpy(session+7405, …, 4097)` at
    0x1011552c, `strlcpy(…, 257)` at 0x10115554, and a clear-path
    strcpy/strlcpy pair at 0x10115504/0x1011552c (constant empty source).
- **Overflow reach from +6380 (unchecked):** +7405 (1,025), +16626 (10,246),
  +17651 (11,297), +21748 (15,368), … **+30000 (23,620)** — the observer cell
  `{handler ptr, generation, changed}` lands at string offset **23,620**.
  A node string ≥ 23,632 bytes overwrites the pointer, the generation, and
  the flag in one shot; the same overflow bytes double as the fake object +
  fake vtable (ptr → +0x20 slot → PC control via 0x102bf890/bctrl).

**Emulation state:** the type-sweep harness (`/tmp/tnzm_typesweep.py`,
session-4 infra) reaches the read loop and serves TOC entries for types
1–8, but the post-loop node consumers (`0x101a8f20`/`0x101a946c`/`0x101a999c`
+ the `0x1041d914` 64-bit size arithmetic at node+496) diverge on synthetic
node state — the register-exact path conditions to reach 0x10115478
(r28 = per-entry code ≠ 0, r22 = 0, [r26] = 0 — derived from the branch
algebra at 0x10115448–74) are documented above; a live capture of the
strcpy needs the node built to a terminating state (real session seeding).

**Root-RCE recipe (now complete analytically):**
1. TOC entry whose parsed value exceeds 23,632 bytes (type per the
   0x10115470 path conditions),
2. value bytes at offsets 23,620–23,631 = fake observer ptr (→ the overflow
   itself), generation, changed-flag,
3. overflow body = fake object (obj+6100 fields as needed by 0x102bf778)
   + fake vtable with slot 8 = payload,
4. the next state-change notify (`0x101e3aa0` ← `0x101e3d60` ← the +12
   notifier at 0x101060b8) loads the poisoned pointer and bctrls through
   the fake vtable → PC control, r3 = fake object, root context.
