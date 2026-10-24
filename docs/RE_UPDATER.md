# RE_UPDATER — live lane log

One lane of the 2026-10-24 root swarm. Facts with addresses/commands only; merge into FIRMWARE_PATHS.md when the lane closes.

Build: `captures/firmware/57.23-74170-1-16/upgrade` (PPC, stripped, `libsonoscrypto.so.1`
alongside it). Ghidra project `captures/research/ghidra-upd` (imported copies, image base
0x10000000 for `upgrade`; the lib is rebased +0x10000 by the loader script).

## Section table, dispatch and the two passes

`upgrade` is a two-pass tool. `FUN_10008e3c` (0x10008e3c) does:

```
iVar1 = FUN_10007fa0(file, cb, ctx, 0, 0);      // pass 1 - parse + verify
if (iVar1 == 0) FUN_10008890(file, ctx, 0, 0);  // pass 2 - write/flash
... FUN_10006670 -> FUN_10006720 -> FUN_100068c0
```

**Everything that touches flash is in pass 2, which only runs when pass 1 returned 0.**

Pass 1 dispatches section types through a 16-byte-entry table at `0x10017950`
(`[type, flags, code, handler]`, 13 entries, type 0 = terminator). Only the handler at
entry +0xc is called (`0x100083f8: lwz r10,12(r10)`), with `r3 = &state`:

```
type 21 -> 0x100073b8   type 2 -> 0x1000760c   type 3 -> 0x10007768
type 4,5,6,11,12,22,23 -> 0x10007db0          type 17 -> 0x1000868c
```

`0x10007e98` (MD5, `len <= 0x110`) is the entry with type 0xffffffff, i.e. a legacy
type-17 shape that the shipped 380-byte record does not fit.

Section length handling is uniform: every handler takes `*(u32*)(header+8) - 0x10` as the
payload size, and 0x1000868c additionally requires `len - 17 <= 0x293bf` (168895).

## Duplicate sections: "Extra section" aborts the pass

At 0x1000832c-0x10008340:

```
if (local_4ac != 0) {                       // local_4ac = raw header type, already swapped
    if (local_49c != 0x11)                  // parsed type; only 17 may repeat
        -> syslog(3, "Extra section %u (%u bytes)") and error 1
```

The error path goes to 0x10008234 (close) and then `if (!bVar1) goto LAB_10008134`, which
returns with the error code, so **a second section of any type other than 17 fails the whole
verify pass before a single byte is written.** Consequences for the trailing-section plan:

- appending another type-3 or type-4 can never work while the original is present;
- a trailing section must use a type the image does not already carry, or be type 17;
- the type-17 handler is the only repeatable one, so a *second* type-17 record is the only
  way to add signed metadata without dropping an existing section.

## What the type-17 verifier actually hashes — two separate values

`0x1000868c` (364-byte `621da74d` header) reads the payload with `FUN_10006a20(0, fd, buf,
len-16)` — flag 0 = do not hash (0x10006a20 skips `FUN_10006318` when the first argument is
0; every other handler passes 1). So the record's own bytes are not added to the running
hash. It then:

1. `ZZ0121` (lib 0x1ad68) parses the header and stores, for the **list2** region only,
   `param_1[0x425]` = pointer to the list2 count and `param_1[0x426]` = its byte length
   (0x850/0x854); the region runs from the list2 count word to the first byte after the
   last list2 record, i.e. it stops exactly at `cert_count`.
2. `ZZ0061` finishes the SHA-256 context at `0x1002c564` into `sp+0x22d8`.
3. `ZZ0124` -> `ZZ0125` (lib 0x1b284) verifies. With a non-empty list2
   (`*(int*)(section+0x850) != 0`):

   - `ZZ0115` copies the type-2 record (alg byte + digest) out of the section;
   - `memcmp` of **that digest** against the buffer from step 2 (`0xb43c`, `memcmp` at
     0x1e734);
   - `ZZ0117` recomputes a digest **over the list2 region stored in step 1** into a local
     and length-checks it;
   - `ZZ0110` -> `ZZ0109` RSA-verifies the signature over *that recomputed* digest, with
     the key resolved from the 20-byte name via `ZZ0039`.

So the record's 32-byte value is compared twice, but the two sides are different domains:

