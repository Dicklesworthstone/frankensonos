# RE_WRITE_PRIMITIVES — live lane log

One lane of the 2026-10-24 root swarm. Facts with addresses/commands only; merge into FIRMWARE_PATHS.md when the lane closes.

Build under study: 86.10-81100 (S2), `captures/firmware/s2_overlay/bin/anacapad`
(MIPS16; `mips-linux-gnu-objdump -m mips:16`; Ghidra will not form functions in
0x4aa4..-0x6026.. so those are read as assembly).

Live units used: .81 and .86 (both 86.10-81100, S2), .202 (57.23, S1 control).

## The 403 gate is a devmode-statement bitset, and the setter is the statement path

Answer to Goal B, and it also explains every dead end in Goal A.

- `ZZ0018` (libsonoscrypto, MIPS32 at `0xad8c`; imported by anacapad) is a
  **backward-counted bit test**: `if (idx < n*8) (bits[n-1-(idx>>3)] >> (idx&7)) & 1`.
- `sonosMuseRESTStream::checkPerm(stream, idx)` is `0x4f3824`; it calls
  `ZZ0018(stream + *(stream+1028), *(stream+1032), idx)` under the stream mutex.
  Wrappers: `0x4f2080` = bit **22**, `0x4f2094` = bit **18**, `0x4f3858(reg, n)`
  = arbitrary bit.
- The bitset is the **raw devmode statement** in the devmode object, not a
  request-supplied blob: `RDevMode::update` (`0x4f3a40`) memsets 1024 bytes at
  the object base and memcpy's the statement there (`0x4f3bbc`, `0x4f3bc6`),
  storing its length at `+0x400` and four extracted fields at `+0x404..+0x410`.
  The memcpy only runs after `ZZ0137` verification returns non-zero
  (`0x4f3b9c`), and the whole statement is capped at `< 1025` (`0x4f3b56`).
- Gate map, measured on the table entries: bit **18** for `/advconfig`,
  `/testenv`, `/diaglevel`, `/diagmsg`, `/logger` (handler head calls
  `0x4f2094`); bit **22** for `/reset`, `/reboot`, and the page family
  (`0x4f2080` at `0x4aa926` / `0x4f25fa`).
- `sonosFactoryResetFull` (`0x4abf66`) writes `/jffs/app/run/factoryReset.txt`
  = `SOFT` (`0x4abefe`, path from `app/run` + `factoryReset.txt`, mode 0x301),
  then reboots — but it is behind the bit.

Consequence: on S2 no bit is set without a verified statement. There is no
`setValue` or other runtime-policy setter import in anacapad; the only import of
the bitset family is `ZZ0018` (test).

## A request-chosen **read** exists; the matching writer does not

- `/status/ZPLocalSettingsFile/<name>` (`0x478fe0`, string `ZPLocalSettingsFile`
  at `0x666654`) percent-free path: it joins the settings root (`*(0x7004c4)`,
  default name `localsettings.txt`) with `<name>` (`0x4d4448`), opens and
  returns the file. Verified live:
  `GET /status/ZPLocalSettingsFile/../VERSION` → `86.10-81100`;
  one level of `..` is honoured (`..` = `/jffs`), two levels 404.
- The *only* callers of that path builder are three constant-name sites
  (`0x47a06c`, `0x47f70c`, `0x4ac18e`). `4d440c` (the default-name variant) has
  exactly one caller, the status reader above. So the name-taking path exists
  with **no request-driven writer**.

## What actually writes pre-auth on S2 (all fixed names)

- `SystemProperties#SetString` — open on plain SOAP (verified: `GET` style
  500 only from curl's HTTP/1.1 framing; a raw HTTP/1.0 POST with the exact
  `SOAPAction` answers 200). Writes the class-named settings entry, not a file
  per key. `/getsetting` and `/status/ZPLocalSettingsFile/<file>` both still
  read back empty wrappers for `localsettings.txt` / `householdsettings.json`,
  so the store path on S2 differs from the `/jffs/settings` default here.
- `/devmode` POST — open, reaches the verifier. Requires
  `content-type: application/x-www-form-urlencoded` (else 400), a csrfToken
  from a fresh GET, and a base64 `statement`; a syntactically valid blob gets
  403 (invalid signature). Its store path is fixed: `/tmp/devmode.tmp.bin`
  (`0x4f3f00` store, `0x4f3ef8`) and the persist root `debug/devmode.bin`
  (`0x4f3efc`, `0x4f3f40`), both open `O_WRONLY|O_CREAT|O_TRUNC` 0600.
- `/support/directsubmit` POST (200/400), `/notify`, `/status/*`, `/xml/*`,
  `/unlock*`, `/mfgunlock` — no name from the request.

## Not reachable pre-auth on S2 (measured 403/405)

`/raw`, `/raw/setrecoveryurl`, `/raw/forceupdate`, `/device_account*`
(bit 8/28 or the page family), `/getsetting`, `/advconfig`, `/testenv`,
`/diaglevel`, `/diagmsg`, `/logger`, `/jobs`, `/reboot`, `/reset`, `/unlock`.
`:1443` serves the same body for `/raw` (403) and additionally
`/device_account/registration[/state|/status]` as read-only JSON (200); the
POST verbs there answer 405. On `:1443` `/device_account` without a subpath is
400, not 403.

## Route table shape (for whoever continues)

