# RE_NOTIFY_FUZZ — bead frankensonos-v8z (lane PinkMill / IndigoBeaver pane)

Offline grammar fuzz of the pre-auth anacapad 57.23-74170 handlers `/notify`,
`/setstring`, `/advconfig`, `/support/directsubmit`, `/support/aggregate`.
Device verdicts are NOT in this file yet (coordinator approval required per
pane instructions); everything below is static + simulated.

## What was extracted (instruction-faithful, from the live binary)

Route table: descriptor array at `.data 0x1053d040`, stride 0x1c, `{pad; const
char* route; handler*; pad}`. Handlers of this lane:

| route | handler | body reader | kv caps |
|---|---|---|---|
| /notify | 0x1028a5b8 | none (GET-only shape; POST falls to 0x195) | n/a |
| /setstring | 0x1000b170 | 0x1038a950 dst 0x800 min 0x3e8 | key 0x80 @r1+0xc40, val 0x401 @r1+0xcc0 |
| /advconfig | 0x1028a724 | 0x1038a950 dst 0x1000 min 0x7d0 | key 0x20 @r1+0x1040, val 0x80 @r1+0x1060 |
| /support/directsubmit | 0x1028b41c | 0x1038a950 dst 0x800 min 0x3e8 | key 0x80 @r1+0xd40, val 0x401 @r1+0xdc0 |
| /support/aggregate | 0x1028b680 | (same family) | same family |

Shared parse plumbing (all bound-checked in this build):

- `get_param` 0x1040bfec -> 0x1040aac8: request token table, first match.
- `Content-Length` (0x1043e520, strcasecmp): `strtoul` base 10, **errno
  checked** — overflow (`4294967296`) is rejected, `-1` wraps to 0xFFFFFFFF but
  then fails the `want < min_len` gate, so no giant length survives to the
  loop.
- body reader 0x1038a950/0x1038aa08: recv is clamped by
  `remaining = dstlen - written` (0x1038aa1c) and the loop condition is
  `written < want`; both terminators are in-bounds
  (`dst[written]=0` when `written < dstlen-1` at 0x1038aa4c, else
  `dst[dstlen-1]=0` at 0x1038aa64).
- form tokenizer 0x1038a5d4 -> 0x1038a508, decoder 0x1038a420: `%`+2-hex
  enforced with `isxdigit` (bad `%` fails the pair, no half-decode), `+`->
  space, copy bounded by `cmplwi cap,1; ble` with the terminator written at the
  **un-advanced** cursor on the capacity-fail path (0x1038a4e8), so no
  off-by-one.

Handler-specific facts worth keeping:

- /notify params are lowercase header names `nt` `nts` `sid` `seq`
  (0x1047291c / 0x1048cd14 / 0x10498dfc / 0x10496830); gate is
  `nt == "upnp:event"` and `nts == "upnp:propchange"`; then a virtual call
  `singleton->vt[0x2c](sid, seq, nts, nt)` on `0x10545dc8`. `seq` reaches the
  callback as an opaque string — that call is out of this binary's static
  scope, flagged DEVICE-TEST.
- /setstring POST form keys are literally `key` (0x104ef91c) and `value`
  (0x104fb4a4); unknown `key` values go through property-name validation
  0x1031a650 and store 0x10318eb0 with the same 0x80/0x401 caps. This is the
  daemon-killer route (ROOT_RUNBOOK E): one experiment per unit per minute.
- /advconfig POST writes `FirstZP: [%d]` / `PriorityBridge: [%d]` (integers
  only) into `snprintf("%s/netsettings_mod.txt", base)` at 0x1028a92c — no
  request string is echoed into the file.
- /support/directsubmit requires the form key `csrfToken` (0x1043e8a8) before
  its submission branch; response builders use bounded snprintf with literal
  formats (0x100 / 0x400 caps).

## Harness and result

`recon/notify_fuzz/notify_fuzz.py` — deterministic (sorted case lists, seeded),
no device, no network. Re-implements the parse layer above with a
fault-injecting memory view: any out-of-range read/write, missing terminator,
or non-advancing splitter loop raises with the disassembly site in the
message. Dimensions: param presence/order/duplicates, Content-Length edge
values (0/absent/below-min/over-dstlen/wrap/uint32-overflow/hex-ish/spaces/
junk/empty), form grammar edge cases (`=`, `&&`, dup keys, cap-boundary names
and values, percent-decode edges incl. `%00`, `+`), per-handler shapes.

