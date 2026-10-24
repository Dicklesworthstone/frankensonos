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

## What the type-17 verifier actually hashes

`0x1000868c` (364-byte `621da74d` header) reads the payload with `FUN_10006a20(0, fd, buf,
len-16)` — **flag 0 = do not hash** (0x10006a20 skips `FUN_10006318` when the first argument
is 0; every other handler passes 1). So the record's own bytes are not part of the running
hash. It then:

1. `ZZ0121` (lib 0x1ad68) parses the header. It stores, for the **list2** region only,
   `param_1[0x425]` = pointer to the list2 count and `param_1[0x426]` = its byte length
   (0x850/0x854 region); the region runs from the list2 count word to the first byte after
   the last list2 record, i.e. it stops exactly at `cert_count`.
2. `ZZ0061` finishes the SHA-256 context at `0x1002c564` into `sp+0x22d8`.
3. `ZZ0124` -> `ZZ0125` (lib 0x1b284) verifies. With a non-empty list2 it
   (`*(int*)(section+0x850) != 0`):

   - `ZZ0115` copies the type-2 record (alg byte + digest) out of the section;
   - `memcmp` of that digest against the buffer from step 2;
   - `ZZ0117` recomputes the digest **over the list2 region stored in step 1** into a local
     and length-checks it;
   - `ZZ0110` -> `ZZ0109` RSA-verifies the signature over that recomputed digest, with the
     key resolved from the 20-byte name via `ZZ0039`.

Measured on the shipped 57.23-74170-1-16 package (type-17 at 0x558be1, payload 364 bytes,
list1 count 0, list2 count 2):

```
payload[0x10:0x49] = 00000002 00000002 00000021 017c50ba...6c1c 00000003 00000004 00000001
sha256(payload[0x10:0x49]) = dbd059c4a69503d0e6ad35f61670280e38c64963dcafdde4fd3e0879eaccfb48
RSA-recover of the 256-byte signature (key at libsonoscrypto+0xdae0) =
  3031300d060960864801650304020105000420 dbd059c4...accfb48
```

So the signed range is exactly **the list2 count plus the two list2 records (57 bytes)**.
Outside it: the `621da74d` magic, `header_len`, `version`, `field`, list1 (count and records),
`cert_count`, every cert entry (name, keytype, sig_alg, field, sig_len) and the signature
itself. `header_len` is only a bound (`<= payload length`, and the parse must consume
within it) — extending it with trailing junk inside the payload is safe for the signature.

Open, and the one thing not nailed down: the buffer compared at step 2 is the *running*
context, which for every other record type is fed with the record header (0x10006d9c in
`read_hdr`) and the section body (0x10006b14 in the buffered reader), and the measured digest is
the 57-byte list2 hash rather than a whole-file hash. There are exactly two `ZZ0062` (hash
init) call sites in the binary, both at the top of `FUN_10007fa0` (0x10008124 MD5,
0x100081e8 SHA-256), so a re-init before the type-17 record is implied but was not located;
Ghidra dropped a block as unreachable at 0x1000853c. Practically it does not matter: both
digests must be equal, and the signature covers the list2 one.

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