- **memcmp side: SHA-256 of the whole file prefix**, i.e. every byte from offset 0 to the
  start of the type-17 record, hashed raw as it is streamed (record headers via
  `read_hdr` 0x10006c2c, section bodies via the buffered reader 0x10006a20 -> 0x10006318).
- **signature side: SHA-256 of the 57-byte list2 region only.**

Verified on three builds by comparing the stored value with `sha256(file[:record_offset])`:

```
57.23-74170-1-16  7c50ba322becb3b11095cae199d48e579086fec63a213812a20ce91a60ba6c1c  equal
57.22-65130-1-16  580afcffc60cb479e0c8e6a8...                                      equal
86.10-80260-1-12  c5909deae55dc6fed982418c...                                      equal
```

and the 256-byte signature of the first one RSA-recovers to a DigestInfo whose digest is
`dbd059c4a69503d0e6ad35f61670280e38c64963dcafdde4fd3e0879eaccfb48` =
`sha256(payload[0x10:0x49])`, not the prefix value.

Outside both ranges: the `621da74d` magic, `field`, `cert_count`, every cert entry (name,
keytype, sig_alg, field, sig_len) and the signature. `header_len` is only a bound.

### Consequence for a patched image

Any byte changed before the type-17 record changes the prefix hash, so the stored digest
must be rewritten — and the list2 region then changes too, so the RSA signature must be
recomputed. `scripts/build_upd_rootfs.py` does the payload swap, the re-encryption and the
digest rewrite; it cannot re-sign, because the verifier key (`libsonoscrypto+0xdae0`, name
`d67d7a07…`) is not the model content key (`model16-private.pem` has a different modulus and
only unwraps envelopes).

## `FUN_1029c34c` — the only writer of `/var/run/upgradeinfo`

In `anacapad` (57.23). Registered as a timed job from 0x10008eec
(`FUN_103853f4(manager, name, 0x1029c34c, 0x7fffffff, 10)`), so it is a periodic task, and it
sleeps 10 s at the end of every attempt (`0x1029c538 sleep(10,1)`).

Sequence:

```
FUN_102c691c(ctx, 10000)                       // the arming predicate, 10 s timeout
  false -> syslog "Failed to query cloud settings", set *out = 0x7fffffff, return
true  -> fopen("/var/run/upgradeinfo", "a")
```

`FUN_102c691c` (0x102c691c) is a plain cloud fetch:

1. builds an `application/json` request for the literal path
   **`/product-settings/v3/settings`** (host from the usual cloud-host provider, TLS);
2. `FUN_1035fb44` must return 0 (request dispatched — DNS/TLS/connection failure is non-zero
   and returns 0);
3. reads the body in 0x1000 chunks (`FUN_1035d160`) into a JSON parser (`0x103901a4`);
4. succeeds only if the parser reports completion (return 0) **and** the parsed object has a
   non-zero flag at `+0x14e` **and** a non-zero int at `+0x108`.

Then, and only then, the writer emits `MinVersion: [<v>]` (and, gated on
`FUN_1029c328(global)` and a flag byte at object+4, `PrevSWGenMinVersion: [<v>]`) and
fcloses. So there is **no local or offline arming path**: without a successful, parseable
`/product-settings/v3/settings` answer carrying those fields, `/var/run/upgradeinfo` is never
created and `/bin/upgrade` is never started. The MITM plan on re-beginsw-fdp is the only
route, and the response must satisfy the two field checks, not merely be 200 OK.

The other reference to the job shows *when* it matters: `FUN_1029c55c` (0x1029c55c) writes
the progress/Ignore/Report/SonosID/HouseholdID block, `__xstat`s a path, and unless a flag at
+0x92c is set it calls the manager's vtable +8 with `0x1029c34c` directly (0x1029c9b8) after
logging "scheduling deferred shutdown for online update". So the settings fetch is run as part
of the deferred-shutdown path (as well as being registered as a timed job), which is why the
fetch has to succeed *before* the reboot that would start `upgrade`.

## Tooling