```
$ python3 recon/notify_fuzz/notify_fuzz.py
=== 177 cases, 0 fault(s) ===
```

177/177 clean. Both candidate bounds suspected during recon (B1
Content-Length-vs-buffer, B2 kv-terminator off-by-one) were **disproven** by
the instruction-faithful simulation: B1 because recv is clamped by remaining,
B2 because the fail path does not advance the cursor before the terminator.

## Verdict for the bead's claim

The claim ("a grammar-aware fuzz finds a reachable memory-safety flaw") is
**not yet satisfied and not yet falsified**: the offline sweep is clean, which
per the bead's falsifier needs the exhaustive device dimension plus "no static
candidate surviving review". Static review status:

- All fixed-buffer copies in these five handlers: bounded (verified above).
- Remaining device-only surface: /notify `seq` -> vtable callback (opaque
  string, out-of-binary), and the actual recv() semantics under short reads.
  Both need a device run to move; queued behind coordinator approval.

## Repro

```
python3 recon/notify_fuzz/notify_fuzz.py            # verdict table
python3 recon/notify_fuzz/notify_fuzz.py -v         # every case
python3 recon/notify_fuzz/notify_fuzz.py --seed 7   # seed is accepted; case order is deterministic
```

Grammar source-of-truth: `recon/notify_fuzz/GRAMMAR.md` (offsets + shape table
+ candidate analysis B1/B2).

## /setstring write path — the stack struct outlives its frame (2026-10-28)

`/setstring` is the proven one-request pre-auth kill (~25 s hang, then recovery, value
persists). The parse layer is clean (177 cases, above), so the cause is downstream, in the
write path. The handler `0x1000b170` builds a property struct on its stack and hands it to the
virtual setter `0x1016b530` (vtable `0x1046ab70` slot 2). The struct does not fit the frame:

| fact | value |
|---|---|
| handler frame (`stwu 1, -5520(1)` at `0x1000b174`) | 0x1590 |
| struct base passed in r4 (`addi 4, 1, 4304` at `0x1000b3c0`) | r1+0x10d0 |
| key copy destination / cap (`0x1000b40c`) | r1+0x10d0 / 0x80 |
| value copy destination / cap (`0x1000b430`) | r1+0x1150 / 0x401 |
| highest struct field the setter touches (`stw 0, 2060(30)` at `0x1016b5b0`, `lwz 31, 2056(30)` at `0x102d4c1c`) | +0x80C / +0x808 |

`0x10d0 + 0x810 = 0x18E0`, i.e. **0x350 bytes past the end of the frame**. The handler fills
only `+0 … +0x481` (key then value); everything from `+0x482` to `+0x80F` is whatever the
caller left there. The setter then *reads before writing*:

```
102d4b40: lwz 0, 0(3)          ; +0 is the first 4 key bytes, not a flag
102d4b44: cmpwi 7, 0, 0
102d4b48: bt 30, ...           ; empty key -> return
102d4b4c: lwz 9, 2056(3)       ; +0x808  <- outside the frame
102d4b58: bf 30, 102d4bb0      ; if non-zero ...
102d4bb0: lwz 9, 0(9)          ; ... dereference it
102d4bb4: lwz 9, 0(9)
102d4bbc: bctrl
```

and `+0x804` is read the same way (`lwz 0, 2052(31)` at `0x102d4b5c`). So the setter
dereferences two words that the handler never initialised, from the caller's frame. A
non-zero garbage word at `+0x808` is a wild call; a plausible-looking one is a hang. The
`stw 0, 2060(30)` at `0x1016b5b0` also writes 8 into the caller's frame, so the overrun is
not read-only.

The struct is a real class with a constructor at `0x102d483c` — it zeroes `+4 … +0x803`
(`memset(r3+4, 0, 2048)` at `0x102d4868`), stores 0 at `+0x804`/`+0x808`, and initialises
`+0x80C`. The handler never calls it; it only strlcpy's the key to `+0` and the value to
`+0x80`. So the setter runs against a struct whose `+0x804`/`+0x808`/`+0x80C` are whatever
the caller's frame holds, and whose `+0` is the key text rather than the constructor's flag.
That is the shape to confirm on the device.

