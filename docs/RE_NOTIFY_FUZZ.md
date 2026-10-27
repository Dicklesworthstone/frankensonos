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