`mips-linux-gnu-objdump -d -m mips:16` applies to the S2 builds; for PPC use
`llvm-objdump -d --triple=powerpc` (note: `llvm-objdump` prints bare register numbers,
`addi 4, 4, -31676`, so grep the decimal immediate, not a register name). Ghidra 12 headless
on the `ghidra-upd` project decompiles both binaries; the script set in
`captures/research/ghidra/scripts` (`DecompForce`, `Callers`, `FnStart`, `StackVars`,
`Dis`) is what the addresses above came from.

## First device run of the patched image (2026-10-24)

Fixture: the base package with the type-4 payload replaced by
`patched-rootfs-v30.bin` re-encrypted to the record's own 20-byte recipient, the build
bumped to `74171` and the prefix digest rewritten — 5,606,749 bytes, the same total as the
base. Round-trip checked: the new type-4 decrypts back to the patched rootfs byte for byte.

Target `.4.202` (S5, 57.23-74170, model 16), served from `.4.160:8127` with a byte-counting
handler, triggered once with `BeginSoftwareUpdate`.

| run | bytes served of 5,606,749 | reboot | version after |
|---|---|---|---|
| patched, plain | 5,242,880 (93.5%) then client close | no | 57.23-74170 |
| patched, repeat | 5,242,880 | no | 57.23-74170 |
| patched, re-encrypted stock rootfs | 4,194,304 | no | 57.23-74170 |
| patched, 32 KiB writes + keep-alive | 1,114,112 | no | 57.23-74170 |
| **control: unmodified base package** | **5,606,749 (100%)** | **no** | 57.23-74170 |

Class: **fetch aborted mid-stream, no reboot, no handover.** Only one GET arrives; the
`/bin/upgrade` fetch never starts (that would be a second request with no query string).
The identical server completes the unmodified package twice, so the abort tracks the changed
bytes rather than the transport; the stop offset varies from 1.0 to 5.0 MiB, so the daemon
appears to validate as it streams and drops the connection at the first thing it dislikes.
What it dislikes is not yet isolated — candidates are the bumped build, the fresh envelope
IV/ciphertext and the rewritten digest, each of which can be varied independently next.

## Appended unused section types (2026-10-24): parsed, but pass 2 never runs

The prefix digest makes an append the only signature-neutral edit, so the appended-type
route was probed rather than the rootfs swap. Fixtures are the stock package plus one record
after the genuine type-17, prefix byte-identical (checked: `sha256(prefix)` unchanged,
`7c50ba32…`), built with `scripts/append_upd_section.py`. Shapes copied verbatim from a
34.16 image, where type 5 is `len 17 / field 0 / payload 00` and types 9 and 10 are
`len 528 / field 0 / 512-byte plaintext`.

| # | appended record | bytes served | fetch | reboot | /VERSION after |
|---|---|---|---|---|---|
| 1 | type 5, field 0, payload `00` | 5,606,766 | 19 s | none (0.5 s ping blip) | 57.23-74170 |
| 2 | type 5, field 0, repeat | 5,606,766 | 18.6 s | none (0.5 s blip) | 57.23-74170 |
| 3 | type 5, field 1, payload `00` | 5,606,766 | 17.7 s | none | 57.23-74170 |
| 4 | type 5, field 4, payload `00` | 5,606,766 | 22.6 s | none | 57.23-74170 |
| 5 | type 5, field 4, enveloped `00` (model key) | 5,607,111 | 21.0 s | none | 57.23-74170 |
| 6 | type 9, field 0, 34.16 payload | 5,607,277 | 21.8 s | none (0.4 s blip) | 57.23-74170 |
| 7 | type 10, field 0, 34.16 payload | 5,607,277 | 21.0 s | none | 57.23-74170 |
| 0 | **control: stock package, no append** | 5,606,749 | 25 s | none | 57.23-74170 |

Every appended variant fetches to the last byte and then goes quiet: no second reboot, no
>40 s accepted-install path, `/status/ZPLocalSettingsFile/../VERSION` unchanged. Class is
therefore the **reject** one, i.e. the record is parsed but pass 2 does not flash. Field 0/1/4
and raw vs enveloped make no difference, and neither do types 9/10, so the appended record is
neither rejected as "Extra section" (that would abort the fetch earlier) nor enough to carry
pass 1 to a successful return.

Recorded but not yet explained: intermittent sub-second ping blips 3-5 s into the transfer in
runs 1, 2 and 6, absent in the control and in runs 3, 4, 5 and 7.