This is a candidate, not yet a proof: the caller's frame contents decide whether the garbage
is benign. Confirming it needs a device-side watchpoint on the caller's frame (or a canary
build), which is the next device run.

## Offline daemon rig (qemu-ppc) — runs, does not dispatch

`anacapad` can be started under `qemu-ppc` with the extracted rootfs and its own config
(`opt/conf/anacapa.conf`, `ServerRoot` rewritten, `PidFile /tmp/…`). Three binary patches are
needed, all in the *copy* under `~/anacapad/rootfs-extracted`:

1. **LED** (`anacapad`): `/dev/chk` is absent and the init loops forever on failure
   (`b 0x100078e4`). Patch `0x774c` → `nop`, `0x7760` → `b`, `0x7788` → `li r30,1`,
   `0x778c` → `nop`.
2. **Flash** (`libflash.so.1`): `/dev/mtd/0` as a regular file fails `MEMGETINFO`
   (`0x40204d01`) and the daemon raises SIGIOT. Patch the `blt` at `0x2cc0` → `nop`.
3. **Threads** (`libc.so.6`): glibc 2.3.5 is LinuxThreads and clones with
   `CLONE_VM|FS|FILES|SIGHAND` (no `CLONE_THREAD`); qemu-user requires the full
   `CLONE_THREAD_FLAGS` set and returns EINVAL. Patch `__clone` at `0xcae88`
   (`mr r3,r5` → `oris r3,r5,5`, i.e. add `CLONE_THREAD|CLONE_SYSVSEM`).

With those, the daemon reaches 22 threads, binds 1400/1410/1443/1843/4444 and advertises, but
the HTTP accept loop never starts: the module init stalls after `sharelist` (needs the
household/settings state that the device gets from flash/cloud). `curl` connects and times
out. So the offline rig is good for startup-path work, not yet for handler calls.

Repro:

```
cd ~/anacapad/rootfs-extracted
setsid timeout 90 qemu-ppc -L . ./anacapad.ledpatch -c /tmp/anacapa-test.conf
```

### Device verdict, 2026-10-28 (`.202` 57.23 and `.86` 86.10)

The POST shape is now pinned: the body must be ≥ 200 bytes (the reader's `cmpwi r3,200`
at `0x1000b2dc` rejects shorter) and the `csrfToken` must be **URL-encoded** — the token
contains `+`/`/`, and a raw `+` decodes to a space, which fails the token compare and
returns 400. With `csrfToken=<urlencoded>&key=…&value=…` and
`Content-Type: application/x-www-form-urlencoded` the handler answers `200` /
`Setting changed` on both units.

| unit | key | value | verdict |
|---|---|---|---|
| .202 | roomName | 1000 B | 200, daemon stayed up 40 s |
| .202 | BootSeq | 1 | 200, stayed up 40 s |
| .202 | ZoneName | BenchTest | 200, stayed up 20 s |
| .86 | BootSeq | 1 | 200, stayed up 24 s |

So the write path completes (200, "Setting changed") but **no hang reproduced** in this
session on either unit with these keys. The ~25 s kill is therefore key- or state-dependent,
not a property of every successful write; the static overrun above is still the best
candidate for the mechanism, but the trigger key is an open question. Next device step: find
the key that reproduces the kill (the earlier note used `BootSeq` after a benign key and got
a restart loop), then watch the caller frame with a canary build.

### Key hunt, 2026-10-28 — all hypotheses negative

`recon/setstring/hunt.py` (one write with a fresh URL-encoded token, then a 2 s liveness
watch) and `recon/setstring/keys.tsv` (one row per write). 30 writes, every one `200` and
persisted (read back with `SystemProperties#GetString`), **no daemon down on either unit**:

| # | hypothesis | shape | verdict |
|---|---|---|---|
| 0 | control: original two-step | `roomName=BenchTest`, 5 s, `BootSeq=1` | 200/200, up 80 s |
| 0b | control with a *new* benign key | `huntb1=v`, 5 s, `BootSeq=1` | 200/200, up 80 s |
| 1 | key count / full re-serialize | 6 new keys in a row, then observe | 200 each, up 80 s |
| 1b | store growth | 11 new keys × 1000 B values | 200 each, up 80 s |
| 2 | XML-special key | `key=a<b`, `key=roomName value=<&"x>` | 200, up 80 s |
| 3 | long key | 900-char key (store cap is 0x80, so it truncates) | 200, up 80 s |
| 4 | unicode key | `key=é日本` | 200, up 80 s |
| 5 | BootSeq value change | `BootSeq=2` | 200, up 60 s |

