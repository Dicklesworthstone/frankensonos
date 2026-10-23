# Firmware paths per device class (owner-directed, 2026-10-08)

The owner's goal: his own software on his own speakers, so the system works
the way he wants. This file maps what the RE lane has established about
getting there, per device class, with the concrete next action for each.
Two rules hold regardless: **flashing or modifying a device is human-led**
(explicit per-operation approval, `docs/SCOPE.md`), and nothing proprietary
(images, keys, decompiled code) is committed — analysis lives as prose here
and in `docs/PROTOCOL.md` §9.

## Class A: S1-gen MIPS players (Play:5 Gen1, Play:1, Bridges) — OPEN BOOK

State (2026-10-20, static RE): milestone-era images (34.16) are unencrypted
and fully decoded; rootfs extracted and readable on both platforms (Play:5
Gen1 model 5, Play:1 model 12). The on-device updater signature-checks
images against keys compiled into `libsonoscrypto.so.1` and carries a vendor
debug path (`allow_policy_bypass`, debug-version flags, no-op mode).

The devmode gate is **cryptographic** and now fully mapped: three embedded
2048-bit RSA public keys sit in `libsonoscrypto.so.1` (moduli prefixes
`00ed54d2…`, `00b80399…`, `00e2601d…`; full values kept local-only). `POST
/devmode` base64-decodes a ≤1024-byte `statement` and verifies it as an
RSA-PKCS1v15 signature against one of the three keys (numbered, revocable).
On success the daemon writes `/tmp/device_unlocked_flag`; the updater's
`allow_policy_bypass` `stat()`s it. Forging a statement therefore needs the
matching Sonos **private** key.

**Private-key hunt (2026-10-20):** neither S1-gen controller app carries the
private key. The still-served 47.2 (v9.3) and 90.0 (v17.2.4) macOS DMGs were
downloaded and the full `Sonos.app` binaries scanned: the only "RSA PRIVATE
KEY" strings are mbedtls's PEM parser *label* constants (clustered, no key
data), `developerKey` is a SOAP *parameter name*, and the three moduli do
not appear in the controller. So the devmode signing key is Sonos-issued
(server-side / developer-program), not shipped in the controller. The 90.0
build does carry `SCMockUpdateDebugPage`, but the whole `SCMock*Page` family
is the SwiftUI design-system preview set (mock UI, not a functional update
backdoor). The controller-side levers that DO matter: `BeginSoftwareUpdate
(UpdateURL)` takes the URL as an argument and the controller logs `Override
AnonymousUpdateURL: %s` (the fetch can be redirected), and the update
**manifest** (`.upm`) is signature-verified against a root-CA bundle
(`SCRootCerts` / `RootCACertBundleDownloader`).

**Live update-path probe (2026-10-22, Play:5 Gen1 on 57.23-74170, reads and
two reboots only):** the device's own fetcher appends
`?cmaj=&cmin=&cbld=&subm=&rev=&reg=&serial=&sonosid=&householdid=` and
`User-Agent: Wget` to whatever `UpdateURL` it is given. Behaviour, measured
with a byte-counting HTTP server and a packet capture:

- a **64 KB** file is fetched to completion, so the server side is sound;
- a 2 MB `.upd` of an **older** version is read for only ~10 KB and then the
  connection is FIN/RST-closed — the version gate runs on the first records,
  not after the whole download;
- the **same** version aborts the same way (nothing to do);
- the same image with the type-1 version record patched upward
  (57.23 → 99.0) also aborts at ~10 KB, so the early gate is not satisfied by
  the URL or the filename: the record is parsed and the (signed) header is
  what decides;
- a bare `.upm` manifest is fetched completely but produces **no** image
  fetch (the manifest is root-CA-signed; ours is not).

Consequence: `BeginSoftwareUpdate` alone cannot install a modified or older
image without the signing key, and "Force" (Flags bit 0) only makes the
device go as far as the gate. An in-band downgrade is therefore *not* a
bypass by itself.