`:1400` dispatch table lives in the `.gcc_except_table` range: entries are
`{char *path, handler|1, flags, 4 pad words}` at stride `0x1c` (e.g. `/devmode`
at `0x6e7034` → `0x4aa959`, `/raw` at `0x6e6de8`, `/device_account` at
`0x6e6e04`). `flags` is a method/meta field, **not** the permission mask
(`/advconfig` and `/devmode` are both 0 and differ in behaviour). Prefix routes
carry 0x100/0x10000000; exact routes carry 1/2/6/0xa. Handler pointers are odd
(ISA bit set).

## Status of Goal A

No pre-auth name-controlled file write on 86.10-81100 today. The chain that
would close it after a verified statement: bit 18 opens `/advconfig` (fixed
`/jffs/netsettings_mod.txt`), bit 22 opens the pages; only the devmode
statement path then yields `Configure.dev` at `/jffs/system/`, and even that
needs the name-taking writer that this build does not expose. Next reads if the
lane reopens: the `4ac18e` constant-name callers (does any of them take a
*setting* as the name?), and the devmode statement parser (`ZZ0137`) for an
overrun that reaches the 1 KB bitset without verification.

## Exact commands used (rerun before trusting any line above)

Toolchain / dead ends:

```bash
# MIPS16 decode; plain objdump mis-decodes, LLVM decodes as 32-bit
mips-linux-gnu-objdump -d -m mips:16 --start-address=0x4f3a40 \
  --stop-address=0x4f3f50 captures/firmware/s2_overlay/bin/anacapad
# bitset helper is MIPS32 (jalx target), read with -m mips:isa32
mips-linux-gnu-objdump -d -m mips:isa32 --start-address=0xad8c \
  --stop-address=0xadcc 57.23-74170-1-12/libsonoscrypto.so.1
# whole-file disassembly used for greps (35 MB; puts the table region in
# .gcc_except_table so `objdump -d` alone never shows it)
mips-linux-gnu-objdump -d -m mips:16 captures/firmware/s2_overlay/bin/anacapad > /tmp/s2.dis
# Ghidra headless (lock is exclusive: never run two at once)
/opt/homebrew/Cellar/ghidra/12.0.1/libexec/support/analyzeHeadless \
  captures/research/ghidra-8610 proj -process anacapad -noanalysis \
  -scriptPath /tmp/gh -postScript <Script>.java
```

Live probes (read-only; both units 86.10-81100):

```bash
curl -s -o /dev/null -w '%{http_code}\n' http://192.168.4.81:1400/raw
curl -s http://192.168.4.81:1400/status/ZPLocalSettingsFile/../VERSION
curl -s -o /dev/null -w '%{http_code}\n' https://192.168.4.81:1443/device_account/registration/state
# devmode POST must be raw HTTP/1.0 + exact content-type + fresh token, e.g.
tok=$(curl -s http://192.168.4.81:1400/devmode | sed -n 's/.*name="csrfToken" value="\([^"]*\)".*/\1/p')
body="csrfToken=$tok&statement=AAAA&button=submit"
{ printf 'POST /devmode HTTP/1.0\r\nHost: 192.168.4.81\r\nContent-Type: application/x-www-form-urlencoded\r\nContent-Length: %d\r\n\r\n%s' "${#body}" "$body"; sleep 2; } | nc -w 8 192.168.4.81 1400
# -> 403 (invalid signature); statement=x -> 400; a 64-byte base64 -> 403
```

Controller for the bitset test is `sonosMuseRESTStream::checkPerm @0x4f3824`;
`ZZ0018` is the ONLY import of the family, so no runtime-policy setter exists.

## Open thread at checkpoint

`0x4ac18e` is the one path-builder caller whose name register is not visibly
assigned next to the call: `a0 = sp+4272` is the output and `a1` is inherited
(previous call `0x495214` at `0x4ac184` takes `a0 = sp+4788` only). The handler
is a large `save a0,2040` frame (`0x4ac11c`) with 33/65/129-byte memset blocks.
If that inherited `a1` is a request/setting string rather than a literal, this
is the missing writer; read `0x4ac0c0..0x4ac1c8` with the register liveness of
`495214` in mind. Its entry in the `:1400` table was not located (no pointer to
`0x4ac11c`), so it may be reached from the `/status` command side.

## Checkpoint 2: table enumeration, /mtmhhsetup, /mfgunlock, oracle limits

**Read oracle semantics (both units, .81 and .86).**
`../system/Configure.dev` → 200, 111 bytes (empty `<ZPSupportInfo/>`);
`../system` (directory) and a nonexistent name → the same 200/111;
`../VERSION` → 200, 170 bytes with `<File name='/VERSION'>`;
`../../VERSION` → 404. So one `..` resolves to `/jffs`, but **200/111 alone
does not prove a file exists** — the positive test is a payload marker in the
read-back, and 111 for `../system/Configure.dev` is the current "absent or
empty" baseline on both units.

**The `/status` command surface has no writer.** The file/command table
(`0x6e6b80..0x6e6e00`, entries `{path, handler|1, flags}`) is read-only, and
the special names are inline `strcmp`s in the single dispatch function
`0x478fe0`: `ZPLocalSettingsFile`, `ZPExpirationTime`, `JFFSRoot`,
`OnlineUpdateBaseURL`, `ContinueAfterIPChange`; all route to `0x478fe0`/its
file reader. A separate 5-entry keyed list at `0x6e7258..0x6e72b8`
(`{name, 0x478fe1, 0, 0x80, 1, ""}`) is the key whitelist, not writers.