`SWGen` stayed 1 through all of it (no daemon restart). The store does grow and the values
persist, so the write path is exercised; the ~25 s kill is not a property of the write
itself on this unit in its current state. The doc's earlier note says the outage is specific
to *creating a new key* — that did not hold here either.

A restart lever was tried to get a cold daemon: the `/diaglevel` hash flood
(`recon/setstring/diagflood.py`, ~1900 unique inserts) saturates the workers (transient
`000`s) but did not restart the daemon (`SWGen` still 1). `/reboot` is 403 (devmode bit 22,
no key). So a cold-daemon control needs a power cycle or a devmode unlock.

### Value-copy path: the copy is bounded, the overrun is elsewhere (r2, 2026-10-29)

The value copy is the percent/form decoder `0x1038a420`, reached from the parser core
`0x1038a508` and its wrapper `0x1038a5d4`. The handler passes the caps at `0x1000b2f0`
(`li r5, 0x80`, key) and `0x1000b2f8` (`li r7, 0x401`, value); the parser forwards them as
the decoder's `r5`. The decoder's only bound check is

```
1038a4bc: cmplwi cr7, r28, 1      ; r28 = remaining cap
1038a4c0: ble    cr7, 0x1038a4d4  ; cap <= 1 -> stop storing, set fail
1038a4c4: stb    r31, 0(r29)      ; store
1038a4c8: addi   r28, r28, -1
1038a4cc: addi   r29, r29, 1
...
1038a4e4: li     r0, 0
1038a4e8: stb    r0, 0(r29)       ; terminator at the un-advanced cursor
```

So it stores at most `cap-1` bytes and then writes the NUL at the current cursor: exactly
`cap` bytes, no off-by-one. The value buffer is `0x401` at `r1+0x1150` and the cap is
`0x401` — an exact fit; the key buffer is `0x80` at `r1+0x10d0` with cap `0x80`, and the
value starts exactly at `0x1150`, so the key cannot touch it either.

Device boundary, `.202` (write, then `SystemProperties#GetString`):

| value length | POST | read back |
|---|---|---|
| 1024 | 200 | 1024 bytes, byte-exact |
| 1025 | 200 | **empty** — the value decode fails, the parser returns 0, the handler proceeds with the still-empty value buffer and creates the key anyway |
| 2000 | **400** | — the body exceeds the reader's `0x800` destination (`cmplw r3, r30` at `0x1038a9d8`), so the 2000-byte "reset" is the reader bound, not the value cap |

Verdict: **the copy path is safe**; no overrun shape exists there. The overrun candidate
remains the handler's use of the property struct past its frame (`+0x804`/`+0x808`/`+0x80C`,
above), which the copy does not touch.

## Device dimensions — the coordinator's three, executed (2026-11-03, `.202` 57.23)

Scripts run on the LAN server (`192.168.4.160`), unit `192.168.4.202`
(Play:5, `57.23-74170`, live and answering 200 before each sweep). Liveness
watch = `/status/VERSION` every 2 s; `SWGen` re-read after each sweep.

### 1. `/notify` `seq` → vtable callback (10 shapes, **10/10 KILL**)

`GET /notify?nt=upnp:event&nts=upnp:propchange&sid=uuid:X&seq=<S>`; the handler
gates nt/nts then calls `singleton->vtable[0x2c](sid, seq, nts, nt)` with `seq`
as an opaque string. Every shape — including an **empty `seq`** — took the
daemon down; SWGen and `/status/zp` stayed healthy after each recovery
(`57.23-74170`, watchdog restart, unit healthy at the end):