**Reachable unauthenticated write (2026-10-22, verified live):** `POST
/advconfig` writes `/jffs/netsettings_mod.txt` as root from two form fields
(`FirstZP`, `PriorityBridge`, each `atoi`-reduced). A CSRF token is required
and must be the **first** parameter (read from the GET form, single use);
the pairing is `csrfToken=<tok>&FirstZP=1&PriorityBridge=0&` → `200 Success.`.
The file only carries two integers, so it is a persistent config write, not
code execution.

**Diagnostics on 57.23:** `/diag` (the 34.16 module) is gone; `/status` is
the live surface, lists its own options (`zl`, `enetports`, `VERSION`,
`proc/...`, `ifconfig`, `showstp`) and runs the fixed command table as root
(`/usr/sbin/brctl showstp br0`, `/sbin/ifconfig`, …), while `/status/<file>`
returns fixed paths. Path traversal and arbitrary-path reads are refused.

**SOAP robustness (2026-10-22, live sweep):** every action's string arguments
were driven to 1850 bytes, bodies to 38 KB and parameter counts to 4800; the
daemon never crashed, and the byte-wise `RCROutParam::accumulate` and the
`RBase64Dec` decoder both clamp to their declared capacities. The obvious
length bugs are not there.

**VERIFIED 2026-10-08**: the .upd delivery path is **blocked** by RSA
signature verification on running 57.23 firmware. Live-fire testing proved:
`BeginSoftwareUpdate` accepts arbitrary URLs and the speaker downloads
served files, but modified .upd files (with real kernel+rootfs from the
readable 34.16 base) are rejected — script records never execute, version
stays unchanged, the speaker recovers. The 34.16-era binary showed the
signature check as advisory; 57.23 enforces it. The gap between 34.16 and
57.23 is where Sonos closed it.
(boundary pinned between 34.16 and 57.19). The readable milestone base gives
the platform's layout; decryption needs the device-family key from a device's
install environment (the key is on-device, compiled into the install path).

Path to owner software (S1), in order of tractability:

1. **Downgrade to a pre-enforcement build** (34.16, where the signature
   check is advisory) and install owner software there. The readable 34.16
   base already gives the platform layout. Open questions: how to get a
   57.23 speaker back to 34.16, and whether the 34.16 updater's advisory
   check is actually a hard no-op or just non-fatal.
2. **Devmode bypass** — obtain a legitimately Sonos-signed devmode
   statement (developer-program credential) for the device, `POST /devmode`
   it, and the updater's `allow_policy_bypass` then accepts a modified
   `.upd` (signature skipped / no-op mode).
3. **Forge the statement** — needs the Sonos private key for one of the
   three embedded moduli. Not in the controller; would have to come from a
   Sonos service or a leaked key.

The S1 `.upd` container (34.16, fully decoded) is a record sequence
`[magic=0x35167F49 LE][type u32][total_len u32 (includes the 16-byte
header)][reserved u32][payload]`: type 1 = version header, 2 = 28-byte
sub-record, 3 = shell install script, 4 = CramFS rootfs (custom Sonos
variant, not stock cramfs), 5 = 1-byte marker, 6 = gzip MIPS kernel,
9/10 = 512-byte records, trailer = type `0xFFFFFFFF`. No signature record
in the 34.16 image — the check runs against the on-device keys.

## Class C: Sonos One / modern S2 (Amlogic A113x ARM) — PUBLISHED EXPLOIT ROUTE

The published route (blasty, Synacktiv, NCC Group BH-US 2024 — all public
research, cited in the local runbook archive):

1. **No USB path**: the One's bootrom USB recovery is fused off.
2. **Physical access + PCIe DMA** (Synacktiv method, no flash writes, low
   brick risk): USB3380-class FPGA board on the mini-PCIe WiFi card slot →
   patch `poweroff_cmd`/`vfs_read` in kernel memory → telnet root shell.
