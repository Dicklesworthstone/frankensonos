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
