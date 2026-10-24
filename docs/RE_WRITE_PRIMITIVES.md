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