## Type-21 (patch list) mutation series (2026-10-24)

Type 21 is the first record, plaintext, and parsed before any key: `FUN_1000760c` mallocs
`len-16+1`, reads the payload with hashing on, NUL-terminates it, then `FUN_100057d8` runs
`strtok(buf, "\n")` and `strchr(line, ':')` per line. Fixtures replace the payload in place
(`build_upd_rootfs.py --raw`) and keep every other byte; the prefix digest is rewritten, and
for an unchanged payload the list2 digest is unchanged too, so that control still verifies.

| fixture | payload | served of ~5.6 MB | abort point | reboot |
|---|---|---|---|---|
| **control: payload copied verbatim** | 68 B, stock | **5,606,749 (100%)** | none | none |
| one token 1 KB | 1025 B | 5,607,706 | **524,288** | none |
| one token 8 KB | 8193 B | 5,614,874 | **524,288** | none |
| one token 64 KB | 65537 B | 5,672,218 | **524,288** | none |
| 20 tokens | 340 B | 5,607,021 | **524,288** | none |
| 100 tokens | 1700 B | 5,608,381 | **524,288** | none |
| every line without a colon | 32 B | 5,606,713 | **524,288** | none |
| embedded NULs | 41 B | 5,606,722 | **524,288** | none |
| tokens without `_PATCHES` | 27 B | 5,606,708 | **524,288** | none |
| record `len` = 0xFFFF, 68 B of data | 68 B | 5,606,749 | **524,288** | none |
| stock prefix of the list + junk | 415 B | 5,607,096 | **524,288** | none |

Class: **daemon aborts the download at exactly 512 KiB** (524,288 = the send-buffer boundary,
so it reads one buffer, evaluates, closes). No reboot, no daemon-down, `/VERSION` and the
version unchanged, recovery immediate. The payload copied verbatim completes, so the check
keys on the type-21 *content*, not on the rebuild: any difference in the patch list stops the
fetch at the first decision point. Size, token count, colon presence, NULs, suffix presence
and the extended-`len` framing all behave identically — the parser itself never faults.

Sub-second ping blips appeared in 5 of 12 runs, including the control, so they are not a
type-21 signal.

## Positive control (2026-10-24): what an accept actually looks like

Serving the genuine `57.22-65130-1-16` to `.4.202` (then running 57.23-74170) gives:

```
full read  5,602,653 / 5,602,653
ping down  67.2 s -> 97.4 s   (30.2 s, the box reboot)
back as    57.22-65130   (/status/zp and /status/ZPLocalSettingsFile/../VERSION agree)
```

and serving the genuine 57.23 package back restores `57.23-74170` the same way. So the
accept class is **full read + ~30 s box-down + a changed version**, and a full read on its own
is *not* an accept — every earlier "full read, no change" run above is a reject. The
sub-second blips seen in some runs are unrelated (they also occur on the control).

## Type-1 / version-record series (2026-10-24)

The 28-byte file header is the version record; `FUN_100073b8` requires `len == 0x1c`, swabs
`major.minor-build` and prints it with `%u.%u-%u` into a 1024-byte buffer, then stores
`ver_this`. Fixtures patch the triple and rewrite the prefix digest (the list2 digest and
therefore the signature stay valid).

| version | served | class |
|---|---|---|
| 57.23-74170 (control, payload copied) | 5,606,749 (100%) | full read, no change |
| 4294967295.4294967295-4294967295 | 5,606,749 (100%) | full read, no change |
| 999999.999999-999999999 | 5,606,749 (100%) | full read, no change |
| 57.23-74171 | 5,606,749 (100%) | full read, no change |
| 57.23-74169 | 5,606,749 (100%) | full read, no change |
| 57.22-74170 | 5,606,749 (100%) | full read, no change |
| 57.23-0 | 5,606,749 (100%) | full read, no change |
| **0.0-0** | 524,288 | **512 KiB abort** |
| **0.23-74170** | 524,288 | **512 KiB abort** |

Only a zero **major** trips the 512 KiB gate; minor/build and even a full-`u32` version are
accepted for the download. Formatting never faults — the 1024-byte `snprintf` buffer holds the
longest possible `%u.%u-%u` (33 bytes). No version variant reaches pass 2, including a higher
build with a still-valid signature.