3. **EL3 exploit** (blasty's A113x BL31 secure-storage parser overflow) via a
   small kernel module → arbitrary EL3 read/write → dump OTP + bootrom.
4. **Offline**: `sonostool` derives the per-model OTA AES keys and model RSA
   key from the MDP blob + OTP → decrypts any S2 firmware image; one dumped
   device opens fleet-wide update decryption for that model. LUKS keys are
   per-device (CPUID-derived).
5. **S2 `.upd` layout (verified 2026-10-20 against the 86.10 Play:1 and
   97.1 Sonos-One images, both downloaded + walked):** same `0x35167F49`
   record container as S1, but the content is wrapped in encrypted blobs.
   Record types observed: 1 = version header, 2 = sub-records, 21 =
   patch-train list (`2021_11_PATCHES:` …), 22 = certificate bundle (Sonos
   root CAs), 23 = **plain tar overlay** of the rootfs (`bin/anacapad` —
   the ~3 MB zone daemon), and types 3/4/6 (install script / rootfs /
   kernel) as `0x886499ca` encrypted-content blobs. Key/signature blobs
   (types 22/17) use a `0x621da74d` header: an RSA-2048-OAEP-wrapped
   per-blob AES-128-CBC key at `[0x32:0x132]`, the AES-CBC body (IV = 16
   zero bytes) at `0x13A`. Type 17 is the 380-byte `.upd` signature blob.
   `sonostool` decrypts each blob with the model private key (from the MDP
   at offset `0x780`) → AES-CBC → the file.

Costs/risks: ~1-hour physical session with FPGA hardware; firmware-specific
EL3 offsets must be re-derived per build (documented in the runbook);
CVE-2023-50809/50810 patches touched the U-Boot env path but not the BL31
parser per current sources. Full step-by-step runbook with tooling URLs is
archived locally (git-ignored captures/research/) since it's operational
detail, not repo prose.

## Recommendation

Reliability goal is already served by the daemon (stock speakers, no risk).
Firmware replacement research proceeds S1-first (open book, zero hardware
risk) with the Class C physical session as a deliberate, scheduled,
human-in-the-loop event when the owner wants the S2 One opened.

## Network-only root lane — state after the 2026-10-22 sweep

Owner-directed condition: no developer-program key, no manual downgrade
flash, no hardware disassembly; software disassembly is encouraged. The
authorized lane is therefore "find a pre-auth bug in the daemon the players
actually run". This section records what is now *proven*, so the next
session does not re-walk it. Exploit-workspace detail lives in the
git-ignored `captures/research/rs-20261022-s1root/` (state file,
`deliverables/SESSION-STATE.md`) and the Ghidra project under
`captures/research/ghidra/`.

### Architecture: `anacapad` is a front proxy, and the service layer has no strings

`anacapad` is the Anacapa HTTP server (conf `Port 1400`, `SSLPort 1443`) and
is launched by `/opt/bin/anacapactl` as `/opt/bin/anacapad -c
/opt/conf/anacapa.conf`. It contains the `R*Server` wrapper classes
(`RDPServer`, `RGMServer`, `RSPServer`, `RZGTServer`) plus
`RCRInParam`/`RCRResultParser`, i.e. an in-process control-RPC layer.

It does **not** contain the literals `MediaRenderer`, `SetAVTransportURI`,
the `AVTransport` service URN, `RenderingControl`, `Browse` or `Search` — in
either the 34.16 or the 86.10 build — yet `/MediaRenderer/AVTransport/Control`
answers 200 on live players and `/getrs` needs a numeric setting index. The
UPnP service/action names are assembled and dispatched without literal
strings. Consequence: string/xref sweeps on `anacapad` alone miss the
AVT/CD/queue layer; work from the descriptor/RPC layer instead.

### Tooling that works

The 86.10 daemon is **MIPS16** (mixed with MIPS32). `r2`/`rabin2` cannot
decode it — `aaa`, `/r`, `pdf` all return `invalid`, and there are no 32-bit
immediates by construction. Use:

- `mips-linux-gnu-objdump -d -m mips:32` for raw disassembly (decodes
  MIPS16 correctly),
- **Ghidra 12 headless** (`analyzeHeadless`, import with
  `-processor MIPS:BE:32:default`) for decompilation; the scripts in
  `captures/research/ghidra/scripts/` do a whole-binary copy-site sweep and
  a targeted decompile, and the 3 MB binary analyzes in ~2 minutes.
- Strings of interest sit in `.text` **literal pools** (`lw aN,<pc-literal>`),
  not in `.data`; a `.data`-only grep finds nothing.

The 34.16 binary is plain MIPS32 with a dynamic symbol table and remains the
better map of the *legacy* source shape, but its addresses do not transfer.

### What is measured hardened (do not re-spend device time)

- HTTP request line, header chunking, `Content-Length` consumers, URI path
  (with `..`/`\\` depth check): every copy has an instruction-level bound.
- SOAP/XML args: per-field `strlcpy` with literal pitch, width-limited
  `sscanf`, `__memcpy_chk` 1023→1024; `RupnpParseNextCsvValue` and
  `RparseUrlEncodedValue` gate on capacity before each store. Live: a DIDL
  `dc:title` of 60 KB through 2.5 MB, 400 KB of escaped DIDL, and a 4.5 MB
  20 000-item document are all accepted without a restart.
- `/devmode` and `/devunlock`: verify-then-store with exact-fit record loops;
  decoded statement/confirm paths are capacity-checked at each stage.
- Update path: the 8193-byte stream context, `readData` clamping, every
  `read16` destination check, response-header `strlcpy`s, redirect copies,
  URL building, `.upd` record loop, `.upm` shape attacks (4 KiB nesting,
  1 MiB tag names, entity bombs) all hold; recovery matches the baseline
  restart of ~5–9 s every time.
- MPEG-TS: the naive CVE-2023-27355 shape (every packet AFC=3,
  `afelen=0xFF`) plays and does not crash 57.23, so S1 parity is unproven.
- Command injection: one `execve` funnel (`ap_create_subprocess_tokenized`),
  fixed command table, no shell, URL pre-encoded. `/status/<file>` is a fixed
  absolute-path table. No SMB code in either extracted build.

### Update-metering facts

`BeginSoftwareUpdate(URL, Flags, ExtraOptions)` writes
`/var/run/upgradeinfo`; `ExtraOptions` supports `system_version=` with a
CSV tokenizer and is written verbatim, so a literal newline forges extra
`Key: [value]` lines. It is inert in practice: `bin/upgrade`'s reader is
last-wins and the writer templates every key afterwards.

The version gate aborts before `upgrade` starts (≈128 KB of the image read,
then FIN/RST) for: the installed version, older signed images, a
newer-but-wrong-model image, and an image whose type-1 version record was
patched upward. `Force` (Flags bit 0) does not override it. 57.23's
`MinCompatibleVersion` is the 56 line, so a signed 34.16/57.19 image cannot
be installed by URL alone.

### Local smart-speaker WebSocket API (port 1443) — pre-auth, and it has admin verbs

Newer builds also accept a local WebSocket API. Handshake: `wss://<ip>:1443/websocket/api`,
header `X-Sonos-Api-Key: 123e4567-e89b-12d3-a456-426655440000` (a published constant),
subprotocol `v1.api.smartspeaker.audio`. The server greets with an error envelope that
already carries `householdId` and `locationId`.

The request frame is a JSON **array of exactly two elements**: a command object
`{namespace, command, householdId, <params>}` and an options object that must be `{}`.
A one-element array, `[]` or `null` as the second element is rejected with
`Parsing terminated:[1]`; a header+command pair is rejected at `[0].command`.
`groups:1/getGroups` and `favorites:1/getFavorites` answer with typed payloads; live
group ids look like `RINCON_<mac>01400:<hash>` and are not the `locationId`.

The command table is compiled in as comma-joined descriptors
(`<required id>,<namespace>,<command>[,<param>]`), which enumerates the whole surface
with no wire probing. It includes, pre-auth:

- `playerId,management,factoryReset`, `playerId,management,reboot`,
  `playerId,networkTest,temporarilyDisableNetwork`
- `deviceId,householdUpdate,beginHouseholdSoftwareUpdate`,
  `playerId,update,beginSoftwareUpdate` / `checkForUpdate` / `getUpdateStatus`
- `householdId,settings,getProtectedAdminSettings` / `setProtectedAdminSettings`,
  `playerId,settings,updateAllSettings`, `setSonosNetChannel`, `setSelfTruePlay`
- `householdId,platformInternal,invalidateCache` / `sync`,
  `playerId,diagnostics,submitDiagnostics`
- `playerId,upnpDeviceProperties,call` and the matching `upnpSystemProperties` /
  `upnpZoneGroupTopology` subscribe/renew/unsubscribe/call families (a generic UPnP
  passthrough)

Whether an individual admin verb needs more than the static key is now measured:
the static key is effectively **read-only**. `POST .../update/check` is open and even
returns a fresh `updateItem` with `updateURL`/`manifestURL`; `management/reboot`,
`update/firmware`, `update/status` and the policy paths answer 403
`ERROR_NO_PERMISSION` with `WWW-Authenticate: Bearer …insufficient_scope`.
`Authorization: Bearer <api-key>` instead of the header answers 400
`ERROR_API_KEY_VALIDATION_FAILED`, so the Bearer channel is a separate token.
`POST .../authorization/authorizeDevice` wants `assertion` then `grantType`
(any grant type then answers 403 Unauthorized), and the JWT errors in the binary
(`INPUT_JWT_X5C_MISSING` / `_UNTRUSTED`) show the assertion must be an RS256 JWT
carrying an x5c chain trusted by the on-device root bundle — i.e. cloud-issued, not
self-signable.

Generic UPnP passthrough is real but partial. `POST /api/v1/players/{id}/upnpDeviceProperties`
and `.../upnpZoneGroupTopology` take `{"method":"<Action>","params":{…}}` and answer
`upnpResponse` with typed `parameters`; `GetZoneAttributes` returned the live zone name.
`upnpAVTransport` answers UPnP 402 (invalid args) for `GetTransportInfo`/`GetMediaInfo`
with or without `InstanceID`, so that one still needs its plumbing.
`update/check` and the passthrough are the two open doors the scoped verbs do not gate.

**The surface is version-gated per build** (measured, benign reads only): the S1
57.23 player answers `info` (with `apiVersion 1.18.10`, `minApiVersion 1.1.0`,
`websocketUrl`, `restUrl`) and `settings/player`, and nothing else — `settings`,
`diagnostics/metadata`, `update/check`, `devices/registration` and the `upnp*`
passthrough are 404 there. A 97.2 S13 answers `info`, `settings`, `favorites`/`groups`
(WS), `update/check` and the passthrough for DeviceProperties/ZoneGroupTopology. So the
newer verbs (and the whole scoped-token front) exist only on S2-era builds; on S1 the
local API is a read-only inventory/settings reader.

**The same command table is also REST over HTTPS 1443**: the namespace triples
above are path templates prefixed with `/api/v1`, e.g.
`GET /api/v1/players/{playerId}/info` and `GET .../settings` answer 200 JSON with
only the static key. Confirmed live: `info` (full device descriptor) and `settings`
(settings groups with schema versions) are readable pre-auth; `diagnostics/metadata`
answers 400 `ERROR_API_KEY_VALIDATION_FAILED`; `update/status` answers 403
`ERROR_NO_PERMISSION` with `WWW-Authenticate: Bearer …insufficient_scope`, so the
update verbs sit behind a scoped token. `POST .../authorization/authenticateClient`
accepts `{}` and returns `{}` (key check only), `POST .../authorization/tokens`
returns `authzTokenStatus: UNAUTHORIZED` with zeroed entitlements, and
`authorization/authorizeDevice` wants a JWT `assertion`. The household
`authorization/policy/{policyKey}` paths answer 403. So the open question narrows to
how a local client obtains a scoped token without the cloud.

### Open leads, in order

0. The local WebSocket API above is the widest new surface: a static key reaches
   `reboot`, `factoryReset`, the update triggers and a generic UPnP passthrough with no
   pairing. The immediate task is finishing the parameter plumbing for the
   `playerId`/`deviceId`/`userId` namespaces (the ids, not the key, are what the parser
   rejects today), then exercising the admin verbs and their parsers.
1. The 2023 Pwn2Own pair is the only public S1-shaped material: `/msprox`
   OOB read (CVE-2023-27353, leak) and the MPEG-TS `afelen` overflow
   (CVE-2023-27355). `/msprox` is **absent** from both extracted builds: no `msprox` bytes exist
   (case-insensitive, UTF-16 and split forms), `/api/anything` is a generic 400
   stub and `/api/v1/msprox` is 404. CVE-2023-27353 is therefore not reachable
   here; retire it.
   The TS demux also cannot be located by its constants: neither image contains
   a `li 0x47` or `li 0xbc` immediate, and the plain update overlay carries only
   `bin/anacapad`. Either the parser is struct-based inside anacapad or it lives
   in a component the delta images do not carry; the naive live trigger did not
   crash, so treat CVE-2023-27355 as unproven here.
   Payload primitives for the day a smash lands: 34.16 `execve` gadget
   `0x4f4178` (`a0=*s1`, `a1=argv`, `a2=environ`, needs `s1`), and 86.10
   `0x656a86` (`a1` alone drives both, path via `*a1`). NX is off on 34.16, on
   for 86.10. The TS demux in the control-RPC layer has not been
   located.
2. `/getrs?id=N` indexes a setting table with no bounds check and then
   `jalr`s through the entry; `N` is `atoi`, so negative and huge indices are
   attacker-chosen. `-1…10000` answer without a restart, so reachable
   entries are still empty or benign.
3. `POST /notify` is the only body reader without a total cap (no
   Transfer-Encoding parser at all; absent `Content-Length` reads to close).
   Survives 100 KB and 1 MB bodies, non-XML garbage, a 300 KB LastChange
   attribute, 2000-deep nesting and malformed percent escapes; the server
   closes early on the larger ones and stays up.
4. `/advconfig` builds a stack buffer of `n+0x10` but validates only `n`
   (n is a constant 0x1000; the saved `ra` sits in the slack).
5. Expat is statically linked at 2.2.0 (34.16) and 2.5.0 (86.10) and 2.5.0
   is still current in a 2026-09-10 build. Most post-2.5.0 CVEs are
   denial-of-service; the memory-corruption ones need external-entity
   sub-parsers (not used) or `XML_ParseBuffer` length misuse (only
   `XML_Parse` is used).
6. A signed devmode statement or a leaked Sonos key remains the cleanest
   *enabler* (the updater then accepts modified images); nothing here
   removes the signature check itself.
7. The 86.10 daemon embeds libcurl for its HTTP client. The curl advisories of
   the last quarter (`Set-Cookie` public-suffix scope, proxy-auth leakage,
   LDAP SASL downgrade, HTTP/2 stream-dependency UAF) are all logic or
   feature-gated, not response-parser memory corruption, so they do not give a
   local code-execution primitive against a malicious server. BusyBox carries
   no TLS here and SMB is absent from both MIPS builds, so the recent
   BusyBox-TLS and Era-300 SMB CVEs are out of scope.

### Safety notes for the next session

Use bench players first and expect a watchdog restart. Do not drive one unit
twice inside a minute. Announce state changes in Agent Mail (`am`) and leave
the house paused at low volume. The Mac's LAN interface has DHCP churn;
bind the source address explicitly when a device answers `EADDRNOTAVAIL`.