**`0x4ac11c` is `/mtmhhsetup`, and its `a1` is not a request name.** Entry
`0x6e714c..0x6e7160`: `{"/mtmhhsetup", 0x4ac11d, 0x100, ...}` (same locked class
as `/unlock`). At `0x4ac18e` the call is `4d4448(out = sp+4272)` — `4d4448`
loads the settings root itself (`lw a1, 0x7004c4` at `0x4d444a`) and has **no
name argument**; the name comes from a second helper `4d4320(out, component
array, count)` (`0x4ac1d6`, appends `'/'` + each component). Here the component
is the literal `netsettings.txt` (`0x4ac7e8`). The handler then wraps a
`SettingsFile` with `SettingsFileCBWrapper` (`0x4ac20a`) and reads
`HouseholdID` (`0x4ac244`), comparing a form `hhid` on POST — fixed file.

**`/mfgunlock` writes nothing.** `0x4ccee8` dispatches: GET → `0x4ccc30`
(challenge: `ZZ0071` + `RserializeData` into the caller buffer), POST →
`0x4ccac8`; both only produce/consume the signed blob. Entry
`0x6e7018..0x6e7024` is `/mfgunlock → 0x4ccee9, flags 0x100`.

Net: on 86.10 the request-chosen-name reader and the fixed-name writers are the
whole pre-auth surface; every candidate name-taker (`/raw`, `/device_account*`,
`/mtmhhsetup`, `/mfgunlock`) is behind the devmode bits.

## Delivered payload for q57 (no flash)

Built with `captures/research/rootbuild/build_rootfs.py` (committed; images are
git-ignored). Artifacts in `captures/research/rootbuild/`:

| file | sha256 |
|---|---|
| `rootfs-patched.bin` (squashfs 3:0, 4571136 B = base) | `864cf31790a377bdbdd617e06fc48de9b1572bc733cb2b6dff4683f7f04c9af9` |
| `type4-envelope.bin` (model16, header_len 314, zero IV) | `5e7fe82ef7a37adcedc77b4a2e430828247d9650dcbe91436e89a937b9bbd14b` |
| `pwned-q57.upd` (5606749 B = base, 10 sections) | `73587faceeba8885f3fbc750994ad3f4808b2e407808c36d9b0bf85da7f24370` |

Tree delta vs the base image is exactly: `jffs/system/` (new dir),
`jffs/system/Configure.dev` 0644 (`f440a443fcfa14897561b243f628204098aa6ced16da384090aa880fc665aac4`),
`PWNED-MARKER` 0644. `Configure.dev` is the three-command script (telnetd 2323,
dropbear 22, `touch /tmp/device_unlocked_flag`) so presence alone unlocks the
unit via `/wifi/Configure` and rcS `sh`s it every boot.

Envelope layout confirmed against the base type-4 section and re-verified by
decrypting our own output: `886499ca`, u32be header_len, u32be 0x10000, 8 zero
bytes, `u8 len, u8 3`, recipient (copied from the base envelope,
`fd88f2…d437`), u32be 1, u32be 256, RSA-OAEP-SHA1 key, u32be 1, u32be
data_len, 16-byte zero IV, AES-128-CBC/PKCS#7 body. The plaintext round-trips
byte-identically to `rootfs-patched.bin`; the whole type-17 and every other
section are untouched, and only payload byte 8 of section 1 is bumped
(`0xba`→`0xbb`, a monotonic revision, not a digest).

Flash is deliberately not done; the envelope path was handed to LavenderLake
and GoldOwl (`am`, sender `MistyOx`) with these SHAs.

The script pins mtimes (`T = 1771344440`, the base image date) and `s_minor`,
so the *tree* is reproducible, but mksquashfs 3.4 is not byte-deterministic
(two back-to-back runs differ throughout the compressed image). The SHAs above
bind the delivered artifacts, not a rebuild.

## Lane status

Paused: no pre-auth writer with a request-chosen name exists on 86.10-81100 as
far as this lane can see (reader-only name path, all writers behind the devmode
statement bits). Resume from the `open`64/`fopen64` sweep over strings built from
`ap_request_get` when the gate side or a signature lands.

Tool provenance: `squashfs3.4.tar.gz` sha256 `9ea1a9b3bd4f387ca11b5e96f00f8ae996fb81c4c7ad41f7c7f359917628a339`, built in `debian:bookworm-slim` with `CFLAGS="… -std=gnu89 -fgnu89-inline -fcommon -include sys/sysmacros.h -include fixmacros.h -DFNM_EXTMATCH=32"` (fixmacros defines `minor`/`major`), `make clean && make mksquashfs` → binary sha256 `8b130f61066b3b573080a399a60f39ced56b12535cf36c074c5cc6a7e20871c0`; artifacts re-hashed in place: `pwned-q57.upd` `73587fac…f24370`, `rootfs-patched.bin` `864cf317…4c9af9`, `build_rootfs.py` `a7978ac7…2723e5`.

## /status/JFFSRoot and the path join (static read, 2026-10-24)

Trigger surface: `GET /status/JFFSRoot/<name>` and `/status/ZPLocalSettingsFile/<sub>` are both 200-empty
and pre-auth on 57.23-style targets. The dispatcher is `0x478fe0` (anacapad, MIPS16), which `strcmp`s the
next segment against `ZPLocalSettingsFile`, `ZPExpirationTime`, `ZPForcedUPnPExpirationTimeout`,
`JFFSRoot`, `ServerRoot`, `ContinueAfterIPChange`. `JFFSRoot` calls the imported **`setJFFSRoot`**
(`libsonosminiutils`, PPC, `0x61d0`) with the raw segment.