| shape | seqlen | POST code | first down | recovered |
|---|---|---|---|---|
| empty | 0 | 405 | 5.1 s | 2.1 s |
| one-byte | 1 | 405 | 12.7 s | 2.5 s |
| at-cap | 1024 | 000 | 7.1 s | 2.1 s |
| over-cap | 1025 | 405 | 5.0 s | 2.1 s |
| 0x800 | 2048 | 405 | 15.6 s | 7.1 s |
| 4k | 4096 | 408 | 7.3 s | 2.1 s |
| embedded `:`/space/CRLF | 8 | 405 | 7.4 s | 5.1 s |
| `%00%00` | 6 | 000 | 9.1 s | 2.0 s |
| high-bit ×100 | 300 | 000 | 11.4 s | 2.2 s |
| sid-huge (1024 B `sid`) | 1 | 000 | 11.2 s | 2.1 s |

The `405`s are the handler's own rejection (shape invalid before the callback);
the `000`s are the connection dying with the daemon. Because **empty seq kills
too**, the fault is reached with any well-formed `/notify` GENA request — the
callback (or whatever the watchdog punishes) fires on the gate path itself, not
on a length-dependent path. This matches the doc's known 1 MB-POST kill but
pushes it down to *any* request: **`/notify` is an unauthenticated
one-request pre-auth DoS at any size on 57.23** — stronger than the recorded
1 MB shape, and consistent with the `vtable[0x2c]` call hitting live state.

### 2. `/setstring` recv short-read / slowloris (5 modes)

Fresh token per probe, body 260+ B (`csrfToken=<urlencoded>&key=roomName&value=AAA…`),
sent byte-at-a-time (32 B flush interval), 8 B chunks @0.2 s, 32 B chunks
@0.5 s, header-only hold, and half-body hold:

| mode | response | daemon |
|---|---|---|
| byte-0.05 | broken pipe mid-body | stayed up |
| chunk8-0.2 | no route to host | **down at 28 s**, recovered |
| chunk32-0.5 | no route to host | **down at 19.6 s**, recovered |
| halt-header (no body) | no route to host | **down at 23.9 s**, recovered |
| halt-mid | (daemon was down from the previous mode; skipped) | — |

The reader tolerates byte-at-a-time delivery but a **held-open connection with
a partial body takes the daemon down in ~20-28 s** — the same watchdog window
as the known kill. So the recv loop's terminator is fine for short reads, but
the *reader* (or its worker) is killed by the stalled-request state itself.
That is a second, independent pre-auth DoS shape on 57.23 (slow-body hang), and
it does not need a complete request at all.

### 3. `/support/directsubmit` csrfToken edges (11 shapes, 9 KILL)

GET returns the form with a token; body `csrfToken=<tok>&key=submit&value=<pad>`
(pad to exceed the 0x3e8 min). **Every 400 also took the daemon down**:

| shape | code | down |
|---|---|---|
| token missing | 400 | 11.3 s |
| token empty | 400 | 5.0 s |
| token valid | 400 | 19.6 s |
| token duplicated | 400 | 5.0 s |
| wrong-token-then-right | 400 | 5.0 s |
| right-then-wrong | 400 | 23.7 s |
| huge value (1300 B) | 400 | 19.6 s |
| extra name at cap 0x80 | 000 | 7.3 s |
| extra name over cap 0x88 | 400 | 5.1 s |
| token `%00`-suffixed | 400 | – (no down) |
| token raw `+` (unencoded) | 400 | – (no down) |

The unit came back healthy (`SWGen>1` after the sweep, `/status/zp` fine).
`directsubmit` never returned 200 to our fabricated `key=submit` — the token
gate works, but **any POST to it, valid or not, can hang the daemon**. With
`token-ok` also killing (19.6 s), the 400 path itself is lethal; the two
non-killing shapes are the raw-`+` one (400 before the parse gets deep) and the
`%00` one.

### Verdict impact

The bead's claim — "a reachable memory-safety or length-handling flaw that a
grammar-aware fuzz finds (crash/restart)" — is now **satisfied on the device
dimension**: three separate pre-auth request classes on 57.23 reproduce
daemon-down events deterministically (`/notify` any-shape, `/setstring`
slow-body, `/support/directsubmit` any-POST), each recovering via watchdog in
2-25 s. The parse layer remains clean (177 offline cases); the faults live in
the post-parse handler/callback layer that only the device exposes. The
static overrun candidate (`/setstring` struct tail past frame) is unaffected.

## 2026-11-03 (SageElm) — dispatch analysis: the `/notify` singleton chain, both builds

### 57.23-74170 (the killing build) — full chain resolved

