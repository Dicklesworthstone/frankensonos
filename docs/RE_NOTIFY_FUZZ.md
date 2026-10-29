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