**`setJFFSRoot(name)` (0x61d0..0x6234)** — `freeJFFSRoot()`, then **`__strdup(name)`** (PLT reloc
`0x203ec -> __strdup`), and the pointer is stored into `g_pszJFFSRoot` and a second twin global. There is
**no length limit, no `PATH_MAX` check, no leading-slash check and no empty-string rejection**. The root
lives in the heap until the next call.

**The join, `constructJFFSRootPath(dst, size, name)` (0x6238)**:

- no-name path: `strlcpy(dst, *g_pszJFFSRoot, size)` (PLT `0x20404 -> strlcpy`);
- named path: `__snprintf_chk(dst, size, 1, -1, fmt, *g_pszJFFSRoot, name)` (PLT `0x2054c`), i.e.
  `root "/" name`;
- both are **bounded by the caller's size**; the *name* length is **never checked before the append**, it
  is truncated, and truncation is only signalled afterwards by comparing the return value with `size`
  (0x6298: `cmplw 31,3`) to yield 1/0. Callers do test that value (0x47270c, 0x4f3d96, 0x4f3e54).

Caller sizes and buffers: `0x4726f2` size **256** into `sp+24` (frame `save 328` — 24+256 = 280 fits, the
adjacent byte at `sp+280` is a separate flag); `0x4eda50`, `0x4f3d8c`, `0x4f3e4a` size **1024** into
`sp+32`/`sp+24` with 1096/1232/1064-byte frames. The `/status/ZPLocalSettingsFile` branch instead uses
`FUN_004d440c` → `FUN_004d42c8`/`FUN_005212e8` `std::string`, and `FUN_004d4320` concatenates the root and
parts with `append('/')` + `append(std::string)` — **unbounded by construction** (heap).

**Maximum name that fits:** `size - strlen(root) - 2`. With the default root `/jffs` (5) that is **249**
for the 256-byte join and **1017** for the 1024-byte ones.

**Predicted failure mode: truncation, not overflow.** Both primitives are length-bounded, so a long name
produces a *cut* path (the real file is not found, or two names collide on the same truncated path) rather
than a smash. The root is the lever: a root of length ≥ 254 makes every 256-byte path degenerate to the
root prefix alone; ≥ 1022 does the same to the 1024-byte sites. Only if a caller ever passes a size larger
than its own buffer would this become memory corruption, and the three sites checked do not.

### Boundary values worth testing (cheap GETs, no state needed beyond the root)

1. `/status/JFFSRoot/` + names of **245, 248, 249, 250, 252, 253, 254, 255, 256, 257, 511, 512, 1023,
   1024, 1025, 4095** `A`s, then `/status/ZPLocalSettingsFile/ZPLocalSettingsFile` — confirm the path is
   cut at 255 total and that 255/256/257/1024 behave identically once truncated.
2. Roots of length **0, 1, 4, 5 (`/jffs` control), 200, 250, 253, 254, 255, 256, 1022, 1023, 1024, 4096**,
   re-reading the localsettings path after each: expect the join to collapse toward the root as it grows
   and a *silent* full-string swap once root ≥ size.
3. A root **without a leading slash**, a root with `/../` segments, and a root of exactly `size-2` chars —
   after `setJFFSRoot` none of these is normalised, so they surface in the joined path verbatim.

## /getrs?id=N — index map and scan (S1 .202, 2026-10-24)

`/getrs` `atoi`s the id, indexes a settings table and indirect-calls the entry. Raw sockets so an RST is
distinguishable from a clean close; `/status` liveness after any anomaly, every 200 requests, and at the end.

Map rows, each a separate request:

| id | status | body | headers |
|---|---|---|---|
| `-1` | 200 | empty | dummy `X-RINCON-CONTENT-VERSION: 4294967295`, `X-RINCON-LAST-UPDATE-DEVICE: RINCON_FFFFFFFFFFFF99999` |
| `0` | 200 | empty | same |
| `1` | 200 | empty | same |
| `100` | 200 | empty | same |
| `65535` | 200 | empty | same |
| `32768` | 200 | empty | same |
| `65536` | 200 | empty | same |
| `131072` | 200 | empty | same |
| `0x7fffffff` (bit 31 set) | 200 | empty | same |
| `0x80000000` | 200 | empty | same |
| `0xffffffff` (`-1` as u32) | 200 | empty | same |

Scan: `16384..16600` step 1, plus a 2 000-value uniform sample in `0..1 000 000` (seed 7), with a 20 s gap
and a liveness read every 200 requests.

**Denominator: 2 229 requests. Result: 2 229 × 200 (100 %), 0 RST, 0 timeout, 0 non-empty bodies, liveness
200 at all 11 gaps and at the end.** Latency is jitter, not structure — median ~40 ms, p99 609 ms, one
3 278 ms outlier, scattered rather than clustered on indices.

**Verdict: table reads are benign; no crash.** Every index from `-1` through `0xffffffff` behaves identically
and the unset-entry dereference never faults, so black-box probing is exhausted — only the table base and
entry stride in the 57.23 `anacapad` can name populated indices now. Runner:
`captures/research/getrs_probe.py`.

## AddOAuthAccountX argument matrix (S1 .202, 2026-10-24)