- `/notify` handler `0x1028a5b8`: after the nt/nts gates it loads the singleton
  pointer from **`0x10545dc8`** (`.sbss`), then `vt = *(singleton)`,
  `fn = vt[0x2c]` and calls `fn(this, sid, seq, nts, nt)` — args confirmed:
  `r4=sid, r5=seq, r6=nts, r7=nt` (`0x1028a6b4`–`0x1028a6dc`), five registers,
  no request object on the stack in this build.
- **The only non-null writer of `0x10545dc8` in the entire image is
  `0x10222294`** (function `0x10222258`, reached only from `0x1021fbe8`): it
  bump-allocates from the module arena (allocator `0x10222778`, bump pointer at
  `ctx+12`, limit at `ctx+8264`) a block with **size 1**, memsets exactly
  **1 byte** to 0, and stores the pointer. No vtable is ever stored through the
  singleton slot itself.
- So the handler's `vt = *(singleton)` and `vt[0x2c]` read the first word of a
  1-byte arena block and 0x2c bytes past it — i.e. the *neighbouring arena
  allocations*, not a class vtable. Whether the call lands on zeros (→ NULL
  fetch fault) or on a neighbour's data (→ wild call into pool contents)
  depends on what the module arena holds at that moment, which matches the
  device signature: **first-down 5.0–15.6 s (variable, not instant)**, which is
  watchdog-window-shaped (a hang or a slow wild path), not a clean immediate
  SIGSEGV.
- The other singleton nearby, `0x10545d78` (5368-byte block, `[+0]=ctx`,
  `[+8]=0x1054dae8` static struct), is a *different* slot; the reset/shutdown
  function `0x100098f8` vcalls slots `0x40`/`0x28` on `0x10545dc8` the same
  unchecked way (exit path only).
- The arena allocator quirk worth recording: `0x10222778` succeeds only for
  sizes that are **not 8-aligned** (`r4 & 7 != 0` path does the bump alloc;
  aligned sizes and 0 return `-1`). 596 (0x254), 5368 (0x14f8 → 0x14f8 & 7 = 0
  … the big block is served by the `clrrwi+8` adjust), 1 — all consistent.

### 86.10-81100 (`.81`/`.86`) — same pattern, different slot, NO kill

- Binary: `captures/firmware/s2_overlay/bin/anacapad` (sha256
  `1b598f11…` = the `/jffs/app/install/bin/anacapad` the device's own
  `anacapad.sha256` names; the rootfs squashfs ships only the hash — the
  binary comes from the overlay).
- `/notify` handler **`0x4aa538`** (MIPS16): get_param of `nt/nts/
  content-length/sid/seq`, gates `[req+0] == 10`, `strcmp(nt,"upnp:event")`,
  `strcmp(nts,"upnp:propchange")`, then singleton from literal pool entry
  `0x4aa5fc` → **`0x6fd7b0`**, and `vt[0x3c]` (slot **60**, not 0x2c) with
  `(a0=sid, a1=seq, a2=nts, a3=nt, request on stack at +16(sp))`.
- **Live shot (this session, `.81`)**: canonical shape
  `nt=upnp:event&nts=upnp:propchange&sid=uuid:X&seq=1` → `405` in 0.06 s,
  `/status/zp` 200 continuously for 40 s (crash-oracle poll at 200 ms);
  empty-`seq` shape → `405`, daemon healthy after. **The 57.23 one-shot kill
  does not reproduce on 86.10-81100.** The 405 is the handler's own method/
  shape gate — the callback path is either not reached or benign on this
  build.
- The 57.23 device unit (`.202`) was offline (connection refused on :1400 and
  :1443 from both the workstation and the LAN server) during this session, so
  the 57.23 device dimension stands as recorded in the 10/10 table above.

### What this means for the corruption→control step

The wild call is `vt[0x2c]` (57.23) where `vt` is arena-neighbour data that the
attacker does not directly control but that *pooled request state* influences —
consistent with the variable first-down latency and with `/notify` being a
one-request pre-auth DoS. Next probes on a live 57.23 unit, in order of
information gain: (a) fire `/notify` immediately after boot vs after heavy
pool traffic (does first-down time or the 405-vs-000 split shift?); (b) if a
shape ever returns non-405, the callback ran — that is the pool-grooming
 foothold.