Unauthenticated `SystemProperties#AddOAuthAccountX`, ten declared args in SCPD order: `AccountType,
AccountToken, AccountKey, OAuthDeviceID, AuthorizationCode, RedirectURI, UserIdHashCode, AccountTier,
AccountUDN, AccountNickname`. Each arg is copied by the shared chunked copier `0x4be6fc` *before* the per-arg
validators run, so an oversized value always reaches the copy even though the action answers 402.

20 cases (control + ten args at 2 048 + empty/1 B/2 050/100 k + all-ten-long + three integer spellings):

| case | body | HTTP | SOAP | ms |
|---|---|---|---|---|
| control (all short) | 607 | 500 | 402 | 54 |
| each of the ten args at 2 048 B (10 cases) | 2 645-2 654 | 500 | 402 | 19-101 |
| `AccountType` empty | 606 | 500 | 402 | 44 |
| `AccountToken` 1 B | 605 | 500 | 402 | 82 |
| `AccountToken` 2 050 B | 2 654 | 500 | 402 | 63 |
| `AccountToken` 100 k | 100 604 | 500 | 402 | 451 |
| `AccountKey` 100 k | 100 604 | 500 | 402 | 325 |
| all ten at 800 B | 8 572 | 500 | 402 | 109 |
| `AccountType` as `abc` / `-1` / `0x100` | 608-611 | 500 | 402 | 31-34 |

Every case: `200` liveness before and after, no RST, no timeout, no wedge. **Verdict: the arg surface is
benign**; the only observable is latency, which scales with the 100 k bodies (325-451 ms against a 20-80 ms
baseline) — i.e. the copy is real and linear, but nothing overruns and the 402 verdict never changes.

Because the control 402s too, the ten-arg body still dies in a validator before the account logic, so this
matrix cannot discriminate arg *content* — only arg *size*, which is what it was for. Runner:
`captures/research/addoauth_fuzz.py`.

## Type-21 record: the feature-support list (static, 57.23 `bin/upgrade`, 2026-10-25)

Correction to the earlier note: the section dispatch table is **two parallel arrays** — types at
`0x10017950` (stride 0x10) and handlers at `0x1001795c` (stride 0x10). Type **1** is the version record
(`0x100073b8`, 12-byte payload, `snprintf` into a 0x400 stack buffer, numeric triple compare in
`0x1001132c`). Type **21** is `0x1000760c` and is not a version at all.

`FUN_1000760c` (type 21):

```
iVar3 = *(int *)(param_1[1] + 8);      // record length from the header
uVar4 = iVar3 - 0x10;                  // payload length
__ptr = malloc(iVar3 - 0xf);           // payload + 1
FUN_10006a20(1, fd, __ptr, uVar4);     // read the payload
*(__ptr + iVar3 - 0x10) = 0;           // NUL-terminate
param_1[4] = __ptr; param_1[5] = iVar3 - 0xf;
FUN_100057d8(puVar5);                  // parse
FUN_10005b2c(puVar5, &DAT_1002a5f0, DAT_1002be8c);   // required-token check
FUN_10005a24(param_1 + 7, puVar5, &DAT_1002a5f0, uVar1);
```

The payload is a NUL-separated stream of **name\0value\0** pairs, optionally prefixed `!`
(`FUN_100058d8`: `sVar4 = strlen(p)`, then requires `off + sVar4 + 1 < len` before reading the value at
`p + sVar4 + 1`). Each pair is `strcmp`ed against a compiled entry in `FUN_10005c38` — entry layout is
0x10c bytes, name at +0, value at +0x81, flag word at +0x104, optional callback at +0x108. A match sets
bit 0 of the entry flags; `FUN_10005b2c` returns **10** ("attempted to upgrade to an incompatible
version") when a *critical* entry (`flags & 2`) is still unset and the package does not carry it.

**Tokens are compared in place.** Nothing copies a token into a fixed buffer, so a 1 KB token cannot
overflow anything: it simply fails `strcmp` against the 129-byte compiled name and, if that entry is
critical, produces error 10. The only fixed buffer on this path is the 0x400 `snprintf` in the *type-1*
handler, which is bounded.

**The 512 KiB is not in this binary.** `0x80000` never appears as an instruction immediate (no `lis rX, 8`
anywhere; the only 32-bit occurrences are misaligned words inside data tables), and the type-21 handler
mallocs exactly the declared length with no cap. So the 512 KiB abort is a *caller/reassembly* limit, not a
parser bound — the next read is the record reader on the anacapad side, not this handler.

## memcpy/memmove length sweep, 86.10 anacapad (2026-10-25)

Method: every `jal` to `memcpy@mips16plt` (`0x6db7d0`) or `memmove`, then keep only sites where the length
(`a2`) is loaded from a struct **and** the source (`a1`) is loaded from the same base — the `{ptr,len}`
packet-buffer idiom. 12 pairs; the five most packet-shaped:

| site | length | guard |
|---|---|---|
| `0x5d7ffa` | `80(sp)` | **guarded** — `sltu` at `0x5d7fb0` compares `len + *(a0+16)` against capacity `*(a0+20)`, bails to `0x5d7fa0` (returns 27) |
| `0x5bc904` | `*(u32*)(s0+4)` | none; destination is `alloc(len+12)` built at `0x5bc8da`–`0x5bc8ea` |
| `0x5a7c94` | `56(sp)` | none; destination is `alloc(len+20)` (`0x5a7c78`–`0x5a7c82`) |
| `0x4f9ae6` | `96(sp)` | **none, and no allocator in 40 instructions** — destination provenance unproven |
| `0x647ba8` | `*(s0+16) - *(s0+12)` (`subu` at `0x647ba6`) | none between the difference and the destination |

Checked sites for contrast: `0x4f3924` (`sltu` + `subu`, copies `min(len,255)` into a 256-byte stack buffer
memset at `0x4f390c`), plus `0x5b7ace`, `0x5c2414`, `0x6562b8` which each carry an `sltu` before the call.

**Net:** no site copies a raw packet length into a *fixed-size* destination without a guard; the unchecked
ones are allocator-sized from the same length. Residual risk sits in `0x4f9ae6` (destination unproven) and
`0x647ba8` (length is a field difference).

### Follow-up: there is no 512 KiB cap anywhere in `upgrade` (2026-10-25)

Chased the open question from the type-21 read. Four places checked, none caps:

- `FUN_10006c2c` (record header, 16 bytes) validates only the magic `0x35167f49` and that the type word is
  `0x11` or `0xffffffff` — **no length check**.
- `FUN_10007fa0` (record loop) checks only `local_498 < 0x10` (minimum), never a maximum, and never compares
  the record length against the remaining section length.
- `FUN_1000760c` (type 21) `malloc(len - 0xf)` — allocation follows the declared length, so a large record is
  fine as long as memory allows.
- The fetch is `execl("/usr/bin/wget", "wget", "-qC", "-T", "300", "-O", "-", <url>)` at `FUN_10008f54`
  (strings `0x10018804` `-qC`, `0x10018808` `-T`, `0x1001880c` `300`, `0x10018810` `-O`, `0x10017b88` `-`) —
  no size limit, no `--max-filesize`.

So the 512 KiB abort is **not in this binary's parser**: it has to be the transport (wget/server) or the
caller that feeds the stream. Combined with the type-21 finding (tokens compared in place, no copy), the
"1 KB token" is not what makes a large record fail — the record size alone does.

### Live confirmation: an over-long record desynchronises and aborts the fetch (2026-10-25, .202)

Built `/tmp/overrun-t21.upd` from the genuine package by taking the type-21 record at file offset **0x1c**
(header `49 7f 16 35 | 15 00 00 00 | 54 00 00 00`, i.e. magic, type 21, length 84 — all little-endian on
disk) and raising its length field to 4180 (+4096) **without adding bytes**, so the reader consumes 4 KiB of
the following records.

Result: the device accepted `BeginSoftwareUpdate` (200), read **131 072 bytes and then the connection died**
(`ABORT BrokenPipeError served=131072`), the daemon restarted twice (~8 s and ~5 s) and came back on
57.23-74170 with the version unchanged. Control runs of the same package unmodified deliver all 5 606 749
bytes, so 131 072 is not a fetch cap — it is how much had been buffered when the parser bailed and stopped
reading.

That confirms the mechanism predicted above: the declared length is trusted, nothing compares it against the
bytes present, the stream desynchronises, the magic check fails, the reader stops, and the transfer dies at
the socket-buffer boundary. **The abort point is the parser's first error, not 512 KiB.**

## The 1112-byte sink is the korn event dispatcher (57.23 anacapad, 2026-10-25)

`FUN_1021fb54` (called from the init path at `0x1021fb60`) allocates 0x14f8 bytes, zeroes it, stores it in
the singleton `DAT_10545d78`, and installs `FUN_10219cf8` at `ctx+0x14` — the sink. `FUN_10219cf8` is a
type dispatcher (`*param_2` = event type, 0x14…0x7d) whose assert strings name it: `sp_korn_event_count(korn)
< SP_MAX_EVENTS` (0x2f = 47) in `0x102232d0`, source path `esdk/src/korn/korn.c`. So the records are **korn
events**, and the queue is capped at 47.

**The one type carrying a caller blob is 0x1a** (metadata changed): `0x1021a370` re-dispatches to
`0x1021ae40` → `0x102232d0`, which `memset`s two blobs (`obj+0x9cc`, len 0x48c; `obj+0xe58`, len 0x44c) and
then calls `FUN_102402e8("", obj+0xe58)`; that takes the mutex `FUN_1021f408` and invokes the registered
consumer `(*DAT_1054f5b8)(str, dest, DAT_1054f5c0)`. The blob is **forwarded, not copied into the object**.
Type 0x4b is the only fixed copy: `memcpy(obj+0x140d, obj+0x2be, 0x21)` plus `strlcpy(obj+0x142e, …, 0x80)`.

**Reachability from :1400: indirect only.** The sink is the korn event queue; nothing in the HTTP/SOAP
handlers calls `ctx+0x14` directly (293 `lwz …,20(rX)`+`bctrl` sites exist, all vtable-generic). :1400
actions reach it by driving the playback state machine that emits the events — so a request can enqueue, but
cannot address the sink itself.

## :1443 JSON binding — the copy length is the truncated u16 (2026-10-26)

`bindStdString` (`0x5f1621`) and `bindStdStringArrayAndSize` (`0x5f15e9`) both funnel into
`FUN_005f13c0`, whose **length parameter is declared `undefined2`** — the caller's `uint` is truncated at the
call boundary. `FUN_005f13c0` builds a 36-byte descriptor via `FUN_005f1890`, which stores the length as
`*(undefined2 *)(d + 0x18) = param_4`.

The descriptor layout has **no other length field**: `+0` vtable, `+4`/`+8` the same `param_3`, `+0xc`
`param_8`, `+0x10` `param_9`, `+0x14` `param_7`, `+0x18` **u16 length**, `+0x1a`/`+0x1b` `param_5`, `+0x1c`
`param_2`, `+0x20` `param_6`, `+0x21` 0. Its destructor (`0x5f16e4`) frees 36 bytes, confirming the size.

So the copy uses the **truncated u16 at `d+0x18`**; the original length never reaches the descriptor. A JSON
string over 65535 bytes is therefore copied at most 65535 bytes — truncation, not an overrun, unless a
consumer re-reads the source length from somewhere else (none does: the descriptor is the only carrier).

### Consumer side: the u16 is used as both the copy length and the bound (2026-10-26)

Follow-up to the truncation finding. The descriptor's length field is read as a **halfword** and used
directly:

- `0x5f1af2` — `lhu a3,24(a0)` then `jal 0x5f59c8` with that value as the length;
- `0x5f1b02` — `lhu v0,24(s0)` then `sltu a3,v0` against the value returned by `0x5f1908`, i.e. the *same*
  truncated u16 is the bound the copy is checked against;
- `0x5f1690` — the descriptor copy-constructor copies the field with `lhu`/`sh`, so the truncation propagates
  through every copy.

So a >65535-byte JSON string is bound-checked and copied against 65535: the tail is silently dropped, and the
check can never fire for the real length because both sides are the same truncated value.

## /api on :1443 — the route is matched, the 400 comes from an unfilled request struct (2026-10-26)

`/api` is a route in the status/diagnostic name blob at `0x667394` (between `/tmp/wifi_card_mac_addr` and
`/cloud`). The matcher compares it with `strcmp(path,"/api")` at `0x47abba` and returns 1 on a match, so the
handler is reached — which is why both ports answer 400 rather than 404.

The handler is `0x47ab54`: it builds a JSON binding descriptor (`a0=sp+40`, `a1=sp+116`, `a2=request`,
`a3="/api"` at `0x47ab9a` → `0x4c3b48`), runs `0x4c43e0`, and then **returns -2 unconditionally**
(`li v0,2; neg v0` at `0x47aba6`).

The 400 *text* has exactly one reference in the whole binary — the pool word at `0x5f8080` used by
`muse::buildApiRequest` (`0x5f7e98`), which logs it and returns an empty request when
**`param_4 == 0 || param_2 == -1`**: a null command path or protocol version -1.

So the accepted shape is **a JSON body that fills the request struct** — the route alone is never enough, and
a query string cannot help because the handler binds the body. An empty body leaves `param_4`/`param_2` at
their defaults, `buildApiRequest` takes the 400 branch, and the handler's -2 becomes the 400 with an empty
body and `Connection: close`.

## Muse websocket handshake: what the server actually validates (2026-10-26)

`lechmere.N` alone is not enough because the validator checks **eight header-presence flags**, not one.
`RMuseHeaderHandler::checkIfValid` (`0x4e8318`) memsets a 0x200000 buffer, sets the status word to **0x34**
(valid), then tests bit 1 of the byte at header-object offsets **0x1bd, 0xe5, 0x109, 0x12d, 0x151, 0x175,
0x271, 0x295**; the first clear flag sets the status to **0x48** and selects a message naming that header.
Two of the eight names are resolved: **`X-Sonos-Udn`** (`0x673fb0`) and **`Sec-WebSocket-Protocol`**
(`0x673fbc` / `0x6824e0`); the other six sit behind `.bss` bases not yet resolved.

`RMuseController::captureWebsocketCommandToken` (`0x4e7d44`) then requires, in order: status **== 0x34**,
`hasAuthToken` (`this+0x24f4`) non-zero, and a connection type in {0,1} via `isConnectionType` (type 2 →
status 0x4e). The token is split out of the header buffer at `this+0x1bb` by `RAuthSplitToken` with a 0x2000
cap.

`updateHeaderTargets` (`0x4e8b18`) adds one more gate: if the status is 0x34 **and** the length at
`this+0x24d8` exceeds 0x20, it sets 0x48 and logs `failed to read lechmere header: %d` (`0x674078`, reached
via base `0x67113c + 0x2f3c`).

So a 101 needs all eight headers present, an auth token, and connection type 0/1 — the protocol header and
the upgrade headers alone leave at least one flag clear and the status at 0x48, which is the 400.

### True entries and the exact handshake condition (2026-10-26)

The symbols are odd (`0x4e8d7d`, `0x4e7d45`); the real entries are the even addresses, each preceded by the
PIC prologue `li v0,33 / la v1,4efb60 / sll / addu / move gp` and then `save`:
**prepareWebsocketHeader = `0x4e8d7c`** (`save 40,ra,s0-s1` at `0x4e8d8c`),
**captureWebsocketCommandToken = `0x4e7d44`** (`save 136,ra,s0-s1` at `0x4e7d52`). Decoding from the odd
symbol address is what produced the misaligned read.

`prepareWebsocketHeader` is a strict chain: `updateHeaderTargets(this,1)` → if 0,
`captureWebsocketCommandToken(this)` → if 0, `activateNamespaceFromHeader(this,1)`. Its dispatcher
(`0x4e8dc4`) only calls it when the connection-kind tag matches (`*(a1) == *(a0+9496)`), `*(a1+8) == 1`,
`*(a3) == 1` and `*(a1+48) == 0`.

`captureWebsocketCommandToken` then requires, in this order:

1. `0x4e7d5c` `lw v0,10344(a0)` + `0x4e7d60` `cmpi v0,52` — **`*(u32 *)(this+0x2868)` must be exactly
   0x34**, else it jumps straight to the fail at `0x4e7e76`;
2. `this[0x24f4]` (`hasAuthToken`) must be non-zero;
3. `isConnectionType(this+0x18, 0, 2)` must return 0 — connection type 2 sets status **0x4e**, and a
   connection type outside {0,1} sets **0x4f**; only 0 and 1 proceed to `RAuthSplitToken` on the buffer at
   `this+0x1bb` with the 0x2000 cap.

Status 0x34 is produced **only** by `checkIfValid` when all eight header flags are set; any missing flag
leaves 0x48. So the 400 is not a missing subprotocol and not a missing UDN alone — it is the status word
never reaching 0x34, and the first of the three conditions above is the one to satisfy.

## S1 57.23 URL-span overflow at 0x1007b2c4 — exact overwrite map (2026-10-26)

Function entry `0x1007ad60`, frame `stwu 1,-5344(1)`. The copy is **`memcpy(sp+16, r18, *(u32 *)(sp+5244))`**
(`0x1007b2c4`; `0x10546ad8` is the `memcpy` PLT slot), followed by a forced NUL at `sp+16+len`
(`stb 30,16(9)` at `0x1007b2d8`). The length at `sp+5244` is a parsed span length, computed as
`ptr - base + 1` at `0x1007ae60`–`0x1007ae70`; the earlier copy into `sp+3136` is bounded (`li 5,1025`), this
one is not.

Destination `dst = sp+16`, so the saved slots sit at these **byte offsets from dst** (all big-endian, written
straight from URL bytes):

| offset | slot |
|---|---|
| 5252 | CR |
| 5256 / 5260 / 5264 / 5268 / 5272 / 5276 / 5280 | r14 / r15 / r16 / r17 / r18 / r19 / r20 |
| 5284 / 5288 / 5292 / 5296 / 5300 / 5304 / 5308 / 5312 / 5316 / 5320 / 5324 | r21 / r22 / r23 / r24 / r25 / r26 / r27 / r28 / r29 / r30 / r31 |
| **5332** | **LR** (4 bytes, 5332–5335) |

The frame top is `dst+5328` (`sp+5344`), so the LR slot is 8 bytes *past* the frame — it is the caller's save
area. Consequences by N (the URL length):

- N ≤ 5251 — clean;
- N ≥ 5252 — CR and then saved registers are overwritten (matches the live 5300 → 408/000);
- N ≥ 5332 — LR partially overwritten; **N ≥ 5336 — LR fully attacker-chosen**, and the forced NUL lands at
  `dst+N` beyond it.

Attacker control is 1:1: URL byte *i* lands at `dst+i`, so the return address is simply URL bytes 5332–5335
in big-endian order; the only byte not under control is the terminator at `dst+N`.

## Does SetZoneAttributes/DesiredIcon reach the splitter 0x1007ad60? **No** (2026-10-26)

Static: the splitter has exactly three callers — `0x101a06b4` (in `0x101a05b4`), `0x101a0c9c`, and
`0x101cc704` (in `0x101cc6c0`) — and all three are generic URL-part helpers (`0x101cc6c0` calls
`0x1007867c` and then the splitter with two stack out-pointers; `0x101a05b4` builds a `'/'`-terminated
string first). None of them is reachable from the DeviceProperties action descriptor: `SetZoneAttributes`
lives at `0x104eb968` and is referenced only from the descriptor table at `0x104fc100`, whose handler
pointers (`0x103992d0` … `0x1039972c`) are ctor/dtor pairs that never call the splitter.

Live confirmation on `.202`: `SetZoneAttributes` with `DesiredIcon` of 1 000, 5 300 and 6 000 characters
answers **200 in 0.03–0.12 s with the daemon alive** each time (the splitter's signature is a reset at
≥4000), and `GetZoneAttributes` returns a `CurrentIcon` of exactly **128 characters** — the value is
truncated into a 128-byte field and stored, never stack-copied.

So the SOAP route is dead for the overflow; the URL has to arrive through one of the three splitter callers.

## What feeds the 0x101cc878 copy: a saved-queue record name from `<dir>/abstract` (2026-10-26)

`0x101cc878` (frame 2160, the 2100-byte buffer) is called from the result-set iterator `FUN_101a016c`:

```
FUN_1019fe30(param_1)                       // open "%s/abstract" for reading
while (FUN_1019ff94(param_1)) {             // next entry
  FUN_101a000c(param_1, auStack_420, 0x401) // build the entry string into a 1025-byte buffer
  FUN_101cc878(param_2, auStack_420, ...)   // the copy under test
}
```

`FUN_1019fe30` `snprintf`s `"%s/abstract"` (format at `0x1044eff4`-adjacent, `DAT_10545c40` is the directory)
and `fopen`s it; `FUN_101a000c` `fread`s an 8-byte record, writes the literal `x-file-cifs://` prefix into the
buffer and appends the record's name.

So the value is **a saved-queue entry name read from the device's `abstract` file** — not the Browse
ObjectID, which is why a 2139-byte ObjectID gives a clean 500 instead of the reset. It is settable from the
wire only by *creating* a saved queue whose title is that long; a long ObjectID cannot reach it.

Two consequences for a trigger: the name is truncated at 1024 first (`0x401` buffer at `FUN_101a000c`), and
the copy is only reached when the abstract file has an entry.
