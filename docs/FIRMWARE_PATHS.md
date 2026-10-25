# Firmware paths per device class (owner-directed, 2026-10-08)

The owner's goal: his own software on his own speakers, so the system works
the way he wants. This file maps what the RE lane has established about
getting there, per device class, with the concrete next action for each.
Two rules hold regardless: **flashing or modifying a device is human-led**
(explicit per-operation approval, `docs/SCOPE.md`), and nothing proprietary
(images, keys, decompiled code) is committed — analysis lives as prose here
and in `docs/PROTOCOL.md` §9.

## NCC whitepaper: what maps to our lines and what does not

Read of "Listen Up" (BHUSA 2024, NCC Group, 40 pp). Two distinct bugs, both
older than our firmware:

- **CVE-2023-50809** — Sonos One (MediaTek) WPA2 `WpaParseEapolKeyData`:
  unchecked IE length, integer underflow, `memmove` into a 32-byte GTK stack
  buffer, malicious-AP trigger, kernel RCE. Fixed in S2 15.9 / S1 11.12.
- **CVE-2023-50810** — **Era 100 U-Boot**, three issues chained: U-Boot still
  tries to load a stored environment despite `CONFIG_ENV_IS_NOWHERE` not being
  set, `sonosboot` ignoring `setenv("bootargs", …)`'s return value (so a
  read-only `.flags=bootargs:sr` keeps attacker `bootargs`), and a Sonos image
  header whose unchecked `kernel_offset` lets data be planted at a fixed
  address while the image signature still validates (`initrd=ADDR,SIZE`).
  Fixed in the same releases.

Neither applies to Play:1/Play:5 as shipped (Atheros Wi-Fi, older SoC, 57.23 /
86.10 are far past the fix). Two things are still worth keeping:

- the **stored-environment** trick is generic U-Boot behaviour — if our
  bootloaders share the misconfiguration, an env write gives a persistent
  `bootargs`/`initrd` override, i.e. the persistence half we want, but it needs
  a flash writer first (the updater writes fixed partitions only);
- `/jffs` is mounted **`noexec`** on Sonos; a `Configure.dev` works because
  `Configure` runs `sh <file>`, but any binary dropped in jffs needs
  `mount -o remount,exec /jffs` (the kernel hooks it via `sonos_allow_mount_exec`).

Class A: S1-gen MIPS players (Play:5 Gen1, Play:1, Bridges) — OPEN BOOK

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

**Correction (2026-10-23): the flag only buys anything on 34.16-era updaters.**
`strings` on the 34.16 updater gives both `allow_policy_bypass` and
`/tmp/device_unlocked_flag`; neither string exists in the 57.23 updater (only
`pthread_mutex_unlock`). So on the current S1 build the devmode flag does not by
itself make an unsigned/modified image install — the 57.23 updater is the newer
signature-only design, and a 57.23-parser bug (bead `nyy`) or Sonos's key is what
reaches code execution there. On older builds the flag path still exists.

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
bypass by itself. See
[`UPDATE_FEED_AND_SIGNATURE.md`](UPDATE_FEED_AND_SIGNATURE.md) for the
direct-package extension (2026-10-22): whole-`.upd` fetch, the encrypted
component envelope, the signature TLV layout, and why unsigned preinstall
does not run.

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
  `RparseUrlEncodedValue` gate on capacity before each store. `utf8strncpy`
  reserves its last byte for the terminator and its truncation path writes the
  replacement length exactly at `dst+cap-1`, so it does not creep into the next
  field. Live: a DIDL `dc:title` of 60 KB through 2.5 MB, 400 KB of escaped
  DIDL, 2k-50k sibling properties, 20k `<res>` elements and a 4.5 MB
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
self-signable. The endpoints, mapped on 86.10: the grant is
`POST /api/v1/players/{id}/authorization/authorizeDevice` (`assertion` then
`grantType`), the *status* read is `POST /api/v1/households/{householdControlId}/
authorization/tokens` (`authzTokenStatus: UNAUTHORIZED`, zeroed entitlements —
`grantType`, `clientId`, `code` and `scope` bodies all leave it UNAUTHORIZED), and
`POST .../players/{id}/authorization/authenticateClient` answers `{}` with
`X-Sonos-Type: none`. All four cheap forgeries — `alg:none` with and without
`x5c`, HS256 with a guessed key, and an RS256 JWT carrying a freshly minted
self-signed x5c — come back as the same blanket 403 `Unauthorized.`, so validation
detail is not observable over REST. **The on-device root bundle is the public CA set — 132 roots, no Sonos root.**
Extracted from the type-22 bundle of both the model-12 (86.10) and model-16
(57.23) packages: Amazon, GlobalSign, Google Trust Services, DigiCert, eMudhra,
AffirmTrust and the rest of the usual list. So chain trust is *not* the barrier
a Sonos root would be; any publicly-trusted leaf can sign the assertion and the
gate reduces to the claim schema.

Two things follow. First, `grantType` matters and
`urn:ietf:params:oauth:grant-type:jwt-bearer` is the right one — it changes the
failure from **403 `ERROR_NO_PERMISSION`** (permission pre-check) to **401
`ERROR_NOT_AUTHORIZED`** (real validation), while every other grant type stays
403 and an absent one is 400 `Parsing terminated:grantType`. Second, the
assertion can be genuinely minted: a real Let's Encrypt chain from this fleet
(ECDSA P-256, so **ES256** with the raw `r||s` JWS form, not RS256/DER) plus a
4-cert `x5c` produces a token the daemon actually validates and rejects on
content, not on chain. `.`/`alg:none`/HS256/self-signed all stay 401 as well, so
the response code alone no longer separates "untrusted" from "wrong claims" —
the claim schema has to come from the parser
(`FUN_00603aac` is the payload reader, errors map through `FUN_00600f08`'s
`INPUT_JWT_*` table). Local minting stays open only on that schema.

**The WS frame shape is `[header, payload]`, and that was the missing piece.**
A command is a two-element JSON array: `{"namespace","command","householdId"}`
then the payload object — putting the payload *inside* the header answers
`Parsing terminated:[1]` forever, and a bare second element gets
`Parsing terminated:[0].command`. The `authorization`/`resolveToken` config
names its payload fields in `RserializeResolveTokenConfigAsJson`, read from the
string table as `token`, `objectId`, `objectType` and an `attributes` object; a
complete frame answers `authzTokenStatus UNAUTHORIZED / UNKNOWN` pre-auth (static
key only) for every token tried, up to 100 KB, including a `h.<100 KB>.s` JWT
shape, so the resolver takes the string and shows no length cliff. The `upnp*`
passthrough is *not* reachable this way: with the correct frame it answers
`ERROR_UNSUPPORTED_NAMESPACE: v1:players namespace is not supported`, so REST
stays the only working passthrough and its argument plumbing is still the
blocker.

**Registration is not wired on 86.10.** `devices/completeDeviceRegistration`
(with the bare `deviceId`, not the RINCON form — the RINCON form is rejected as
`Parsing terminated: deviceId`) answers `ERROR_UNSUPPORTED_COMMAND: command is
not supported`, so the command exists in the descriptor table but has no handler
on this build. Every payload shape tried (`{}`, a `deviceCert` string, a
`deviceCert` object) gets the same answer. The token door on 86.10 is therefore
just `resolveToken`, which never succeeds without a cloud-minted JWT.

**WS connect exhaustion, and it escalates to a restart.** A bare TLS+WS
handshake is enough — no frame, no valid key value, just the static-key header.
At 0.3 s spacing 9 of 12 attempts timed out from the second one on; at 1 s, 9 of
12; at 2 s, 8 of 10, so spacing barely helps. HTTP stays up through the stall,
then the daemon dies outright (HTTP 000) after roughly 25 attempts and the WS
endpoint is still dead 60 s after HTTP comes back. Recovery is a watchdog
restart, so this is the third cheap pre-auth kill on 86.10 alongside /setstring
and HLS, and the cheapest (connect-and-drop). It also explains every "the API
timed out" seen earlier in the session. Bead `re-ws-exhaust`. **It is S2-only:**
20 connects at the same 0.3 s spacing against an S1 unit (57.23) all succeed and
the daemon never blinks.

Generic UPnP passthrough is real but partial. `POST /api/v1/players/{id}/upnpDeviceProperties`
and `.../upnpZoneGroupTopology` take `{"method":"<Action>","params":{…}}` and answer
`upnpResponse` with typed `parameters`; `GetZoneAttributes` returned the live zone name.
`upnpAVTransport` answers UPnP 402 (invalid args) for `GetTransportInfo`/`GetMediaInfo`
with or without `InstanceID`, so that one still needs its plumbing.
`update/check` and the passthrough are the two open doors the scoped verbs do not gate.
Argument plumbing, measured on 86.10: both `params:{…}` and
`input:[{name,value}]` parse (the latter is stricter — a non-array `input`
answers 400 `Parsing terminated:input.name`), but **no argument reaches SOAP**:
`SetZoneAttributes` answers UPnP 402 in all four shapes tried while the
zero-argument `GetZoneAttributes` returns live data. The passthrough is
therefore read-only in practice. Admin verbs on 86.10 with the static key:
`management/reboot` → 401 `invalid_token` (scoped), `settings/updateAllSettings`
and `settings/setProtectedAdminSettings` are **GETs** (405 as POST) but the
player path rejects them — 404 `unknown settings group name` — so they hang off
another namespace; `settings/player` is open, the `settings` list is 401, and
the household id for the household namespace is the `HouseholdControlID`
(`Sonos_…`) from `/status/zp`, not `locationId` (that one answers `Household ID
verification failed`).

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

0. **The only remaining gate on an unsigned image is the signature check, and the
   only way around it is a device flag file** — see "Update gate, measured on the
   running 57.23 build" below for the ordering, the two `allow_policy_bypass` call
   sites and the offline harness. Next concrete work, in order: (a) finish the MDP so
   the harness runs the signature code path (mtdram + partition table, or stub the
   compat checker at source level); (b) re-test the *live* gate with a model-16 image
   whose version record and model are untouched and only a ciphertext byte is flipped,
   to confirm the live stop is the signature and not something else; (c) hunt any
   pre-auth writer of `/jffs/VERSION` or `/jffs/system/Configure.dev` now that the
   exact live daemon is readable; (d) exercise `settings/updateAllSettings`,
   `setProtectedAdminSettings` and `management/factoryReset` on an S2 unit — S2 needs
   no `/jffs` primitive because its `devunlock` path writes
   `/tmp/device_unlocked_flag` and runs `run_sshd.sh` directly.
1. The local WebSocket API above is the widest new surface: a static key reaches
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
   The TS demux is **in the live 57.23 daemon** (the 34.16/86.10 builds simply
   predate it): the PPC binary carries `hls-aac://`/`hls-radio://`, "read frame
   sync failed", `payload=%zu, pesleft=%d` and `peslen=%d`. The frame reader is
   `FUN_101ba764` (Ghidra PPC): 4-byte header into a 0x81c-stride ring slot,
   data capped at `local_c8 < 0x7fb`, slot data area 0x1e..0x819, header-parse
   failures log "read frame header failed", oversize logs "read frame data
   overflow". The ID3 peek copies up to 0x81 into a genuine 0x81-byte scratch
   (-424..-295) and re-seeds its own count, so it is deliberate. The naive
   `afelen=0xFF` trigger is not, by itself, the trigger.

   **HLS playback itself is a reliable remote kill.** The path decodes only over
   HLS (`hls-radio://` fetches the playlist and every segment; a bare `.ts` over
   `x-rincon-mp3radio` never decodes). Once a player is pointed at an
   attacker-served playlist, the daemon dies within 4-20 s — reproduced 10/10
   across mutating segments, a plain 8x2 s VOD playlist, 1/2/4-segment
   playlists, a live (no `ENDLIST`) playlist, `PLAYLIST-TYPE:VOD`,
   `MEDIA-SEQUENCE:5`, and with `EXT-X-DISCONTINUITY` (that one dies fastest, at
   4 s). Recovery is automatic (watchdog) and quick, so it reads as a restart,
   not a wedge. `AFC=3, afelen=0xFF` segments crash *sooner* inside that window,
   which is why the first samples looked value-dependent. Beads
   `re-afelen-live` / `re-hls-kill`. It is **intermittent, not deterministic**:
   identical `g2` replays killed 3 of 6 (down at 4 s, 12 s and 25 s) after an
   earlier window gave 10/10 across the variant matrix; segment count does not
   decide it, and a later window collapsed to **0/20** (payload-less packets,
   4x-length segment, one-segment loops, 12-20 segment lists all clean), then
   came back only partly after a `/setstring` restart (1/3), then went to zero
   again: 29 Play/Stop rounds with a GENA subscription live (events flowing,
   `ThirdPartyMediaServersX` included), 4 runs with segments truncated
   mid-transfer, and 3 identical runs on a second unit all stayed up. The crash
   needs a condition that is not yet identified; per-device state is suspected
   because the earlier kills only happened while this unit had recently churned. So it is
   state-dependent as well as flaky: the only honest claim is "HLS playback
   sometimes kills the daemon, 4-25 s fuse, worst right after a restart".
   Media *content* is not the variable — payload-less packets survive, and so
   does a 4x-length segment. **It is not S1-only:** the same playlist on
   86.10 (S2 Play:1) crashes 2/2 (8 s, 16 s), so the whole fleet is affected —
   the S2 daemon also fetches `hls-radio://`. Next: whether the fault is the
   segment-map builder (the `hls-%s said` / "creating HLS Segment Map entry"
   path) or the fetch/decode tail, and whether the crash point is steerable by
   playlist fields.

   Static shape of the suspect (PPC, `FUN_10190584` = media-playlist ingest): the
   segment queue is a 16-slot ring of 0xc50-byte entries based at `obj+8`
   (`FUN_101913b0` init: capacity 16 at `+0xc514`, array pointer at `+0xc528`);
   the counters `head/count/tail/limit` sit at `+0xc508/0xc50c/0xc510/0xc514`,
   i.e. exactly where entry **16** would begin. `FUN_10192674` compacts with
   `new_head = 16 - (limit - head)` and `memcpy(new_head*0xc50+8, head*0xc50+8,
   (limit-head)*0xc50)` — no check that `limit-head <= 16`, and `FUN_1019278c`
   (the "track list is split" reset) sets `head = limit = 16`. Head advances with
   a real modulo (`FUN_101913fc`), so the wraparound itself is safe; the
   compaction math and the 16/16 boundary are the two places worth chasing. The
   ingest also calls the drain `while (count > 4) FUN_1018e93c(...)`, and an
   adding new segments each reload crashes in 5 s (2-3 reloads at
   `TARGETDURATION:2`), while a window that jumps **backwards** survives 40 s of
   rewrites — so the kill is in the forward append, not in error reconciliation.
   **The TS layer itself is hardened:** `FUN_101fb134` (tsclient) rejects
   `afelen > 0xBB` outright (`if (0xbb < local_140[0]) return 0;`) and reads the
   field into a 192-byte stack buffer, so CVE-2023-27355's overrun shape is gone
   from 57.23; `peslen` is 16-bit and can wrap, but nothing copies by it. The
   crash is an HLS life-cycle bug, not a demux copy. Beads `re-hls-kill`.
   **And it is a write, not just a crash** (instruction-level, 2026-10-23): the
   compaction computes `new_head = 0x10 - (limit - head)` with no bound check and
   copies `(limit-head)` entries of `0xc50` to `new_head*0xc50`. The array is inline
   at `obj+8`, so `limit-head == 17` — which is just the first append after the
   split/reset, since that path sets `head = limit = 16` and head then wraps to 0 —
   copies `0x81d0` bytes to `ring-0xc48`. Content copied is our own segment records. The kill is **not**
   monotone in the per-reload delta, though: 1 and 2 new segments per reload kill reliably
   (7.2 s and 8.3 s, three and two deaths in 25 s), while 4, 8, 24 and 40 survived a 25 s
   window and 40 only died on a third run at 11.4 s. So the trigger is the ordinary forward
   append and a larger delta is not a larger copy — append+drain keeps `limit-head` small.
   Geometry, corrected **again** (0xc50 stride, ring at `parent+0xc508`, so dest =
   `0xc510 - C*0xc50`): the original note was right and my later "inside the parent" fix was
   wrong. The copy always ENDS at the array start, so the counter block is the *tail* of every
   copy, and `C` walks the destination backwards: `C=15` -> `parent+0xc60`, `C=16` ->
   `parent+0x10` (the vtable/header words), `C=17` -> `parent-0xc40`, `C=40` -> `parent-0x12770`.
   That is why 16–20 is the fatal band (vtable and first pointers) rather than a counter effect.
   The 0xc50 bytes before the array cover the four
   counters at `+0xc508/0xc50c/0xc510/0xc514`: they are overwritten with our segment bytes and
   only `head` is restored afterwards, so after each compaction count/tail/limit are
   attacker-chosen and then used as indices (`mulli …,0xc50`). That is a controlled write over
   the queue control block, not a heap underrun. The counters are written from the **last four 32-bit words of the copied range**
   (i.e. the tail of the last entry), not the first, so whatever field sits at entry+0 is the injected value; entry offset 0xc18 is a
   double (segment duration). Whether entry+0 is a copied URI or a pointer decides if the
   injected words are ASCII-bounded or heap pointers.
   Counter roles are now partly resolved: `0xc50c` is **count** (the wrap path does
   `count = limit` before resetting limit, and the split path sums into it) and `0xc514` is
   written as 16 at exactly those two sites and nowhere else in the live binary — which is the
   open puzzle, because a diff of 17 needs limit to be 17. Something must increment it through
   a register-held base; the append after the ingest's success path is where to look.
   Entry layout, partial: the setter `0x10191458` fetches the entry by index and operates on
   `entry+0xc28/0xc2c` as a two-word string field, next to the double at `0xc18`; entry+0 is the
   only plausible place for the URI. If that is a `std::string` the injected words are
   ptr/size/capacity — two of them small — and the next compaction reads a modestly earlier
   offset; a plain `char[]` would put ASCII in all four and wrap every index. The measured
   2–3 reloads to death leans to the `std::string` reading.
   **Confirmed live, and it is a dial.** Identical advancing windows: 5-character segment names
   (short, so the value sits in the string's SSO buffer) die at **0.83 s**, on the first
   compaction; 40-character names (heap string, so the word is a real capacity) **survived 40 s**
   untouched. So the compaction's `limit` really is string internals and **URI length sets it**:
   short is fatal-and-immediate, long is an orderly compaction the queue tolerates. Capacity
   grows with length, so a long URI is a large but chosen backward shift — the knob for landing
   the copy on a target (e.g. the second sub-object at `parent+0xcd48`).
   Length map measured (25 s each, only the name length changed): 5 -> 0.83 s, 16 -> 17.9 s,
   20 -> 11.9 s, and 24/28/32/40/64/128 all survived. The fatal band is **16–20 characters**; from
   24 up it is stable. Destination is `ring+8-capacity*0xc50`, so `capacity = (8-T)/0xc50` lands the
   copy on a chosen parent offset `T` — length is a coarse index into the parent, 16–20 is the
   "make it die" setting and >=24 is the working-parity setting.
   Honest correction to "survived": **the survivor lengths do not play, they wedge.** With
   RelTime as the oracle (liveness alone was misleading), C=24 and C=40 both go TRANSITIONING ->
   STOPPED within ~10 s and sit at 0:00:00 while HTTP stays up. Real behaviour of an advancing
   window is: any length kills playback within a few seconds; 16–20 additionally kills the
   daemon; >=24 keeps the daemon alive with playback wedged. A fixed (non-advancing) window still
   plays, so it is the append/compaction path, not HLS itself.
   Band completed: 128/256/512 characters also keep the daemon alive (22 s each), so the
   daemon-death band is **<=20** and >=24 is daemon-stable up to at least 512. Why 16 matters: it
   is the smallest `C` that lands on the parent's low pointer words (dest `parent+0x10`) while
   leaving the vtable at `0x0` alone — if one of those words is the array pointer it becomes a
   valid heap pointer to bytes we wrote, and later `ptr + idx*0xc50` lookups read our URI text as
   entries. That would be a fake-array primitive; the parent init at `0x101922e8` (which writes
   `+0xc` and `+0x20` in the sibling shape) is where to confirm the offset.
   Init confirmed (`0x101913b0`): it writes the capacity argument to `+0xc`, the array pointer to
   `+0x20`, and zero to `+0`. So `+0xc` is seeded 16, it is the same slot the compaction reads as
   `B`, and it is only ever statically written as 16 — capacity and "limit" are one variable and
   the increment is invisible statically. `B-A > 16` therefore needs either that unseen increment
   or a `+0`/`A` that is negative or >16. The ring also holds an inline array at `+8` *and* the
   pointer at `+0x20`; the copy uses the inline address, so `+0x20` only matters where code indexes
   through the pointer. Dynamic step: watch `ring+0xc` and `ring+0` across a 1-segment window and
   log them just before each call into `0x10192674`.
   Length map measured (25 s each, only the name length changed): 5 -> 0.83 s, 16 -> 17.9 s,
   20 -> 11.9 s, and 24/28/32/40/64/128 all survived. Fatal band is **16–20 characters**; from 24
   up it is stable. Destination is `ring+8-capacity*0xc50`, so `capacity = (8-T)/0xc50` lands the
   copy on a chosen parent offset `T` — length is a coarse index into the parent, and 16–20 is the
   "make it die" setting while ≥24 is the working-parity setting.
   Invocation details the harness needs: the path is `/MediaRenderer/AVTransport/Control`
   (the short `/AVTransport/Control` answers 405) and `SetAVTransportURI` needs DIDL
   metadata — empty returns 500, an `audioBroadcast` item carrying the `hls-radio://` res
   is accepted. Live-window server: `captures/research/hlssrv.py` (any `*.m3u8` yields the
   next 40-segment window, any `*.ts` a fixed sample packet).
   Payload primitives for the day a smash lands: 34.16 `execve` gadget
   `0x4f4178` (`a0=*s1`, `a1=argv`, `a2=environ`, needs `s1`), and 86.10
   `0x656a86` (`a1` alone drives both, path via `*a1`). **Neither is the live
   unit**: the exact 57.23 daemon is PowerPC (NX on, no canary/PIE/RELRO), so
   these are shape references only and PPC gadgets must be derived from the
   decrypted build. NX is off on 34.16, on for 86.10 and 57.23.
2. `POST /diaglevel` grows an append-only global diag hash by one `calloc`+`strcpy` per
   unique `module` name, pre-auth, with no NULL check on the allocation — ~2800 names
   kills the daemon (~20 s outage, watchdog restarts). DoS proven; the interesting
   question is whether the bucket/list index math can be walked out of range (see
   "Update gate" section for the measured behaviour and `captures/research/diagflood.py`).
3. `/getrs?id=N` indexes a setting table with no bounds check and then
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

### Update gate, measured on the running 57.23 build (2026-10-22, second session)

The 57.23 line is no longer blind: the model-16 (ZPS5) package is decrypted with the
recovered per-model key, so `opt/bin/anacapad`, `bin/upgrade`, `libsonoscrypto.so.1`
and the rootfs of the *exact live build* are readable (see
`captures/firmware/57.23-74170-1-16/PROVENANCE.md`). It is **PowerPC**, so the MIPS
tooling in this file does not apply; `r2 -a ppc -b 32` and `objdump` do not decode it
on this host, while `qemu-ppc` does run it (below).

**Live A/B that settles the earlier "99.0 also aborts" result.** The 2026-10-22 sweep
served a *model-5* image to a *model-16* unit and concluded the version gate could not
be spoofed. That was the model mismatch, not the version. Re-run with the model-16
image and **only the type-1 version record** patched 57.23.74170 → 99.0.1: the player
reads all 5,606,749 bytes, then installs nothing and keeps answering 57.23. So the
compatibility gate is model+version and is passed by a correct-model image; what stops
an unsigned image is the step *after* the download. No section is written and no
install script runs.

**Order of the running updater, from the decrypted 57.23 `bin/upgrade`** (function at
`0x10007fa0`; strings at `0x10016ec8…0x1001873c`): fetch and parse the record stream →
per-record compatibility checks ("Min version %u.%u-%u", "Compatible with model …",
`Upgrade blocked.`, "Failed compatibility check" → error 10) → signature-section parse
and verify ("No valid signature" → error 6, "Sonos signature verification failed") →
*then* `upgrade_write_sections` (`0x10008890`), which opens `/var/upgradescript`,
runs `/bin/sh /var/upgradescript umountnone` (`system`, `0x10008af4`) and aborts on a
non-zero exit, then flashes, then `upgrade_postinstall` runs
`/bin/sh /var/upgradescript postinst`. `do_upgrade` only reaches the script when the
whole validation pass returned 0, so **an unsigned image never executes its script**
today. `/jffs/VERSION` present == "debug build": `int_doWriteFlash` returns early
("Detected /jffs/VERSION, perfoming no-op update"), i.e. debug builds validate and then
skip the write.

**What the policy bypass actually gates** (34.16 build, which still carries the symbol;
exact-fit keys and `/tmp` writer confirmed by disassembly): `allow_policy_bypass` is
consulted at exactly two call sites — before reporting *Downgrade blocked.* and before
*Invalid update state*. It returns true when `/tmp/device_unlocked_flag` exists
(`stat`, helper `0x410b40`), when a 13-character `/jffs/...` path (the only 13-char path
constants in the binary are `/jffs/VERSION` and `/usr/bin/wget`) passes a read+compare
helper (`0x410b90`), and never from two default-return-0 stubs. 57.23 keeps the same
behaviour under different strings — `/jffs/persist/deactivated_state` and
`…deactivation_pending_state.json` ("Downgrade blocked (deactivated).") — but the
`allow_policy_bypass` symbol itself is gone from that build. Consequence: the bypass is
worth exactly a *policy* waiver (older version, deactivated-state downgrade); it does
not by itself make a forged signature verify. Both cheap enablers are files
(`/tmp/device_unlocked_flag`, `/jffs/VERSION`), and no pre-auth surface writes files.

**File-write surfaces, checked.** `SystemProperties.SetString` accepts arbitrary key
names and persists them (`GetString` reads them back), but the store is one XML file
written `"%s.tmp"` + `rename` (`RSystemSettingsBase`/`RSPSetValue`), so the key is an
XML tag, not a filename: `SetString` cannot create `/jffs/VERSION`. `/advconfig` writes
only two `atoi` ints; `/set-cr-netstart-dest-mac` writes tmpfs only. `/jffs` consumers
that *execute* are `/jffs/system/Configure.dev` (`sh`, `rcS`), `/jffs/Configure`
(`exec`, `wifi/Configure`) and `/jffs/debug/testpoints.sh` (sourced) — all
existence-gated, so any one arbitrary-content file write is root.

**`/status/<path>` is a file *and command* reader for a fixed table** (not just the six
links its index page shows). Working: `/status/proc/ath_rincon/status`, `/status/dmesg`,
`/status/ifconfig`, `/status/uptime`, `/status/jffs/upgrade.log`,
`/status/tmp/upgrade.log`, `/status/jffs/watchdog.log`. Two gotchas: `/tmp` and `/opt`
are symlinks to tmpfs, and `/status` does not follow them reliably, so log reads come
back empty; and the update attempt is performed by `/bin/upgrade` spawned from
`/etc/runanacapa` on the *next* daemon start, so `/tmp/upgrade.log` (its stdout) is the
place a failed run reports — read it right after a probe, before it rotates.

**Offline harness (works, one blocker).** The 57.23 rootfs is big-endian squashfs 3 and
`unsquashfs` extracts it. With `qemu-user` installed, `chroot <rootfs> /qemu-ppc
/bin/upgrade` runs the *real* updater (uClibc PPC, static-pie qemu) and prints its whole
decision trace, given `/ramdisk/var/run/upgradeinfo` (`Force`/`Ignore`/`Report`/
`CurrentVersion`/`SystemVersion`/`URL` keys) and a local URL; `/tmp/upgrade.log` in the
chroot receives the same lines. Blocker: without a flash device the hardware-feature
compatibility check sees feature set 0 and fails with error 10 before the signature
stage. `/dev/mtd0` exists via `mtdram`, but `hal_flash_info_init` still reports "init
failed" (it wants a valid on-flash partition table for its offsets), so the MDP at
`/dev/mtd` offset 0 is never read. A one-word patch at file offset `0x8654`
(`li r28,0xa` → `li r28,0`) gets past the verdict and shows the next real failure
("No valid signature", error 6), but the parser still stops at the compatibility record,
so a real MDP is needed to exercise the signature code path end to end.

**`POST /diaglevel` is a pre-auth remote kill (found 2026-10-22, second session).** The
page takes `module` (≤31 bytes after the 32-byte parser cap) and `level` (`atoi`), with a
single-use CSRF token from the GET form, and `set_diaglevel` inserts the name into a
global diag hash: `HashAdd` `calloc(1, strlen+0x15)`, `strcpy`s the name at node+0x14 and
links it, with **no NULL check on the allocation**, and nothing ever removes an entry.
Push ~2800 unique module names (two requests each: GET for a token, then POST) and the
daemon dies; the watchdog restarts it, HTTP is refused for ~20 s and the house is dark
until it is back. Measured once at 2800 in a single run; a split run died at ~350, so
the threshold is not stable — treat it as "a few thousand". Reproduction:
`captures/research/diagflood.py` style loop (`GET /diaglevel` → token → `POST` with a
unique `module`). It is a DoS, not yet a controlled write: the name is copied by
`strcpy` into an exact-size allocation and the insert path is bounded.

**Version flips under a probe campaign.** During the restart series from the flood
above, the unit served `57.22-65130` (SoftwareDate 2025-05-13) for several minutes,
then returned to `57.23-74170` on its own. Careful: a parallel session was flashing
the same unit in that window (it landed a genuine `57.22-65130` image), so do not read
the flip as pure watchdog fallback — but the S1 platform does keep the previous image
in a second boot section and a failed boot falls back to it, which is why a reboot
series can move the version under you. Two rules follow: re-read `/status/VERSION`
before comparing versions, and check who else is on the unit before attributing a
version change. ICMP stayed up throughout except during the actual reboots.

### The signature ring carries ids, not keys (so self-signing needs a vendor key)

Answered the open question from the parallel session, on the 34.16 `libsonoscrypto.so.1`
(the same code family 57.23 verifies with):

- `ZZ0121(ctx, buf, len)` zeroes a `0x22b8` context, requires magic `0x621da74d`, then a
  `u32 total`, a `u16` of 1, then a `u32 n` (`<9`) of entries, then a second `u32 m` (`<9`)
  of entries. Every entry is **`0x108` bytes** and is parsed by `ZZ0026` as
  `[u32 kind][u32 len]` with `len < 0x101`, then `memcpy(entry+8, …)`: kind 1 needs
  `len == 0x21` (33), kind 2 needs `0x11` (17), kind 3 needs `0x15` (21). Second byte of
  the payload is a key-type byte that `ZZ0118` validates.
- 33 bytes is a truncated-hash fingerprint, 21 a 20-byte digest, 17 a 16-byte key id —
  **no entry can hold an RSA-2048 modulus**, so the ring references keys, it does not
  carry them. The key itself comes from the caller's context: the verify chain the update
  binary passes in is `ZZ0078 / ZZ0063 / ZZ0110 / ZZ0039`, and `ZZ0110 → ZZ0109` resolves
  through the caller's key ring (this is why "MDP signature missing" and "Cert bundle
  missing required signature" exist as errors) rather than from bytes in the image.
- Consequence: **a self-signed image needs a Sonos private key**; nothing keyless is left
  in the update path. The keyless leverage is the version/patch-train policy, and the
  MDP is the one thing the offline harness still lacks (see above) — supply it and the
  whole path, including a genuine older image, runs offline.

### Harness, second round: the key is the wall, and a mid-stream trailer aborts

Extending the chroot harness needed the model key at runtime; the updater derives it from
flash MDP data, not from anything in the image:

- `hal_flash_get_mdp2` reads 0x1000 bytes at flash 0x200 and accepts only magic
  `0xca989b4a` or `0xfa87b921`; `hal_flash_get_mdp` reads 0x200 at offset 0 and accepts
  `0xce10e47d`. A `libflash` shim (PPC, cross-gcc, `-shared -nostdlib`,
  `-Wl,--hash-style=sysv -Wl,-soname,libflash.so.1`) that serves those from a file is
  enough to get the getters called, but every zero-filled MDP2 still ends at
  "no decryption key": the update path re-derives a decryption key from MDP fields plus
  the static 0x4f0-byte blob at `0x10016ef8`, so **real MDP bytes are required** — a DER
  key pasted at 0 or at 0x780 is not sufficient. Getting them wants a device-side dump.
- Loader facts worth keeping: uClibc needs the **SysV hash** or it segfaults during
  relocation, and `LD_PRELOAD` paths are opened **on the host** by qemu-user, so a
  preload under `chroot` must be given as a host path (`LD_PRELOAD=/tmp/root/lib/x.so`,
  not `/lib/x.so`) — with the guest path ld.so reports "cannot preload" and silently
  ignores it. Preloading over the *real* libflash (instead of replacing it) is the right
  shape: `hal_flash_info_init` then still does its own partial setup.
- A hand-made `/dev/mdp` must be shaped (magic, `+0x64` flags `0x14`, `+0x70` version,
  a 3-char serial at `+0x98`); a zero file makes the MDP parser dereference NULL and the
  updater dies before any of its own code runs.

**Fuzz result so far (650 mutated images, offline, no device):** the 16-byte header reader
and the type-2 sub-record parser take zero-, short-, huge- (`0xffffffff`, `0x7fffffff`,
`0x100000`) and mismatched declared lengths, random bodies, 20 000 records in one file,
and unknown section types without a single crash or hang. One real defect showed up:
**a type `0xffffffff` (trailer) record placed where a section is expected aborts the
updater** (SIGABRT) — `hdr + trailer` alone reproduces it, and so does any stream with a
trailer before the end. On a device that is a failed upgrade plus a reboot; it cannot be
reached on a full image because the key error fires first. Next fuzzing needs the key:
everything after the type-22 cert bundle is unparsed without it, so the script, kernel,
rootfs, cert-bundle and signature parsers are all still untouched offline.

### `upgradeinfo` accepts keys anacapad never writes (pre-auth policy override)

`/bin/upgrade` parses `/var/run/upgradeinfo` as `Key: [value]` lines against a key table
at `0x100178c4..0x100178f4`: `Progress`, `Report`, `SonosID`, `HouseholdID`,
`CurrentVersion`, `SystemVersion`, `Timestamp`, **`OriginClient`**, **`PrevSWGenMinVersion`**,
**`MinVersion`**. `RTopologyImpl::BeginSoftwareUpdate` only ever templates the first seven
plus `Force`/`Ignore`/`URL`, so the last three can only arrive by **injection** — and
`ExtraOptions` is written into that file verbatim, so an XML character reference
(`&#10;`, a bare LF is normalised to a space) appends arbitrary lines:

```
ExtraOptions = &#10;MinVersion: [1.0-1]
```

Measured: in the chroot harness a `MinVersion: [1.0-1]` line turns an older image's
post-download verdict from `Upgrade blocked. (9) downgrade blocked` into a pass (the run
then proceeds to the encrypted-section step). It does **not** help with the *fetch-side*
abort: on hardware the same image with the same injection is still cut off at exactly
`0x20000`, so there are two distinct version gates and `MinVersion` only reaches the
second. `PrevSWGenMinVersion` and `OriginClient` are parsed but not yet mapped to any
behaviour.

### A 34.16 body can be made to parse cleanly on 57.23 — and then only the signature stops it

Chasing the downgrade floor: a genuine model-16 `34.16-37101` image aborts on 57.23, and
the abort is *not* the unknown section types 9/10 — it is the **trailer record**
(`type 0xffffffff`) that every 34.16-era image ends with, the same defect reproduced in
the fuzz above. Remove only the trailer and it parses on; it then hits the version floor
(`Upgrade blocked. (9)`), which the injected `MinVersion` clears. With the trailer gone
*and* a synthesised type-21 patch-train record (`2021_11/2023_01/2023_12/2024_12`) spliced
after the version record, the whole 4.7 MB body parses, the compatibility section
matches, and the run ends at exactly one thing: `No valid signature` (6).

Because 34.16 has no signature section at all, **every edit above is free** — so this is
a byte-exact recipe for the moment a signing key exists, and it also proves the only
remaining wall on a keyless network downgrade is the signature, not the format.

### Writer inventory: no `/jffs` path is name-controlled (kills the direct Configure.dev route)

Every `fopen`/`open`/`rename`/`unlink` in the live 57.23 `anacapad` was enumerated from a
full `.text` disassembly (`0x1000755c-0x1043cbb8`, 324 call sites) and each path
argument resolved against `.rodata`. Result: **anacapad has no format string that builds a
`/jffs` path from a variable.** The only `%s` near a jffs path is
`file:///jffs/sonar-tone/%s`, and that is a playback URI (a read). Every write target is a
fixed literal — `localsettings.txt`, `settings/*.xml`, `sonarEQ.xml`,
`system/{dsp_disable,src_disable,audio_out_disable,overrideOffsets,save_sntp}`, `sntp.txt`,
`shadow/*`, the logs. `netstartd`'s `"%s/Configure"` prefix is a constant root as well, so
`/jffs/Configure` (exec'd from `wifi/Configure`) is just as unreachable.

Therefore neither `/jffs/system/Configure.dev` nor `/jffs/VERSION` can be created by any
shipped writer, and the writers that do exist (`SetString` → one XML file via `.tmp` +
rename; `/advconfig` → two `atoi` ints) cannot aim at them. The boot-exec target is real
(`rcS`: `sh /jffs/system/Configure.dev`) — only the write is missing.

### `POST /setstring` is a one-request pre-auth kill

`/setstring` is a plain HTTP form on 1400 (CSRF token from its own GET, fields `key`,
`value`), i.e. no SOAP envelope and no API key. One POST answers `200`, the value
**persists** (readable with `GetString` after the restart) and the daemon dies ~25 s later
for ~80 s; a second POST rebooted the box (ICMP down ~36 s). 1 request versus ~2800 for
the `/diaglevel` flood, so it is the cheapest DoS on the device by three orders of
magnitude. The value surviving means the write completes before the hang — the lock or
replication path is the likely deadlock.

**It does not walk the boot slot.** Two writes (a benign key, then `BootSeq=1`) on
57.23 produced a *daemon* restart loop (down/up every 6-12 s for minutes; ping
stays up while HTTP is down, so the box does not reboot) and `SoftwareVersion`
stayed `57.23-74170` across ~12 cycles. Slot fallback is driven by the
bootloader's failed-boot counter, which daemon restarts never touch; reaching
the other slot therefore needs real power cycles or a reboot the updater
performs. The form needs URL-encoded fields (`csrfToken`, `key`, `value`) and
returns "Setting changed".

### Two device sweeps, both negative (bundle parser, devmode ring)

**Cert bundle (type-22), 36 mutants on the device** — record lengths `0/16/17/0x200/0x1000/
0xffffffff/0x7fffffff/len+4096`, magic bytes, inner header bytes at `4/8/0xc/0x10/0x1c/0x20/
0x30/0x32/0x13a`, and bit flips in the ciphertext. Outcomes are only "no fetch", a partial
read, or the full 5.6 MB with the usual ~10 s drop; ICMP loss never exceeded a normal
reboot (~24 s). No crash, no hang. (About a third of the runs lost the trigger while the
unit was still restarting from the previous case, so the real denominator is lower.)

**Devmode statement, 13 shaped rings** (1/2/3 entries with correct kind+length, bad magic,
count 9, under/over declared length, kind 0/9, truncated, magic only, empty) — all 403, but
they split the response time cleanly: parser rejects come back in 0.6–1.0 s while anything
that reaches the RSA verify takes 2.7–5.7 s. That is a usable structural oracle for future
devmode work without a key ("≥2.5 s" = the ring parsed). The walk is lenient — a 4-byte body
containing only the magic still takes the slow path. Nothing crashes the verifier.

### The `403` page family is one policy flag, not a header

`/`, `/reset`, `/testenv` and `/diagmsg` all refuse with 403 through a *single* check:
handler → `bl 0x10362fe0` → `cmpwi r3,0` → `li r4,0x193` → 403 unless the check is
non-zero. `0x10362fe0` reads flag **22** from a global policy object (`0x10545f3c`) through
`0x10394420 → 0x10394078` (a locked bitset read); its sibling `0x10363018` reads flag **18**.
So the gate is device policy, not request shape — five user agents, `X-Sonos-Api-Key`,
`X-Sonos-Device-Id`, `X-Requested-With`, and POST as well as GET all still 403. `/diagmsg`
renders its heading before refusing, so its handler does run first. `/diaglevel`,
`/advconfig` and `/support/*` are *not* behind this flag. Next step is whoever sets bit 22,
and whether that is settings-backed (then `SetString` could arm it).

**Answered:** they are the devmode/permission bits. The setter neighbourhood `0x103940e0` is
the devmode statement path itself — it takes the policy lock, calls `ZZ0137` to verify a
statement, then stores the result and the flags — and the page gate reads bit 22 of the same
object. So `/`, `/reset`, `/testenv` and `/diagmsg` open **after a successful devmode
unlock**: another capability hanging off the same missing RSA key, not something
`SetString` can arm. The object (`0x10545f3c`) and bits 18/22 are the pointers for later.

### Two small surface facts

**`SUBSCRIBE` CALLBACK grammar.** Accepted (200): a normal `<http://ip:port/path>`, two
callbacks in one header, no port, a 900-char path, 20 callbacks, port 65535, and a space
inside the path. Rejected (412): missing angle brackets, empty `<>`, `https`, IPv6 literal,
`user:pw@`. A 4000-char value resets the connection — that is the header-line cap
(~2044), not a parser bug; the daemon stays up. The parser is strict about grammar and
tolerant of length/multiplicity up to that cap. Delivery was checked against a real sink:
900- and 920-character *paths* subscribe and NOTIFY normally, `CALLBACK` of 2044 bytes
still parses (412 on grammar) while 2050 answers **413** and the daemon stays healthy,
and with two callbacks only the **first** ever receives NOTIFY. A subscription also leaks
`BootSeq` and each member's `ZoneName`/`ChannelMapSet` in the ZoneGroupState property.

**HLS AES-128 keys are fetched but never used.** `#EXT-X-KEY:METHOD=AES-128,
URI="ekey.bin"` makes the daemon fetch the key file (confirmed in the server log,
including a 404 and 0/15/16/17/32/1000-byte keys), yet an actually encrypted
segment never advances the transport: RelTime stays `0:00:00` for 20 s with the
correct 16-byte key and IV, with the key line before `#EXT-X-VERSION`, and with
no `IV` at all (default IV). The same segment truncated by one byte *does*
advance, and all key/IV length variants (missing key, 0/15/17/32/1000-byte key,
8/32/64-hex-digit IV, `SAMPLE-AES`) leave the daemon up. So decryption is not a
usable path here and not a crash lever. Playlist-level pathology is clean too:
a single 100 KB line, 5000 segment lines, CRLF endings, `MEDIA-SEQUENCE` of
`0xFFFFFFFF`, `TARGETDURATION:0` and a file with no trailing newline all parse
and play, 6/6 without a restart. Segment *switching* is not it either: two
entries pointing at the same file and two pointing at different files are both
6/6 clean, and so are two players fed the same playlist at once. After the
initial burst the only reliable statement is that HLS restarts this daemon
*sometimes*. (`hls-aac://` and `hls-static://` behave
identically to `hls-radio://` — the scheme only picks a service label. On 86.10
the same fixture also never advances, but that unit was timing out on the plain
control at the time, so the S2 result is not conclusive.)

**`CreateObject` never gets past 402 either.** `SQ:` browses as a valid, empty
container (`NumberReturned 0`), but `CreateObject` answers 713 for a DIDL with no
`res`, 710 for an empty `ContainerID`, and **402** for every DIDL that carries a
`res` — with `parentID` `SQ:`/`SQ:0`/`SQ:1`/empty, class `musicTrack` and
`playlistContainer`, and both `x-rincon-mp3radio` and plain `http` resources.
The store it would write is the fixed `savedqueues.rsq`, so again no path
control; it does parse far enough to distinguish 710/713/402.

**`CreateAlarm` never gets past 402.** `ListAlarms` answers pre-auth and shows an
empty list, but `CreateAlarm` faults 402 for every shape tried: empty
`<DIDL-Lite/>`, a DIDL in the public UPnP form, the Sonos form with
`xmlns:r`/`r:streamContent`, `Recurrence` `DAILY`/`ONCE`/`WEEKDAYS`,
`Enabled` 0/1, `StartTime` with and without seconds and as ISO, a wrong
`RoomUUID`, and past times. The fault is immediate in every case and does not
discriminate, so it is not ordinary argument validation — 402 is also what this
service uses when it cannot reach its store, and the store is the fixed
`/jffs/settings/alarmclock.xml`. Persistence target is fixed either way, so no
path control.

**`SetString` is not the settings table.** `key=ZoneName` answers 200 but the room does
not rename; `key=BootSeq` answers 200 and `BootSeq` in the group state stays 67; `key=VERSION`
answers 200 and the reported build is unchanged. All are just kv entries in the
`/setstring` store (readable back with `GetString`), not the settings object that publishes
them, so the name still needs `DeviceProperties#SetZoneAttributes` and a real boot-counter
change needs whatever writes the settings record. `SetString` is also *inert* for those
names while `SetZoneAttributes` is not — see below.

**The `SetString` store can be read back, and it has a ~1 KB value cap.**
`SystemProperties#GetString(VariableName)` — not `DeviceProperties` — returns
`StringValue` from the same store `/setstring` writes (a key written earlier read
back exactly). Values round-trip byte-exact up to 1000 bytes; a 2000-byte value
**resets the connection** and 3000+ answers 400, with the daemon staying up and
no key created. Newlines in a value are flattened to spaces, so a `key=value`
line cannot be forged by injecting into the value.

**The `DeviceProperties` setters are open pre-auth and they do write the settings
record.** `SetZoneAttributes(DesiredZoneName, DesiredIcon, DesiredConfiguration)`
renames the room with a plain SOAP call on 1400 and the name **survives a daemon
restart** (the settings store is re-read), so this is a persistent pre-auth write,
not memory state; `SetLEDState` / `SetButtonLockState` answer the same way and
`GetLEDState` reads the new value back. The SCPD also exposes `AddBondedZones`,
`RemoveBondedZones`, `CreateStereoPair` and `SeparateStereoPair`, `SetAutoplayVolume`,
`SetAutoplayLinkedZones`, `SetAutoplayRoomUUID`, `SetUseAutoplayVolume` and
`AddHTSatellite`, all unauthenticated — so pairing/bonding topology can be changed by
anyone on the LAN. Verified end to end on a live stereo pair: `SeparateStereoPair`
with the pair's own `ChannelMapSet` splits it (both halves revert to "Unnamed Room")
and `CreateStereoPair` with the same string re-bonds it, after which the old name has
to be set again — separation drops it. Zone names up to 4096 characters are accepted
without a restart. (The `:1443` proxy answers 402 for these only because its own
argument plumbing is broken; 1400 direct works.) `GetString`/`SetString` are not in
the SCPD at all, which is why the SOAP read of the kv store 401s. A write to an *existing* key also did not
wedge the unit — the ~80 s outage is specific to creating a new key.

### `BeginSoftwareUpdate` shuts the daemon down but never arms (both generations)

Controlled on 57.23 (.202) and 86.10-81100 (.81, via the passthrough): the action is accepted, the
file is fetched once, and the daemon drops ~3 s later — the deferred shutdown fires, so
`/etc/runanacapa` does get to run and no box reboot is involved. But `/status/tmp/upgrade.log` (the
redirect target it uses) and `/jffs/upgrade_sys_report.log` both stay **empty**, which means the
`[ -f /var/run/upgradeinfo ]` guard is false: `upgradeinfo` is still never written and `/bin/upgrade`


**The writer's mechanism, read (57.23 `0x1029c34c`).** It is a periodic worker: `gettimeofday`, a
10 000-unit delta, then a call to a predicate at `0x102c691c` that performs a **synchronous HTTPS
settings query**. Only if that predicate returns non-zero does it `snprintf` `"%s/%s"` of
`"/var/run"` + `"upgradeinfo"` and `fopen(..., "a")` — the file is *created* at open — and a second
site later re-opens the same path with `"w"` and writes the body. Content is gated on two bools
(`lbz 0x15e(r1)`, `lwz 0x118(r1)`); when they are false the file is still created and only the body
is skipped. `runanacapa` tests existence, so the predicate is the whole gate.

**Consequence, and it is the useful part:** the trigger is the **cloud's answer**, not the local
call. `CheckForUpdate` runs the same query and the cloud answers "same build" or "an upgrade you
already have", so `minVersion` never yields a downgrade record. Feed the daemon an answer that
carries one and the file appears — which is where the eMudhra forgery stops being only an auth
story: the query is HTTPS to a Sonos host and the trust store is the public bundle we can already
sign against, so a DNS redirect plus a forged leaf is a complete path to the writer. Order matters:
the file must exist *before* `BeginSoftwareUpdate` triggers `runanacapa`, and `/var/run` is tmpfs,
so it has to happen in the same boot. causes no drop and adds nothing when paired. So the whole update
path is one missing write away — whoever writes `/var/run/upgradeinfo` (`FUN_1029c34c`, after the
cloud settings fetch) is the single target.

### `BeginSoftwareUpdate` shuts the daemon down but never arms (both generations)

Controlled on 57.23 (.202) and 86.10-81100 (.81, via the passthrough): the action is accepted, the
file is fetched once, and the daemon drops ~3 s later — the deferred shutdown fires, so
`/etc/runanacapa` does get to run and no box reboot is involved. But `/status/tmp/upgrade.log` and
`/jffs/upgrade_sys_report.log` both stay **empty**, which means the `[ -f /var/run/upgradeinfo ]`
guard is false: `upgradeinfo` is never written and `/bin/upgrade` never starts. `CheckForUpdate` alone
causes no drop and adds nothing when paired. The whole update path is one missing file write away.

**Recent-CVE check (2026-10-22).** The only Sonos-tagged CVEs published in the last
month are both in **`tract`**, Sonos's ONNX/TensorFlow inference toolkit:
CVE-2026-75093 (`from_raw_dt_align` buffer size) and CVE-2026-55832 (attacker-chosen
offset/length selecting an out-of-range mapping slice), published 2026-08-18 and
2026-09-15, i.e. **after** the 2026-02-17 S1 build. They are not reachable here yet:
neither the 57.23 rootfs nor the 86.10 `anacapad` overlay carries a tract runtime
(no `_ZN5tract…` symbols, no `onnx`), and the `tract_*` strings that do appear are
`time`-crate leftovers. Re-check on any 97.x image, and if a tract-linked component
appears, look for a path that parses a model from attacker input (TruePlay tone data
is the obvious candidate).

### Safety notes for the next session

Use bench players first and expect a watchdog restart. Do not drive one unit
twice inside a minute. Announce state changes in Agent Mail (`am`) and leave
the house paused at low volume. The Mac's LAN interface has DHCP churn;
bind the source address explicitly when a device answers `EADDRNOTAVAIL`.

`192.168.4.0/22` is **routed** through an eero at `192.168.4.1`, not on the
attached L2. ICMP crosses it; TCP payload and unicast UDP do not (SYN is
forwarded, the SYN/ACK comes back, our ACK and data are never delivered, and
`hal_*`-era ARP entries never appear for those hosts). `192.168.5.0/24` and
`192.168.6.0/24` are on-link and fine, so bench on those. Do not read a `.4.x`
timeout as a dead player; it is the path.


## 2026-10-22 late: the update gate is version policy, and the signature is real

New, verified state; supersedes the "signed-bundle splice" open lead.

### Model numbers were confounding every earlier gate test

**Every S1 player on this LAN is model 16** (Play:5 Gen2 running S1 firmware),
while every `.upd` held locally was `-1-5` or `-1-12`. Every earlier
"a signed older image is rejected" result was therefore also a *model
mismatch*, and could not distinguish the version policy from the
compatibility section. Check `GET /devmode` (`Model:`) and the type-2
compatibility record (`[1][model][submodel_min][submodel_max]`) before
reading anything into a rejection.

Canonical model-16 URLs are derivable from the manifest Sonos still serves and
are public: `…/Prod/34.16-37101-v7.1-nzhemz-MS-1/34.16-37101-1-16.upd` and
`…/Prod/57.23-74170-v11.16.1-DILU1Q33hC-GA-1/57.23-74170-1-16.upd`
(`captures/firmware/manifest_s1_57.23.upm` carries the per-model list; model
16 appears once, at 34.16, with `flags="0x6"`).

### The gate is a version-policy check with a hardcoded floor

`bin/upgrade` (34.16) is the whole update pipeline: `wget_fork`, then the
section parser `fcn.0x40aa04`, then `upgrade_write_sections` (writes the type-3
script to `/var/upgradescript` and `system()`s it) and `reprogram_flash`.
Its compat handlers call `upgrade_compare_versions` against
`get_current_version`, and an *older* image is rejected unless
`allow_policy_bypass()` allows it, whose failure text is `Downgrade blocked.`.
The bypass is only consulted below a hardcoded floor `ver_this = 34.7-35162`
(string `0x4174fc`, parsed by `parse_version_string`), i.e. images in the
`34.7 … current` band need no policy at all. `/jffs/VERSION`, if present,
overrides `get_current_version` outright.

Measured consequence: an **unmodified version-record bump** (34.16 → 99.0) is
enough to make the daemon read the entire image instead of closing after the
`0x20000` sniff window. Model, submodel and the encrypted-envelope shape are
not what the early gate keys on.

### The signature is enforced after the download, with a clean oracle

The fixture `SONOS-BEGIN` probe is: trigger `BeginSoftwareUpdate` and watch
(a) how far the fetcher reads and (b) whether the unit reboots. A **genuine
same-version model-16 image** reflashes and reboots (~50 s), so that is the
accept baseline;

- version record bumped, signature copied → full read, then reboot in ~18 s,
  version unchanged, script never runs;
- `res` field of the type-3 record zeroed → same;
- type-3 payload replaced with a plaintext marker script (`touch`,
  `telnetd`, `sleep 150`) → same, and the 150 s hang never appears.

So any single-byte edit fails verification, and failing verification happens
*before* `upgrade_write_sections` — no script execution. Without the vendor
private key, a modified image cannot install on 57.23. A downgrade needs a
**genuine** model-matching image whose own version satisfies the policy.

### Signature handler shape

Handler table at `.data 0x10000090`, 16-byte entries `[type][a][b][fn]`; type
`0x11` (17) → `0x40a774`. It bounds the section at `(len-0x11) < 0x22b8`,
runs the "enveloped" reader `0x408b1c` and requires it to consume exactly the
payload, parses the ring with `ZZ0121` into a `0x22b8` context, then verifies
through `ZZ0124`→`ZZ0125`: per ring entry it resolves the key through a
callback, compares the hash algorithm, `memcmp`s a DER-hash fingerprint, and
only then verifies. The ring parser and both verifiers were already audited as
bounded; the remaining question is whether that resolver honour inline key
material (the devmode path does not — it pins the built-in via `ZZ0080`).

### Other surfaces measured this round

`/support/review` is unauthenticated on S1 57.23 and returns ~84 KB of live
network diagnostics (per-device `MinCompatibleVersion`, `LegacyCompatibleVersion`,
`HwFeatures`, topology) — good for a fleet inventory, no upgrade log. The
`/status/<key>` table on 57.23 is the short list it advertises (`zp`,
`enetports`, `VERSION`, `proc/…`, `ifconfig`, `showstp`); `/tmp/upgrade.log`
and `/jffs/upgrade.log`, present in the 34.16 table, return an empty document,
so the updater log is not remotely readable on 57.23.

Cross-check from the session that ran in parallel (see "Update gate, measured on
the running 57.23 build" above): the **57.23** `bin/upgrade` has no
`/tmp/device_unlocked_flag` string at all, and in a chroot harness neither that
file nor `/jffs/VERSION` changed the verdict on an older image — it still
answered `Upgrade blocked.` / error 9. So take the 34.16 bypass as a 34.16/56.0
property and re-measure it before relying on it on 57.23.

### SUCCESS: a model-matching older image installs over the air (and the patch-train is the gate)

After the signature findings above, the missing variable turned out to be the
**patch-train record (type 21)**, not the cert bundle or the version:

- a genuine `57.22-65130-1-16` image (the `57.22-59130` manifest lists it) →
  full 5.6 MB read, ~60 s reflash, and the unit comes back on **57.22-65130**.
  This is a complete, keyless, network-only downgrade from 57.23.
- a genuine `57.22-59130-1-16` image → the fetcher stops around `0x22000`
  (13 %), fast reboot, version unchanged. Its type-21 reads
  `2021_11 / 2023_01 / 2023_12`; the accepted build's reads
  `2021_11 / 2023_01 / 2023_12 / 2024_12`. Appending `2024_12_PATCHES:` to the
  rejected image's type-21 makes the daemon read the **whole** file again, so
  the required patch level is exactly what the gate is comparing.
- It still fails after that full read, because the type-21 edit breaks the
  signature. So the gate is satisfiable, the signature is not.

Practical floor: the earliest build that already carries `2024_12_PATCHES`.
That is the 57.22-61162 fix line, i.e. `SSA-2024-0002` — **by URL you cannot
downgrade a patched S1 player below the release that fixed it.** 34.16 sits
below the whole patch-train scheme *and* predates signatures, so it needs both
the patch gate and a signature it never had.

Public build-directory names are recoverable from the Wayback CDX index
(`url=update-firmware.sonos.com&matchType=domain&fl=original`) — that is where
`57.19-46310-v11.12-rozcvdwa-GA-1` and `57.16-41110-v11.9-wzhipjet-GA-1` came
from; the `v` tag is otherwise unguessable. Per-build manifests live at
`update.sonos.com/firmware/Prod/<version>-<build>-<vtag>/update.upm`.

### Update-path gotchas

`BeginSoftwareUpdate` is on **ZoneGroupTopology**, not DeviceProperties
(DeviceProperties has no such action on 57.23 and answers UPnP 401). The
fetcher follows the URL with a `Wget` UA and appends `cmaj/cmin/cbld/subm/rev/
reg/serial/sonosid/householdid`. `CheckForUpdate(UpdateType, CachedOnly,
Version)` always answers the newest build regardless of `Version`, and returns
both an `UpdateURL` (with the `^<version>` per-model placeholder, resolved
on-device) and a `ManifestURL`.

### Aftermath: what does and does not move the gate

- **Manifest flags are not device flags.** The model-16 34.16 entry carries
  `flags="0x6"`; that is a *manifest* flag. `BeginSoftwareUpdate`'s own `Flags`
  word maps as bit0 → `Force`, and the daemon's `Ignore` key is only zeroed on
  one branch, but neither `Flags=2` nor `Flags=4` installs a rejected image.
- **The enforced minimum is not the blocker.** `/support/review` shows
  `MinCompatibleVersion` and `LegacyCompatibleVersion` both `56.0-00000` on the
  unit, i.e. every 57.x image clears it. The type-21 patch level is the real
  guard, and it is covered by the signature.
- **A record appended after the type-17 is covered too.** 57.23 plus a trailing
  plaintext type-3 script record (after a valid signature) reads to EOF and then
  fails, so "append unsigned script after the signature" does not work either.

### WebSocket / REST API on 57.23, measured

`wss://<ip>:1443/websocket/api` with `X-Sonos-Api-Key:
123e4567-e89b-12d3-a456-426655440000` and subprotocol `v1.api.smartspeaker.audio`
answers pre-auth; a frame is `[{namespace,command,householdId,<params>},{}]`
(second element `{}`). `groups:1/getGroups` and `favorites:1/getFavorites`
return typed payloads.

The JSON parser is robust: array nesting 100 → **60 000**, object nesting
20 000, and string values of 1 MB and 4 MB all answer with the daemon alive
(the 4 MB case just exceeds a 10 s read deadline).

The UPnP passthrough (`upnpZoneGroupTopology`, `upnpSystemProperties`,
`upnpDeviceProperties`) has a different argument shape from the rest of the
API: `method` goes in the **options object**, `playerId` in the command object.
So `[{…,"playerId":…},{"method":"GetZoneGroupState"}]` answers
`success:true,result:0`. **Arguments never reach SOAP**: `params` as a map, a
`[{name,value}]` list, a bare list, nested objects, or at command level all
answer UPnP 402, including for `GetString`/`GetTransportInfo` which need
exactly one argument. The passthrough is therefore a no-argument proxy today
and is not a useful fuzzing surface for argument handling.

## 2026-10-23: test-host addressing (operational)

The Mac's wireless interface renumbers with DHCP and the old secondary aliases
die with it: mid-session `192.168.4.222` stopped answering while the interface
had moved to `192.168.1.226` and then `192.168.4.160`. Servers are bound to
`0.0.0.0`, so only the URL host and the client source address need updating —
re-read `ifconfig` and use the current `inet` value both for `--interface` and
in the `hls-radio://` URI. `192.168.4.160` is the working value at the time of
writing; `192.168.4.222` is dead. A stale host address shows up as the player
never fetching, not as an error.

## 2026-10-23: `BeginSoftwareUpdate` is open pre-auth and fetches our URL

`ZoneGroupTopology#BeginSoftwareUpdate(UpdateURL, Flags, ExtraOptions)` — the
action behind the update path — answers success **with no key and no token**
(the `:1443` `management/reboot` equivalent is 403, this is not). Pointing it at
`http://<mac>:8127/test.upd` made the device pull the whole 4.7 MB file from our
server in 10 s (logged). `CheckForUpdate(UpdateType, CachedOnly, Version)` is
open too and returns the vendor `UpdateItem`, which is where the real URL shapes
come from: `UpdateURL` is `…/^<version>` (no extension), plus a `ManifestURL`
`.upm`.

What does **not** happen yet: no install, no `/status/tmp/upgrade.log`, no
version change, and `/status/tmp/upgradeinfo` stays empty. Tried: a local
`test.upd`, the same file renamed to the real `34.16-37101-1-16.upd` (a
downgrade for this model), the vendor `^57.23-74170` URL, and `Flags` 0/1/2/4 —
all accepted, all inert; one vendor-URL call was followed by a daemon restart
but nothing else. The daemon only launches `/bin/upgrade` when
`/var/run/upgradeinfo` exists at start, so either the file is not being written
or a real box reboot is required — and `management/reboot` is exactly the token
we do not have. Still, the *fetch* half of the update path is now unauthenticated
and parameterised, which is what any local-manifest trick would need.

Who writes the flag file is now known and it is **not** this action:
`FUN_1029c34c` (log tag `topology`) queries the **cloud** settings and, only if
that query succeeds and carries a minimum-version/downgrade record, opens
`/var/run/upgradeinfo` with `fopen(…,"a")` and writes `MinVersion: [...]`. So an
install needs the cloud policy to admit a version, and the local
`CheckForUpdate` currently returns the installed build itself with
`DownloadSize=0` — the device has nothing to do. A local package only becomes
installable if the cloud is made to advertise something newer, or if the writer
is reached another way.

**The install half is a reboot away, and no pre-auth reboot exists.** On 86.10
`CheckForUpdate` really does advertise a newer build (97.2-81220 with
`UpdateURL`/`ManifestURL`), and `BeginSoftwareUpdate` with that exact
`UpdateItem` is accepted — the daemon bounces for ~20 s and returns on the same
version. Five `/setstring` writes and 120 WebSocket connects later `BootSeq`
(S2's own counter, 107) had not moved and the version was still `86.10-81100`:
the daemon restarts, the box does not. `DoPostUpdateTasks` changes nothing.
So the update path is: cloud advertises → we fetch (all pre-auth) → **the
device installs on the next box reboot** → and the only pre-auth trigger for
that reboot is missing, because `management/reboot` answers 401 without a token.
The S2 unit is left *armed* with the real 97.2 item, so a power cycle there will
apply it — useful to know before blaming the next session's test for a version
change. (The units reboot on their own often enough — `BootSeq` 91 and 107 — so
whatever escalates a repeated daemon crash to a box reboot is the thing to
find; it was not 120 WebSocket connects.)

Tried the obvious combine on S1 too — arm with a local `UpdateItem` in
`ExtraOptions`, then force `/setstring` writes until the box reboots — six
writes and three minutes later: still `57.23-74170`. (The vendor URL with the same item XML does
take the daemon down for ~70 s, versus ~20 s for a bare restart, so *something*
differs, but the version and the log stay unchanged.) `getZoneGroupState`'s
`BootSeq` is the reliable reboot counter — a daemon restart does not bump it —
and this bench unit is already at 91 from the session's crashes, so reboots do
happen; they just never coincide with an armed update.

## 2026-10-23: token validation is a *cloud* call, and its host is templated

When resolution fails the daemon answers **503 `service_unavailable`, "Could not
resolve token"** even for garbage (`Bearer AAAA`), i.e. it does not decide
locally first — it asks the cloud and reports the transport failure. That is the
single most useful fact for the token lane: the gate is a network dependency we
can own.

The endpoint is built from `https://%s-%s.lower-sslauth.sonos.com%s` plus
`/auth/oauth/v2/validate?access_token=%s` — the same two strings are in **both**
the S1 and S2 daemons. Since `lower-sslauth.sonos.com` is a public-CA host and
the device's bundle is the public root set, a forged eMudhra leaf is exactly the
right tool to impersonate it; the missing piece is name resolution (the device
takes DNS from DHCP, and the two `%s` prefixes are compile/config-derivable).
The builder is generic — the neighbours of that template are a service list
(`oauth`, `keys`, `clientdata`, `music-accounts`, …), a second form
`https://%s.%s%s` and `lechmere.%s.ws.sonos.com`. The `lower-*` form is the
lower-environment variant; the production form resolves:

```
oauth.sslauth.sonos.com  ->  CloudFront
GET /auth/oauth/v2/validate?access_token=AAAA  ->  403, "Client certificate required"
```

So the validator is **mutual-TLS**: the device presents a client certificate and
we must present a server certificate the device trusts — which is exactly the
forged eMudhra leaf, and we can ignore the device's client cert on our side. The
remaining prerequisites are put DNS for `*.lower-sslauth.sonos.com` (or the
production name) at us, and the response shape the parser wants (reacts with
"Invalid user", "Access token does not have adequate permissions", "Command
scopes could not be determined").

The scope vocabulary is solved, from `FUN_004bd3c0`: the `scp` claim is
**CSV-split** and matched case-insensitively against a three-entry `{bit, name}`
table — bit 1 `hh-config`, bit 3 `hh-config-admin` — and the bits are OR-ed into
the token mask at `+0x14`. Authorisation is `(token_mask & required) == required`
plus a `strcmp` on the user string, so `hh-config-admin` covers both bits. The
claim shape (CSV string or array) changes nothing yet because the token still
fails earlier, in the `iss` compare.

`iss` is the whole story on the claim side. The pre-gate is a `rfind` of
`urn:dev:ops:44974-zp-` (`FUN_00520578`), so a token that does not start with
that URN never reaches a claim check. Everything after it compares `iss` with a
fingerprint the daemon composes itself as
`{ owner { luid <luid> } hhid <hhid> network_hash <nh> }`, and **`<nh>` is
computed locally** (`0x603940` → `0x51b9b4`), so a literal in the token can only
match by reproducing it. Burnt shapes, all `401` with the resolver up: URN plus
brace with and without a space, URN directly against the brace, RINCON in the
`luid` slot, the brace alone, and the URN with a bare device id — the next read
is `0x51b9b4`, the hash getter itself.

That read narrows it: the composer's literals come from the pool at `0x6021d4` —
`owner`, `luid`, `network_hash` — and the fourth entry (`0x67797c`) is a **byte
table** (eight `0x10`, sixteen `0x40`, then `0x20`s), not text, so the hash slot
is not a plain echo. `0x603940` reads an *optional* out of the token context
(`0x47c6c8` is the optional-empty test, `0x51b9b4` a string copy) rather than
hashing there. Filling `luid`, `hhid` and the hash slot with the same value
(household, device id, RINCON) and matching `systemId` all still answer 401.

Recorded separately: with the cloud unreachable every token — forged, real or
garbage — returns 503, so a run of 503s says nothing about the token.

## 2026-10-23: why the trust anchors cannot simply be replaced, and where that stops

Swapping `libsonoscrypto.so.1` for one carrying our own keys is circular on the
face of it: `upgrade` has **no RPATH/RUNPATH** (checked with `llvm-objdump -p`;
only `NEEDED` entries), so the loader always takes `/lib` out of the read-only
squashfs, and writing there needs an install. Same for `/bin/upgrade` itself.

It stops being circular in one important place: on **S2 the daemon is not in the
rootfs at all.** The 86.10 rootfs ships only `opt/bin/anacapad.sha256`, and the
path inside it is `/jffs/app/install/bin/anacapad` — i.e. the daemon binary lives
on **writable jffs**. The same is true of `upgrade`'s own artefact writes
(`upd_cert_bundle.rcb` under `/jffs/sys/run/rcb/`) and of every settings store.
So a single arbitrary *file write* into jffs yields persistent code execution on
S2 with **no signature involved at all**; the signature only matters for
installing a whole image.

That reorders the target list: the pre-auth writers we have are all fixed-name
and fixed-shape (`/advconfig` writes `netsettings_mod.txt` from two `atoi` ints;
`/setstring` writes a kv store; `CreateObject`/`SetString` write their own files).
What is missing is a writer that lets the *path* or the *content shape* be
chosen. Note also that `SetString`/`SetZoneAttributes` prove jffs is writable
from the network path today, so the filesystem side is not the obstacle.

## 2026-10-23: VM harness — exact flash contract, and the trap in it

`upgrade` (57.23, PPC) takes options (`Eek! Unknown option.`, `No URL provided`,
a `-T` letter are in the binary) — worth checking for a key-file option before
emulating flash at all.

The libflash import layout, from the PLT relocations: `get_mdp` `0x1002bf7c`,
`ptable_read` `0x1002bfb4`, `info_exit` `0x1002c01c`, `info_init` `0x1002c034`,
`check_repartitioned` `0x1002c04c`, `get_bootsection` `0x1002c054`, `get_mdp2`
`0x1002c0ac`, `get_bootgeneration` `0x1002c1a4`, `program_m16` `0x1002c1cc`,
`get_mdp_wrapper` `0x1002c284`, `reprogram_flash` `0x1002c2bc`. The startup
contract is `get_mdp_wrapper(&info)` → `info_init(&ptr, 0)` →
`get_mdp2(ptr, &struct)`, and **all three store a pointer through r3** — a shim
that only returns it makes the next call write to NULL and segfaults. With the
storing fixed the binary still dies before printing anything, so the wrapper's
info struct needs its real fields; that struct is the remaining unknown.

Harness gotcha worth keeping: busybox `test` **segfaults** as a `while`
condition in this guest (`while /bin/busybox test …` kills init with
`Attempted to kill init`), so the offset sweep is written as a `for N in 0 8 …`
word list. `[`, `head` and `wc` have no symlinks in the rootfs; call
`/bin/busybox <applet>`.

## 2026-10-23: eMudhra's compromised intermediates are in the bundle, and the keys are public

The root bundle of **both builds** contains `emSign ECC Root CA - G3` *and*
`AAA Certificate Services` (Comodo) — exactly the two roots the October 2026
eMudhra incident hangs from. On 17 Oct 2026 the private keys of eight eMudhra
**intermediate** CAs were shown to be recoverable ("tiny integers", recoverable
almost instantly, Mozilla confirmed independently), and on 18-19 Oct the
intermediates were revoked and blocked by the root programs. A Sonos firmware
bundle has no CRL and does not consume OneCRL/CRLSet, so on 57.23 and 86.10 the
compromised intermediates are still trusted.

The recovered private key for **`EM DV TLS CA - G3B`** is published
(`github.com/Fluepke/emudrahahahahaha`; the scalar is almost all zero bytes) together
with its certificate. With it, a forged leaf verifies cleanly against the root
extracted from the device's *own* bundle:

```
openssl verify -CAfile emroot.pem -untrusted chain2.pem \
    -untrusted 'EM-DV-TLS-CA-G3B.crt.pem' leaf.pem   ->  leaf.pem: OK
```

So an assertion can now be signed by a leaf the device cannot distinguish from
Sonos's own. What the token validator wants is visible in the binary: the
assertion parser registers exactly `jti`, `iss`, `sub`, `aud`, `iat`, `exp`,
`systemId`, `riat`, `rexp`, `scp` (`FUN_00605084`, names at `0x6aff9c…`), i.e.
an OAuth-style device token with a scope list and refresh window. RS256 (RSA
leaf signed by the EC intermediate) and ES256 (EC leaf) are both accepted by
`openssl`; the daemon answers 401 to both with the `jwt-bearer` grant, so the
remaining unknown is the *claim content/issuer* the validator compares against,
not the chain. Kit and mint script: `captures/research/emudhra/`
(`mint_jwt.py`), key material stays under the ignored `captures/`.

Where it currently stops, precisely: the forged token used as
`Authorization: Bearer` on `management/reboot` is *inspected* and rejected with
`401 invalid_token` ("Access token is invalid") — not `API_KEY_VALIDATION_FAILED`
(which is what a non-token gets) and not `insufficient_scope` — so signature and
chain pass and the inspector itself fails. Its checks are visible:
`FUN_006021e4` normalises `token+0x20` (`iss`, via `FUN_00601ffc`) and compares it
to a trusted device identity, then compares a `%llu` of the expiration with
`token+0xd0`, with distinct result codes (2 = expired, 5 = bad expiration,
6 = "not minted by this device"). `resolveToken` answers `statusReason: UNKNOWN`
for every `iss`/`sub`/`objectType`/`scope` variant tried, and the daemon writes
no readable log (`/status/tmp/*` serves only known files), so the oracle is the
Bearer path plus the result code in the debugger-free code above. The next step
is the compare target, not the normaliser: `FUN_00601ffc` is a one-instruction
wrapper over `FUN_005f7210` (an empty tag string), so `iss` is not reformatted at
all, and the value it is compared to is `param_3+8` — reader state from
`FUN_00603aac`, the streaming reader that parses the assertion. So the expected
issuer is built from the token/payload context rather than being a constant, and
that reader is where to read next (the compare site is `0x603cf8`).

That read is done: `param_3+8` is **header** state, not payload — reader B
(`FUN_0060397c`, handler `FUN_00604a20`) binds exactly the JWT header fields
`x5c`, `alg`, `kid` (names at `0x6aff38`, `0x6aff3c`, `0x6aff40`), while reader A
binds the claims. So "minted by this device" compares a claim against material
the code derives from the header/`x5c`. Setting `iss` to the forged leaf's CN
(plain and `CN=` forms) still fails. Also failed: `iss` equal to the leaf's
base64 DER (the `x5c[0]` value), a single-cert `x5c`, `x5c` as a bare string, the
device id, the household id, `Sonos`, `sonos`, `api.sonos.com`, `muse`,
`https://muse.sonos.com`, a URN, `:1`/`:0` suffixed ids and an empty string — all
`401 invalid_token`. So the derivation is struct-derived, not one of the obvious
strings, and the next read is reader B's field handler (`FUN_00604a20` class,
`x5c`/`alg`/`kid`) to see where the compared `string*` actually comes from.

### Lane status (checkpoint)

What is proven: both firmware builds carry `emSign ECC Root CA - G3` and AAA
(Comodo); the recovered `EM DV TLS CA - G3B` key is the public "tiny integer" one
from the October 2026 incident; a leaf forged with it verifies against the root
extracted from the device's **own** bundle (`leaf.pem: OK`, re-checked). Kit:
`captures/research/emudhra/mint_jwt.py` — RS256 (RSA leaf under the EC
intermediate) and ES256 (EC leaf) both mint tokens that reach validation.

The oracle is now sharp: a forged token as `Authorization: Bearer` on
`management/reboot` returns `401 invalid_token` "Access token is invalid" — neither
`API_KEY_VALIDATION_FAILED` (a non-token) nor `insufficient_scope` — so chain and
signature pass and the **inspector** rejects it. That is the signal that was
missing when every failure looked like a blanket 401.

The inspector is mapped: `FUN_006021e4` compares `token+0x20` (`iss`) with reader
state and a `%llu` of the expiry against `token+0xd0` with distinct codes (2
expired, 5 bad expiration, 6 "not minted by this device"); `iss` is not normalised
(`FUN_00601ffc` is a one-instruction wrapper over `FUN_005f7210` with an empty
tag), and the compare target is `param_3+8` from `FUN_00603aac`, the streaming
reader — so the expected issuer is derived from the token context, not a constant.

Unknown and blocking: `resolveToken` answers `statusReason: UNKNOWN` for every
`iss`/`sub`/`objectType`/`scope` variant, and there is no readable daemon log
(`/status/tmp/*` serves only known files), so the Bearer path plus the code above
are the whole oracle. Next read: `FUN_00603aac` at `0x603cf8` — what fills
`param_3+8`.

**Oracle recipe (2026-10-23).** The reason string only appears when the
request carries **both** headers: `X-Sonos-Api-Key: <published dummy>`
selects the API-key path and the `Authorization: Bearer <jwt>` is then
handed to the token inspector. With the Bearer alone every forged token is
answered `400 ERROR_API_KEY_VALIDATION_FAILED`, which is why earlier trials
read as if the token were never parsed. The route is
`POST /api/v1/players/{playerId}/management/reboot`; household paths
(`/households/{hhid}/…` and `/households/{HouseholdControlID}/…`) answer
`404`, and the S2 daemon is **MIPS16** — plain `objdump` mis-decodes it,
`-m mips:16` is required.

**Answered — the value is a composition of claim names.** At `0x603cf2` the caller
sets `a2 = sp+516`, `a3 = sp+32`, `a0 = sp+264` and calls the inspector with
`a1 = s1` (the context). The buffer at `sp+516` is filled at `0x6020f8` by appending
four **literal claim names** in order through the literal append at `0x602050`, with
a separator append at `0x602094` between them:

```
0x602132  "owner"          0x677360
0x60213a  <separator>      (0x602094)
0x602140  "luid"           0x6af8d0
0x602162  "hhid"           0x67697c
0x6021e0  "network_hash"   0x6af8d8
```



**The prefix half is `urn:dev:ops:44974-zp-`.** The `rfind` guard at `0x603c92`
compares `sp+64` against the BSS `std::string` at `0x7048e8` (literal pool
`0x603d24`), which static-init builds from the literal at `0x666b40`,
`urn:dev:ops:44974-zp-`; the call to the inspector at `0x603cf8` is only taken
when `rfind(...) == 0`, i.e. the string must *start with* that URN. The
equality compare against reader state follows at `0x6021e4`.

Tried and still `invalid_token` (so still open): `urn:dev:ops:44974-zp-` with
the serial (`78-28-CA-92-21-DC:6`, `7828CA9221DC6`, lower-case), the bare MAC,
the sonosid (`114063254`), the household id and `Sonos_<hhid>`; and the bare
claim-name composition with `/ : . - _` separators. Which string `sp+64`
actually holds, and which claim feeds it, is the next read at the `rfind`. A mismatch logs
`"Device token not minted in this HH."` (`0x6af900`) under category
`muse_token_inspector` (`0x6af8e8`); the expiry failure is
`"Token is expired, security settings have changed since the t…"` (`0x6af924`) and
its value goes through `%llu` (`0x665e74`). Note the `hhid` compare at `0x60228c`
reads `token+108`, so the claim *values* still matter after `iss` passes.

**The `|` composition is **not discriminating** (2026-10-23).** The composition is built from the token's own claim values
in the order `owner|luid|hhid|network_hash`, joined with a pipe — with all
four claims set to `x` and `iss` set to `x|x|x|x` the endpoint moves off
`401 invalid_token` to **`503 service_unavailable` "Could not resolve
token"**, reproducibly. Every other separator (`/ : . - _ & , ""`) and the
claim-*name* variants stayed at 401, which read as "`|` is the live
separator and the check is self-consistency". **Caveat measured hours
later:** the same `x|x|x|x` token then answered `401` on four consecutive
tries, and so did a fresh `jti`, the all-household variant and the `iss=x`
control — with the unauthenticated baseline still a correct `403
insufficient_scope`, so the endpoint is healthy. Nothing about the token
changed. The 503 window (~2–9 s per call, `service_unavailable` "Could not
resolve token") therefore looks like a transient service state as much as a
passed inspection, and comparison inside a healthy window (control `401`) shows `x|x|x|x`,
`owner|luid|hhid|network_hash`, `x|x|x|`, `x|x|x` and the context-household
variant **all at 401** — identical to the control. So the separator is a
guess, not a finding; `503` is service flakiness that can hit every token,
and the only protocol that means anything is: gate on the control returning
`401`, then compare inside that window. Code is the only trustworthy oracle.

A further control-gated sweep of ten separators with all-equal values
(`, ; . - _ : # & = @`) is **all 401** as well, so it is not a separator
question at all — the composition must contain a device string for every
candidate to fail this way. The claim fields are `std::optional<std::string>`
shaped: a presence byte four bytes below the string (the `iss` guard reads
`lbu 108` before using `112`, and `iss` itself is `lbu 28`/string at `32`).

Value sources in the composer, by contrast, are concrete: separators sit
between segments; one value is read at `token+0x48`, another at `token+0x30`
(`lw a1,v0+7; addiu a1,41/65`), and the last is the context string from the
virtual call — so at least one component is device state, not a claim, which
explains why self-consistent issuers keep failing.

Composer arguments, read from the call site: `0x6020f8(out, token, ctx)`
where the second operand is the **token** (the first value append reads
`[+7]` of it) and the third is a `std::string` obtained from a virtual call
on the context (`[param_2+16] → vtable+12`). The iss itself is normalised
first into `sp+32` by the empty-tag wrapper. Both the token's own claims and
a context-derived string are therefore in play, which is consistent with
either reading but rules out a pure constant. Feeding
`HouseholdControlID` (with the `.T7aLjgf…` suffix) as `hhid` changes nothing
— still 401. Equal
values cannot distinguish the order; a real `hhid` with `x` elsewhere still
401s, so at least one more component must differ from `x` when the values
are mixed.

Grant/alg facts from the same round: only `grantType` =
`urn:ietf:params:oauth:grant-type:jwt-bearer` is recognised on
`authorization/authorizeDevice` (it answers `401` instead of the blanket
`403`), and `mint_jwt.py` now also mints **ES256** (`ALG=ES256`, EC leaf,
DER→raw R||S) which behaves identically — algorithm is not the
discriminator. The assertion path still answers `401` for every shape

The next gate is **resolution**, not inspection: the 503 comes from
`resolveToken` and does not move for `sub` = UDID, `sub` = short id,
`systemId`, or `aud` = UDID. `GET settings/player` and `GET info` answer
`200` with the same token, but they are also open with the API key alone,
so they do not yet prove anything about scopes.

**The response is one-bit (2026-10-23).** Expired `exp`, absent `exp`,
far-future `exp`, a bad signature, a missing `hhid`, and every iss shape
tried (names only, names+values, `"" / : | . - _ & = ,`, and the same
wrapped in braces) all return the identical `401 invalid_token "Access
token is invalid"`. Nothing in the body distinguishes *which* sub-check
failed, so issuer guesses cannot be validated incrementally — the string
must be reproduced exactly from code.

The composer is readable enough for that: `0x6020f8` builds into a
`std::string` and each segment goes through `0x602050` (append a literal) or
`0x602074` (append a `std::string` dereferenced from the argument), a
separator append (`0x602094`) sits between them with its value as a
constructor parameter (falling back to `'{'` = 123), the tail appends `'}'`
(125), and the result is compared with `std::string::operator==`
(`0x48cb64`) against a field of the token at `0x60229a`.

Next is mechanical: mint with all four claims present and `iss` equal to the
composition, trying `/` and `:` as the separator, and watch whether the reason moves
off `invalid_token` to the expiry class (2/5). Compose `hhid` from the device's real
household id so the later compares have something to match.

**Correction from the decompiler: `iss` is not the compared claim — `hhid` is.** Ghidra renders
`FUN_006021e4` cleanly (`DecompAt 0x6021e4` in `captures/research/ghidra`):

```c
FUN_006020f8(local_18, asStack_d0, local_70);                       // build the expected string
if ((*(char *)(param_4 + 0x6c) == '\0') ||
    (std::operator==(local_18, (string *)(param_4 + 0x70)) == 0)) { // expected == token.hhid
    ... "Device token not minted in this HH." ...  result 4;
}
```

`param_4` is the parsed token (the caller passes the claims object), `+0x6c` is a bool and `+0x70`
a `std::string` — an `optional<string>` holding `hhid`. This check demands the token's `hhid` equal
a value the device *computes*; `iss` is compared elsewhere.

That computed value is a brace-formatted fingerprint (`FUN_006020f8`): `'{'`, then `owner`, `'{'`,
`luid`, `<v1>`, `'}'`, `hhid`, `<v2>`, `network_hash`, `<v3>`, `'}'` —

```
{owner{luid<v1>}hhid<v2>network_hash<v3>}
```

`<v1>`/`<v2>` come from the first argument's string object, `<v3>` from the second. Sent live as
`hhid` with all four claims present, on `.4.81` and against ten separators/formats (`/ : | , - _ . \
n`, space, `;`), plain values, brace form and `Sonos_<hhid>`: every one returns the same
`invalid_token` / `Access token is invalid`.

Two things bound the remaining work. First, the caller runs `FUN_00520578(auStack_270, 0x7048e8, 0)`
*before* the inspector; `0x7048e8` is in `.bss`, so it is a runtime-populated set, a non-zero result
jumps to `LAB_00603c94` and skips the inspector entirely — every current token may be failing there
and never reaching `hhid`. Second, the inspector logs at level 7/6 under `muse_token_inspector` and
the message strings are known, so reading the log is worth more than another claim permutation.

### The claim schema is in the binary, and the WS command channel is a parameter oracle

The parser's claim-name block sits at `0x6aff9a` and, in order, is:

```
jti  iss  sub  aud  iat  exp  systemId  riat  rexp  pinEpoch  accessPolicy  route  protocolVersion
```

`FUN_00603aac` is a rapidjson `ReaderHandler` that fills a 232-byte claims struct (`auStack_120`) at
`FUN_00605940`; the inspector then reads fields of that struct — `exp` as a flag at `+0x64` over an
`int64` at `+0x68` ("missing"/"invalid" codes 5), an optional string at `+0xa4`/`+0xa8`, and the
claim at `+0x20`, which is rendered and compared against `*(param_3 + 8)` (the device's own id, code 6
"not minted by this device"). With `+0x20` landing on `iss` under a 32-byte stride hypothesis,
`iss` = the device's own id and `hhid` = the fingerprint over it; nine value combinations were sent
(`iss`/`sub`/`route`/`luid`/`owner` = the UDN, `hhid` as `{owner{luid<UDN>}hhid<HH>network_hash<…>}`
with the network hash empty/UDN/household) — all still `invalid_token`.

Also recorded: the result-code to reason mapping lives as strings `MALFORMED`, `REVOKED`, `HOUSEHOLD`,
`INVALID_REQUESTED_VALUE`, `NOT_MINTED_THIS_DEVICE` at `0x6afa58`, and the class vtable is `0x6afab0`
(slot 2 = `0x603b69`), referenced only from the descriptor table at `0x4d8dec`.

**The WS commander works once the frame is split correctly** — the WebSocket command channel gives
the parameter diagnostics that REST flattens into a blanket error. `[header, payload]` with
`playerId` **in the header** and the UDN as its value:

```
[{"namespace":"authorization","command":"authorizeDevice","householdId":"Sonos_…","playerId":"RINCON_…01400"},
 {"assertion":"<jwt>","grantType":"urn:ietf:params:oauth:grant-type:jwt-bearer"}]
```

gives `ERROR_MISSING_PARAMETERS: Missing playerId` for a payload-side `playerId` or the locationId,
`ERROR_INVALID_PARAMETER: playerId` when a locationId is used in the header, and only then
`ERROR_NOT_AUTHORIZED: Unauthorized.` — i.e. the same wall as REST, reached deliberately.
`resolveToken` on the same channel answers `success:true` with `statusReason: UNKNOWN` for every
claim variant (empty, `sub`, `owner`, `luid`, `hhid`, far-future `exp`), so it does not expose the
inspector's code either.

Live identifiers on `.4.81` (household `Sonos_wI8BAkz9WLbe5jdUaRXQSQMJW8.T7aLjgf696_0Ij2e5ZDB`,
location `lc_3ee386abdf5745068202801414f1a14a`, UDN `RINCON_000E58CADE9401400`) differ from the older
`Sonos_qN6HO…` value used in earlier probes — **re-read `/status/zp` before minting**, and note the
household id carries a `.` and a long suffix.

### Structure fully mapped; the inspector still rejects every shape

The header and payload handlers are both rapidjson `ReaderHandler`s, and their registered field
tables are readable:

- header (`FUN_00604a20`): `x5c` (cap 0x15), `alg` (5), `kid` (5) — `kid` is slot 0, so the
  `param_3 + 8` target behind code 6 ("not minted by this device") **is the JOSE `kid`**;
- payload (`FUN_00605084`): `jti iss sub aud iat exp riat rexp scp` (strings `jti/iss/sub/aud`,
  numbers `iat/exp/riat/rexp`, array `scp`) — so the claim at `+0x20` that is rendered and compared
  against `kid` is one of those, and `hhid`/the optional at `+0xa4` are Sonos extensions.

Two namespace strings matter. The claim formatter (`FUN_005f7210`) takes a tag that
`FUN_005f71d4` substitutes through the text, and that tag is a `std::string` in `.bss` (`0x7048e8`)
initialised at `0x46fa14` from **`urn:dev:ops:44974-zp-`**. The same object is the needle of a
`rfind(…, 0) != 0` prefix test that runs *before* the inspector and skips it on a mismatch, so the
received token is expected to carry that `urn:` form. Sonos claim namespaces seen in the build are
`urn:sonos:environment|user|hhid|udn|idtype|device`.

Every mechanical combination tried on `.4.81` still returns `invalid_token`: plain and
`urn:dev:ops:44974-zp-`-prefixed claim names, `Bearer` value with and without the `urn:` prefix,
`kid` as the UDN and as `urn:dev:ops:44974-zp-<UDN>`, `idtype=device`, and the braced `hhid`
fingerprint. Since the *same* rejection now covers a correct-looking structure, the next step is an
**emulator run of the daemon** (a MIPS guest with `/opt/s57` and a readable log) rather than more
permutations; the code paths log every reason under `muse_token_inspector` and the message strings
are known (`0x6af900`, `0x6af924`, `0x6af974`, `0x6af98c`, `0x6af9b4`, `0x6af9e0`, `0x6af9ec`).

Committed at `90d2155`; doc and mail carry the same stopping point.

## 2026-10-23: CVE-2023-50810 (U-Boot) is worth keeping on the list

NVD: "In certain Sonos products before S1 11.12 and S2 15.9 … a failure to
correctly handle the return value of the `setenv` command can be used to
override the kernel command-line parameters and ultimately bypass the Secure
Boot implementation", persistent with kernel privileges, and it names **PLAY:5
gen 2, PLAYBASE, PLAY:1, One, One SL and Amp** — that is this fleet exactly.
CVSS 6.0, `AV:N/AC:L/PR:H`.

The "before release X" phrasing does not mean fixed here: the OTA packages
carry no bootloader. The preinstall scripts touch `/dev/mtd` only for the
install flow and never `setenv`/`fw_setenv`, and the kernels embed only the
stock defaults (`root=/dev/ram`), so the effective `bootargs` are assembled by
U-Boot from its environment at boot. Nothing in the network surface writes that
environment today, which is why the CVE still needs a foothold: either a first
root primitive to write the env/cmdline, or an update-path write that reaches
`/dev/mtd`. Keep it as the persistence half of a chain rather than an entry.

## 2026-10-23: 86.10 differential — three unclamped copies the 57.23 build lacks

86.10's daemon was extracted from the preserved `.upd` (model-12 key recovered from
the plaintext 34.16-37101-1-12 updater, recipient `e35f7c21…`; the daemon itself
ships as the 3.8 MB overlay component, SHA-256 `1b598f11…`, matching the rootfs's
`anacapad.sha256`) and swept the same way: 981 stack-destination copies, 9 with a
variable length. Seven are clamped; **three are not**, and all three are on the
S2 device-registration / devmode path:

- `FUN_004f38a0` (source file `dev_…`): `local_118[256]`, `memset(…,0xff)`, then
  `memcpy(local_118 + (param_3 - uVar4), …, uVar4)` and the else branch
  `memcpy(local_118 + (uVar4 - param_3), param_2, param_3)`. Nothing compares
  `param_3` with 256 — the `max()` only feeds `*param_5`. If the caller-supplied
  length (from the devmode/registration frame) exceeds 255, the copy runs past
  the frame.
- `FUN_004f9818`: `memcpy(dest, param_3, 0x14)` then
  `memcpy(dest + 6, param_3[2], *param_3)` — the payload is written over the
  tail of the fixed header. The ring's free space is checked against
  `payload + 0x19 + strlen(name)` first, so it is safe unless that sum wraps;
  it is a telemetry writer and the payloads are small.

Follow-up on the first two after reading the callers:

- `FUN_004c0420` is **capped, not open**: the whole body sits inside
  `if (len < 0x21)` (32 fits, 33+ logs "Invalid registration signing key" and
  returns), so the u32 from `packet+0x20` is safe. Only the `FUN_004f38a0`
  finding stands.
- `FUN_004f38a0` is only ever called with a devmode statement result:
  `FUN_004f3b44` runs `ZZ0137` on the `POST /devmode` statement and passes its
  output length as `param_3`, and `FUN_004f3974` is the notification-only path
  (passes 0). **Correction 2026-10-24: `param_3` is a u8 payload *length*
  (`lbzx 9,31,27` in `ZZ0137`), so it cannot exceed 255, and the 256-byte frame
  is exactly sufficient rather than one byte short.** `memset` covers 255, the
  copy ends at `max(param_3, stored_len)` which is itself only ever stored from
  `param_3`, and the loop bound is the same max. No wrapper length is
  reachable, and the statement's 0x400 input cap is irrelevant to this bound. A
  later `libsonoscrypto` would need to return more than a byte for it to bite;
  see `docs/RE_DEVMODE.md` for the instruction-level walk.

**The 86.10 statement format is not the S1 one**, decoded from the S2 app's
`validateDevModeStatement` (`0x138f4`): it compares the magic word to
`0x711e7228` (the same little-endian `0x28721e71`), requires `len >= 0x1c`,
compares `[+4]` (`rev`'d) to a caller value, then requires `len >= 0x20` and
tests `rev([+0x18]) <= r0 <= rev([+0x1c])`, where `r0` is composed from two
caller arguments. So `+0x18` and `+0x1c` straddle a caller-supplied bound
instead of being equal constants as on S1, and the S1-shaped preamble (zeros
there) fails. The live page also reports the real model (`8`) and device id,
so the preamble is checkable from the page.

86.10 is the S2 build, so these are not on the 57.23 S1 daemon at all. The first
two need a peer that speaks the registration/devmode frames, which is the next
piece of work. Sweep saved at `captures/research/ghidra-8610/sweep8610.txt`.

**The daemon read matches the app prefix and completes the field map
(2026-10-24).** `FUN_004f3b44` → `ZZ0137` (`libsonoscrypto.so.3`, `0xca14`)
checks the same magic bytes, then `[+4]` against the model, then a version
window: `[+0x18]` (u32 BE) is a lower bound and `[+0x1c]` an upper bound, **both
inclusive** (`bc 13,28` leaves if `r0 < [+0x18]`, `bc 13,29` leaves if
`r0 > [+0x1c]`; `+0x18 == +0x1c == r0` passes). The version word is composed as
`(byte@obj+0x49d << 16) | byte@obj+0x49e`; `+0x49e` is the version halfword
written at `0x4dda2e`, and `+0x49d` is zero on this path, so the comparison is
effectively on the major-version byte. After the version comes `[+0x20]` (u8
devid length, must equal the caller's), a `memcmp` of the statement bytes at
`+0x21` against the caller's buffer (`0xcbd4`), then a u8 payload length and
the ring.

**Everything before the ring is signed, the device id included.** The verifier
chain computes the digest over `statement[0 .. ring_offset)` (`0xa7d4` calls
the hash with `data=statement, len=0x2e`; S1 at `0xa888`), and only then does
the RSA check. The `memcmp` is a redundant pre-check, so a public statement
cannot be patched to another serial. An instruction-level Unicorn harness
(`scripts/emu_devmode_stmt_s1.py`, `_s2.py`) reproduces the map: a valid
preamble + ring reaches the verifier, while bad magic, a wrong-id `memcmp`, an
out-of-window version or a missing ring stop before it; flipping one of the 16
"free" bytes still reaches the verifier. On the wire the status is `403` for
every decodable statement (`400` only for a missing/undecodable field), and the
old `>=2.5 s` signal is the rate limiter, not RSA: spacing under ~2.1 s makes
the daemon sleep ~2.1 s per missed interval. Portal procedure:
`docs/RE_DEVMODE.md`.

`POST /mtmhhsetup` (`0x4ac11c`, S2) is the MTM provisioning handler: a
urlencoded body with `hhid` (optional, `Sonos_MTM_` + 22 alnum or empty to
generate), `name` (≤64 bytes, must already be UTF-8-normalized), `icon`
(alnum/`:`/`-`, ≤128), `key` (24 base64url chars → 16 bytes) and `reboot`
(0–30). It needs a factory-reset unit, writes `HouseholdID`/`WEPKey` to
`netsettings.txt` plus the room `ZoneName`/`Icon`, and answers with
`{"hhid","key","rebootDelay"}`. Shape and error codes: `docs/RE_DEVMODE.md`.
The API-key header on that route is hash-checked against two built-in constants
(`0x4aa99c`), not presence-only.

## 2026-10-23: stack-copy sweep triage (live 57.23 anacapad)

The full-program copy sweep is 454 + 1789 sites. Of the 403 with a stack
destination, 28 take a non-constant length; audited one by one, **26 are
clamped** in the caller (explicit `min`, an early `return` above the cap, or a
contract that rejects an over-long value — e.g. `FUN_10287374` returns 2 for
`param_4 > 0x38` and then copies into a 56-byte buffer). Two remain:

- `FUN_102886e0` (eventing send): copies `param_4` bytes into a 2060-byte frame
  with no constant clamp — the only gate is `param_4 <= uVar1` where `uVar1`
  comes from a vtable call on the connection object, then the same length is
  `write()`n. Over-range needs a context larger than the frame, i.e. a source
  that reports an inflated context size.
- `FUN_101fb134` (tsclient) is already handled: `afelen > 0xBB` is rejected.

So the copy surface is genuinely clean; the live kill found today is HLS
life-cycle, not a copy.

The bundle parser was re-read with the same filter and is also clean.
`sonosRcbParseCerts` in `libsonosrootcerts.so.1` is only a 16-bit-masking thunk
into `FUN_10396e34` in the daemon, which caps the count at `0x12d` (returns
status 4, "too many certs"), `malloc(count * 0xc)` and fills fixed 12-byte
records through a validating reader. The header parser `FUN_10396bfc` checks
availability before every field, requires version 3 or 4, and copies a fixed
0x40 bytes into a 0x40 stack buffer. This agrees with the earlier 128-mutation
run. The updater's `read_hdr` (`FUN_00406fb0` in `upgrade`) is likewise fixed
size: exactly 0x10 bytes into the caller's 16-byte buffer, then magic
`0x35167f49` and version `0x11`/`0xffffffff` checks — no copies.

`/status` has no injection surface. The handler set is `zp`, `ifconfig`, `zl`
(empty), `enetports`, `VERSION` and `showstp` (4.4 KB of bridge state);
`tmp/upgrade.log` is registered only while an update runs. A ~50-word brute will
miss `zl`/`enetports`/`showstp`/`VERSION` — use this list. Arguments in `?…` are
ignored, `%20`/`;` are not re-split, and an unknown name returns 200 with an
empty `<ZPSupportInfo/>` rather than 404.

**`/status/showstp` stalls the status server on S2.** It answers 200 with only
~227 bytes, but takes 15.7 s / 3.2 s / 0.65 s on three consecutive calls and
each call blocks `/status/*` completely while it runs (a concurrent `/status/zp`
times out, then recovers). Nothing dies; it is a trivial stall lever. Warm, four
parallel `showstp` calls each returned in 0.5-0.8 s and the daemon stayed up, so
the 15 s figure is a cold-start cost, not serialisation.

**S2 gates the debug pages but not `/setstring`.** On 86.10 `/advconfig`,
`/diaglevel` and `/diagmsg` all answer 403 (they are open on 57.23) and
`/support/*` is 404 on both. `/setstring` is *open* on 86.10 and one POST kills
the daemon there too, faster than on S1 — measured down at 4 s after the "Setting
changed" 200. So the one-request kill is fleet-wide; only the debug helpers
differ by build.

## 2026-10-23: live 57.23 binaries recovered (model-12 key), verifier mapped

The model-12 legacy key was recovered from the plaintext 34.16-37101-1-12
updater (fixed wrapper at `0x19300`, system word `0x0000`, recipient
`e35f7c21…`) with the archive tooling, following the model-16 technique. It
decrypts the model-12 OTA family, which puts the **exact live 57.23 MIPS
`anacapad`, `upgrade` and their libraries on disk** (git-ignored
`captures/firmware/57.23-74170-1-12/`, with `57.19-46310-1-12` beside it for
diffs). Interoperability facts measured on those builds:

- 57.23 `upgrade` is a rewrite over `libsonoscrypto`, `libsonosrootcerts` and
  `libflash`; the record parser and cert-bundle parser live in those libraries
  (`sonosRcbParseHeader` / `sonosRcbParseManifest`, status strings "tampered
  bundle", "bad magic number").
- **Verification scheme (traced live, not inferred):** the type-17 ring is a
  key ring; a key reference of type 3 is the SHA-1 of an RSA public key in
  **SPKI DER** form, matched against keys compiled into `libsonoscrypto`; the
  signature is RSA-2048 PKCS#1 v1.5 over **SHA-256 of every file byte before
  the type-17 record** (captured digest at the call: 32 bytes, and reproduced
  byte-for-byte with `sha256(file[:type17_start])`). Ring grammar: count1/count2
  entry lists, then a key count and per-ref `u8 type`, two `u32` fields (second
  one is the key-material length, `<0x101`), key bytes.
- The type-22 certificate bundle is **plaintext** (`rcbundle` magic after the
  0x621da74d ring header; 132 DER certificates in the model-12 image). It is
  parsed before any signature result, so it is the pre-verification surface on
  the update path. 128 mutations of the bundle header and 128 of the signature
  ring (magic, lengths, counts, types) parsed without a crash; the obvious
  overflows are checked.
- `/status/tmp/upgrade.log` is a live read of the updater's own log, and the
  spawned updater's stdout is reachable there until it rotates. `BeginSoftwareUpdate`
  on an on-link unit plus that read is a complete remote oracle for the gate.
- A patched 57.23 `upgrade` (`0x407d7c`: NOP the compatibility branch) runs in
  the 34.16 VM with six RAM `mtdram` devices, `/tmp/bootsection`/`bootgeneration`
  and its own `ld.so.1` (`/opt/s57/lib/ld.so.1 --library-path …`), reaching the
  real verifier. Same-version images hit a "no decryption key" quirk; serve a
  bumped version (e.g. 99.1.1) instead.

Open: an inline-key reference (type 1/2) is accepted by the ring parser but has
not verified yet; if it verifies with an attacker key, an unsigned image is
accepted end-to-end. Also open: the exact ref field semantics for those types.

### Addendum: digest captured; inline refs still open

The verifier's hash was captured at the call site by patching the call
instruction to `break` (absolute breakpoints are not inserted when gdb debugs
`ld.so.1`, because the binary is still unmapped): the value is exactly
`sha256` of the file prefix before the type-17 record, reproduced offline. The
signature is PKCS#1 v1.5 over it. The ring reference format was corrected to
`[u32 key_len][u8 type][key bytes][u8 hash_alg][u32 1][u32 siglen][sig]`
(type 3 is a SHA-1 of an SPKI and matched the built-in key at
`libsonoscrypto+0xdd80`).

Inline references (types 1, 2, 4) parse, but nine combinations
(SPKI/PKCS#1/PEM × three types) with the now-correct digest all fail at
"Sonos signature verification failed", and no "signed with key N" is printed
for them, so the verifier does not resolve those to a key on the 57.23 MIPS
build. The open question is whether a reference type can select an
attacker-supplied certificate in the type-22 bundle; if it can, the image
self-signs and the update floor stops mattering.

### `.upd` container robustness, measured

The record loop tolerates far more than a real image needs:

- **Section count**: a file with 4 096 record 3s may be parsed end to end; 8 192
  also completes (131 KB, clean). 32 768 stops the read around 155 KB (≈9.7 k
  records) without a restart — a graceful cap between 8 k and 9.7 k.
- **Zero / short `total_len`**: `total_len` 0, 8 and 17 all parse benignly (the
  loop just consumes the remaining stream).
- **`total_len` far above the data**: `0x100000` (1 MB) and `0xfffffff0` (≈4 GB)
  both take the daemon down and the watchdog restarts it within ~5 s. One field
  is therefore a reliable pre-auth DoS; the record copy does not re-check the
  declared length against the bytes actually received.

### Read-through of the live 57.23 bundle parser (`libsonosrootcerts.so.1`)

The bundle path is the one pre-verification parser in the live build, so it got
a manual read (6.7 KB library, plain MIPS32, exported API
`sonosRcbParseHeader` / `sonosRcbParseCerts{,Copy}` / `sonosRcbParseManifest`).

It is defensively written and matches the earlier mutation result:

- `sonosRcbParseHeader` requires a `rcbundle` magic, then walks fixed fields with
  `ptr >= base`, `ptr < base+len`, and `len - field >= offset` checks before every
  copy; the 64-byte and 32-byte fields both re-check `len` first.
- `sonosRcbParseManifest` rejects a certificate count `>= 301` (`0x1004`,
  `sltiu 0x12d`) and `malloc(count*12)` for the entry array, so the loop counter
  and the allocation are the same quantity.
- The per-entry loop range-checks the entry length against the remaining buffer
  before passing `(ptr,len)` to `mbedtls_x509_crt_parse_der`, and re-checks the
  running offset each iteration.

The DER certificates themselves are handed to the bundled mbedtls 2.5.0, so the
remaining exposure is mbedtls's x509 parser rather than Sonos code.

### Self-signed image built end to end: the resolver does not read the bundle

With the type-17 grammar and digest fully known, a complete attacker-signed
image was built to test the last open question (does a key reference resolve
against the *bundle* the way it resolves against compiled-in keys?):

1. RSA-2048 key and self-signed certificate, SPKI DER, SHA-1 of that SPKI.
2. The bundle really is plaintext: `rcbundle\0`, a length-prefixed version, a
   big-endian `u16` count at `+0x6A`, then `u16 flags / u32 offset / u16 length`
   entries (offset relative to the `rcbundle` byte, DER data after the table).
   The new certificate is appended, its entry added, the count bumped and every
   existing offset shifted by the 8 bytes the new entry occupies.
3. The type-17 ring kept, with the digest field at `+0x1d`, the SHA-1 at `+0x4f`
   and the 256-byte signature at `+0x6c`; the version record bumped to 99.1.1 so
   success would be unmistakable. Signature = RSA PKCS#1 v1.5 over SHA-256 of
   every byte before the type-17 record (verified offline with `openssl dgst`).
4. Served it. The unit read **all 5,607,497 bytes** (so the version/patch gate
   passed and the modified bundle parsed), rebooted in ~10 s and came back on
   57.23 — i.e. it failed at verification.

Combined with the earlier inline-reference result, the conclusion is: **the
type-17 key resolver only matches keys compiled into `libsonoscrypto`; neither
inline key material nor an attacker certificate in the bundle is honoured.** A
modified image therefore needs the vendor key even though the format is fully
understood. The builder is `captures/research/selfsign/build.py` and is reusable
for any future attempt that supplies real key material.

## Runtime access (2026-10-22)

The boot path itself gives the cleanest persistence story: `/etc/Configure` executes
`/jffs/system/Configure.dev` as root and the same block sets the unlock flag that
`/etc/scripts/run_sshd.sh` waits on before starting dropbear. One file in writable flash therefore
yields root on every boot plus SSH, without the devmode statement. Live-verified unauth interfaces
that could supply such a write, the `:1443` UPnP proxy, the `/getaa` SMB ingress and the
`NumThreads 4` DoS are documented in [`RUNTIME_ACCESS.md`](RUNTIME_ACCESS.md).

### 2026-10-23 later: what the ring byte is, and anacapad notes

Clarified by live/differential runs: the ref byte after the length is accepted
for many values (0 invalid, 1–4 accepted, 5+ parse in some builds), and mutated
types still verify the genuine signature. It is therefore not a key *selector*
by itself: the 20-byte payload is still resolved against the compiled store (the
genuine model-16 fingerprint `d67d7a…` exists only in `libsonoscrypto`, not in
any of the bundle's 238 key encodings). Inline-key hypotheses are exhausted for
now; the bundle's role is at most a chain, not a key source.

Other measured negatives/positives:
- `/getrs` with no query string hangs (no response within 5 s); any unstastisfied
  query answers 200 with `Content-Length: 0` and dummy content headers, so the
  three `4d2e08` lookups are not the obvious names yet.
- A 1 MB POST to `/notify` times out at the client but leaves the daemon alive.
- The 57.23 `anacapad` will not start under qemu-user in the VM: it aborts
  (`SIGIOT`) after `MEMGETINFO` on `/dev/mtd0` returns `ENOTTY` and the flash/MDP
  init fails. `upgrade` is unaffected because it tolerates the failure. Local
  anacapad fuzzing therefore needs an emulated MDP partition table, not just a
  RAM mtd.

### H12 boot-execution path: confirmed in the rootfs, primitive still missing

The claim in bead `frankensonos-q57` is **real and better than stated** — two
independent boot scripts act on one file:

- `/etc/rc.d/rcS` (run 1, after `/jffs` is mounted): `if [ -f
  /jffs/system/Configure.dev ]; then sh /jffs/system/Configure.dev; fi` — i.e.
  arbitrary root commands, every boot.
- `/wifi/Configure` (run earlier by rcS): `if [ -f /etc/unlocked_build_flag -o -e
  /jffs/system/Configure.dev ]; then /usr/sbin/telnetd; touch
  /tmp/device_unlocked_flag; fi` — i.e. **mere existence** starts a root telnet
  shell on 2323 and sets the flag the updater's `allow_policy_bypass` keys on.

What is missing is a primitive that creates a file at a chosen path. Status:

- `/jffs/system` appears in **no** binary — only in those two shell scripts. The
  writable settings store is `/jffs/settings` (singular literal in `anacapad`;
  `RSystemSettingsBase` builds `<g_pszZPSettingsRoot>/<name>` with `snprintf`).
- `SystemProperties#SetString` **does persist** (values survive a full reboot) and
  it is reachable unauthenticated on 57.23 through both the WebSocket API and the
  `:1443` UPnP proxy (`input[]` of name/value). But it stores settings as
  key/value entries in a **file named by the class** (`localsettings.txt` and
  friends), not one file per key.
- Path traversal in the key is accepted (`SetString("../system/Configure.dev",
  script)` and `"../../system/..."` both return result 0 and read back after a
  reboot), and still does **not** produce `/jffs/system/Configure.dev`: no
  telnetd, `/unlock` still 423, `/reboot` still 403 after two boots.
  `SetString("Configure.dev", …)` likewise.

So the remaining task for H12 is exactly one thing: **a named-file write
primitive** (a class whose file name or path comes from the request), after which
root is automatic at the next boot. Candidates to audit: `sonos::SettingsFile`
constructors reached from a request, `SetZoneAttributes`' icon fetch/cache path,
and the diagnostics/report writers.

### `/notify` body: one unauthenticated 1 MB POST kills HTTP

Reproduced twice on the 57.23 Play:5: `POST /notify` with a 1 MB body
(`Content-Type: text/xml`, arbitrary bytes) gets no response and the daemon
stops answering on `:1400` (and `:1443`) until the watchdog restarts it, while
SSDP and audio continue. A later size sweep measured the threshold. This is a
one-request, pre-auth DoS from the GENA sink reader; it is the strongest
blocking surface found so far, ahead of the multi-thousand-request
`/diaglevel` flood.

### Correction: an install wipes `/jffs`, so the earlier Configure.dev test was confounded

The 57.23 preinstall script (decrypted with the model-12 key) does this in its
`umountjffs` phase:

```sh
rm -rf /tmp/jffs; mkdir /tmp/jffs
cp /jffs/localsettings.txt /tmp/jffs
cp /jffs/wifi.txt /tmp/jffs
cp /jffs/netsettings.txt /tmp/jffs
cp /jffs/onlineupdate-inprogress.txt /tmp/jffs
cp /jffs/irconfig.txt /tmp/jffs
umount /jffs
```

`/jffs` is then re-formatted and only those five files are copied back. So:

- **anything else put in `/jffs` — including `/jffs/system/Configure.dev` and
  `/jffs/Configure` — is destroyed by every update.** H12 only holds until the
  next install, and a test that sets the file and then reboots *by installing*
  measures nothing.
- the preserved set is the only place a pre-install persistent write could
  survive an update (`netsettings.txt`/`wifi.txt` are parsed by `netstartd`, the
  rest are plain settings).
- `/jffs/Configure` is `exec`'d by `/wifi/Configure`, so it would need the
  executable bit; `/jffs/system/Configure.dev` is `sh`'d, so mode `0644` is fine.
  A settings-store write would not carry a mode, which favours `Configure.dev`.

A non-OTA reboot is therefore required to test either path. `management/reboot`
is 403 pre-auth and `/reboot` is 403 while locked, so the practical reboot is the
watchdog, a crash, or leaving the device alone until it reboots itself.

Size sweep on the same unit: 64 KiB answers **405** (the handler wants a
different method/headers) and the daemon survives; **256 KiB and 512 KiB** both
time out and kill HTTP, like 1 MiB. The unit also stayed without `:1400` for
minutes after the largest attempts while ICMP and playback continued, so this is
a durable one-request control-surface outage, not a momentary reset.

### The non-destructive reboot primitive (needed for any `/jffs` test)

Because an accepted install reformats `/jffs`, any test of a file placed there
must reboot **without** completing an update. A rejected image does exactly that:

1. `BeginSoftwareUpdate` at an image the gate refuses (wrong patch train, or a
   version-bumped image with a bad signature).
2. The daemon reads ~130–160 KB, aborts, and the box reboots in ~18 s.
3. The install script never runs, so `/jffs` is untouched.

Contrast: an accepted image runs `umountjffs` (wipe) and takes ~50–60 s to come
back. So "short reboot, `/jffs` preserved" vs "long reboot, `/jffs` wiped" is also
a clean oracle for whether a candidate was accepted, independent of the version.

`tot-huge.upd` (`total_len = 0xfffffff0`) only restarts the *daemon*, not the
box, so it is not usable for this.

Cross-unit check on .168 (same 57.23 build): a clean 256 KiB POST also kills
HTTP, and the watchdog restarted the daemon in ~6 s. The long .185 outage came
after three hits in quick succession (256 KiB, 512 KiB, 1 MiB); treat
"durable" as "repeat hits outrun the supervisor", not a single request.

### H12 result after a clean test: SetString keys are not files

On a freshly reflashed unit (so no stale `/jffs`), `SetString("../system/Configure.dev",
<script>)` returns result 0 and reads back after a reboot, but the unit is still
locked afterwards (`/reboot` 403, `/unlock` 423, 2323 and 22 closed) — and the same
holds for a plain `Configure.dev` key. So the store keeps keys as *entries in a
class-named file* and does not turn the key, or `..` in the key, into a path.

`mtmhhsetup` (unauthenticated on S2) opens `netsettings.txt` through
`sonos::SettingsFile` — again a fixed name.

H12 therefore stands blocked on exactly one thing: a write primitive whose **file
name comes from the request**. The boot side needs nothing more (see above), and
`/jffs/system/Configure.dev` needs no executable bit because it is `sh`'d.

### WebSocket passthrough: argument shape, and what it is worth

The `upnp*`/`call` commands take `method` in the **options object** (element `[1]`)
and arguments as `input`, a list of `{name,value}` pairs on the command object,
with `playerId` present:

```
[{"namespace":"upnpSystemProperties","command":"call","householdId":…,"playerId":…},
 {"method":"SetString","input":[{"name":"VariableName","value":"k"},
                                {"name":"StringValue","value":"v"}]}]
```

That answers `result:0`, and `GetString` reads the value back (a missing key gives
UPnP `800`). A `params` map, a bare list, nested objects or command-level `params`
all fail with `402` on both generations — the earlier "params never reach SOAP"
result was a shape problem, not a plumbing gap.

Boundary by build: the passthrough is **57.23-only** over WS (the same frames give
`ERROR_UNSUPPORTED_COMMAND` on 86.10). `management/reboot` and
`management/factoryReset` answer 403 insufficient scope on 57.23 and
`ERROR_SERVICE_UNAVAILABLE` on 86.10, so there is still **no pre-auth reboot** and
no pre-auth mutation beyond `SetString`. `diagnostics/submitDiagnostics` and
`update/checkForUpdate` are accepted over WS on 86.10 (`update/check` is 404 on
57.23, as recorded earlier).

The handler is stricter than "big body": `POST /notify` with 64 KiB answers 405
(body read first, then the method test), but a **2 KiB body sent with the
`NOTIFY` method** (a plain `<e:propertyset>` wrapper with a 2 000-char property)
also gets no response and takes HTTP down. The two failures therefore differ:
one is a size limit before the method check, the other is reached only on the
GENA method itself. Both units used (`168`, `185`) stayed without `:1400` for
minutes; recovery is watchdog-driven and slow after repeat hits.

### H12 is live on 57.23, and the file is in the preserved set

Two corrections to the section above, from the decrypted 57.23 tree
(`libsonosutils.so.1`, `anacapad`):

- **57.23 knows the path.** Both binaries carry `system/Configure.dev`
  (`/jffs/system/Configure.dev` in `anacapad`), next to `devmode`,
  `unlockedBld`, `confdev`. It is not a 34.16-only relic.
- **It is a preserved file.** `libsonosutils` holds the `/jffs` table as
  `{u32 flags, char *relative_path}` pairs terminated by `0xffffffff`:
  `system/Configure.dev` is `0x1101`, i.e. file (`1`) + preserve (`0x100`) +
  `0x1000`; the same preserve bit is on `conf/anacapa.conf` and `regdevicecert`,
  while plain logs carry only `1`. The `umountjffs` copy list in the install
  script is therefore partial; the preserve table is what carries the file across
  an update. So once `/jffs/system/Configure.dev` exists it stays, which makes
  H12 a one-shot write rather than something to redo every boot.

Still missing is only the write primitive with a request-chosen name; nothing
about persistence or the boot side is.

### Unauthenticated `SetZoneAttributes`: the icon is stored verbatim, nothing fetches it

`DeviceProperties#SetZoneAttributes` is open on plain :1400 SOAP (`DesiredZoneName`,
`DesiredIcon`, `DesiredConfiguration`) and answers 200. `DesiredIcon` accepts an
arbitrary URL and is stored **as-is** — it shows up immediately in `/status/zp`'s
`<ZoneIcon>`, alongside the built-in `x-rincon-roomicon:<name>` form.

A URL icon is *not* fetched: with a byte-counting server bound on the LAN, setting
`http://<host>:<port>/icon.png` produced no request, before or after a boot. So the
icon is not a way to make the daemon fetch an attacker document, and it stores no
file.

The socket-level route is the only one here: DeviceProperties is absent from the
57.23 WebSocket command table (`ERROR_UNSUPPORTED_NAMESPACE`), though it works over
SOAP, so a "the proxy covers DeviceProperties" assumption is false on S1.

### The payload to drop, if a filename primitive is ever found

Keep it short, keep it idempotent, and make it survive its own mistakes — the file
is in the preserve table, so it runs on every boot. It is `sh`'d as root from rcS
and, because `/wifi/Configure` only tests for existence, the same file also
unlocks the unit and starts telnetd on its own:

```sh
#!/bin/sh
# /jffs/system/Configure.dev
/usr/sbin/telnetd -l /bin/sh -p 2323
/usr/sbin/dropbear -p 22 2>/dev/null
touch /tmp/device_unlocked_flag
```

`touch`-ing the flag is what the updater's `allow_policy_bypass` checks, so after
the first boot a modified `.upd` no longer needs the version/patch games. Remove
the file (or empty it) to fall back to a normal, locked player.

### VM anacapad bring-up: what the blocker actually is (2026-10-23)

Runnable detail for anyone continuing the qemu lab (all on `css`, guest `:2311`):

- The Debian 4.9 `mtd.ko` compiles `MEMGETINFO` as `0x40084d01` and rejects the
  compiler-independent ABI; the 57.23 `libflash` builds `0x40244d01` (it puts a
  36-byte `mtd_info_user` on the stack) and gets `ENOTTY`, which anacapad turns
  into `SIGIOT`. The 45.1 `libflash` uses `0x40204d01` and the stock
  `mtd.ko` **accepts** it (`ioctl(...0x40204d01) = 0`, verified against
  `/dev/mtdr0`). So the 57.23 library simply post-dates the guest kernel's MTD
  ABI. One word fixes it: patch `lui s6,0x4024` at libflash vaddr `0x1670` to
  `0x4020`; `MEMGETINFO` then succeeds for every probed device.
- `mtdram` registers at most **8** devices per kernel regardless of module name
  (`insmod` answers `File exists` past 8), so use nandsim (`parts=1` gives
  `mtd0`/`mtd1`) plus eight mtdram units and alias `/dev/mtd9`..`/dev/mtd15`
  onto existing minors; anacapad probes 0..15 and aborts on the first `ENOENT`.
- With the ABI patched the server reaches the DCMS block and then aborts in
  `dcm`: it loads `/jffs/regdevicecert/cert.xml`, `/jffs/regdevicecert/prod/cert.xml`
  and `/jffs/regdevicecert/prod/metadata.txt` (in that order, via
  `%s/%s` + `prod`), and on any problem logs
  `<dcm,1> regcert: loaded:%s (b:%d e:%s)` with the status word from
  `{SUCCESS,BAD_FILE,BAD_KEY,BAD_CERT,BAD_ISSUE_DATE,MISMATCH_ENV,MISMATCH_ISSUER,MISMATCH_HHID,MISMATCH_USER}`
  and raises `SIGABRT`. Creating the three files with a self-signed PEM (or DER)
  certificate and a plausible `metadata.txt` still yields `BAD_FILE (b:0 e:)`,
  so the metadata format is the remaining unknown; both files are read in full
  before the verdict, so it is a content test, not an open failure.
- `SIGABRT` comes from the shared fatal printer at `0x4a6d40` (its `jal abort` at
  `0x4a6d38`); replacing it with `b 0x4a6d30` (`jr ra`) keeps the process and
  port 1400 alive but the main thread has already left the accept loop, so the
  socket accepts nothing. Forcing the loader's return value to `SUCCESS`
  (`0x458e7c`) is not enough either — the request continues in the DCMS block.

### WS `subscribe`: plumbing found, delivery does not reach an HTTP callback

The 57.23 command table (recovered from the live binary) is short: `management/factoryReset`,
`settings/getPlayerSettings`, `settings/getPublicSettings`,
`settings/get{Protected,Restricted}AdminSettings`, `settings/set*`, and the
`upnpSystemProperties` / `upnpZoneGroupTopology` `call` + `subscribe` / `renew` /
`unsubscribe` families. `settings/getPlayerSettings` and `getPublicSettings`
answer unauthenticated; `management/factoryReset` is `ERROR_NO_PERMISSION`.

`subscribe` needs two values in the **options** object, `port` and `eventPath`,
and then answers `result:0` with a `logicalSID` and a subscription duration:

```
[{"namespace":"upnpSystemProperties","command":"subscribe",…,"playerId":…},
 {"port":19181,"eventPath":"/cb"}]
```

Delivery is still open: with a listener bound on that port and a `SetString` on
another connection, **nothing arrived** — neither an HTTP `NOTIFY` to the callback
nor a pushed frame on the subscribing WebSocket (5 s wait, twice). So a
subscription can be created pre-auth but its events do not reach a caller-supplied
callback in this configuration; treat the WS `subscribe` as bookkeeping until that
is understood.

### The wedged state is the HTTP accept path, not the daemon

`.164` (same firmware, never sent a `/notify` by us in this round) entered the
same state while being read only with `GET` for inventory: ICMP answered, TCP
connected to `:1400` (kernel completes the handshake from the listen backlog),
nothing accepted, and every request timed out. On that unit `:1443` was still
fully alive at the same moment: `GET /info` answered `200`,
`/api/v1/players/local/upnpSystemProperties?protocolVersion=1` answered `403`
(route present, scope/services missing) while `:1400/info` and `:1400/status/*`
were dead. So the crash leaves the process and its TLS/WS listener running and
kills only the plain-HTTP accept loop; the watchdog does not treat that as death
and takes many minutes (observed >35 min, three units at once) before it
restarts the daemon. That matches the lab: a `SIGABRT` that is turned into a
plain return leaves a process with `LISTEN` on `:1400` and no accept.

No pre-auth reboot exists (`management/reboot` answers 403, documented earlier),
and the WS handshake on `:1443` answers `400` to a well-formed RFC 6455 upgrade
on `/websocket/api` and `/api/websocket`, so a wedged unit cannot be poked back
over TLS either.

### How to recognise the missing primitive (audit checklist)

H12 needs one thing: a file write whose **name** comes from the request. Do not
re-tread the paths below — all are measured dead ends:

| Path | Why it cannot write a named file |
|---|---|
| `SystemProperties#SetString` (SOAP, WS, `:1443` proxy) | key/value entry in an encrypted, class-named settings file; `..` in the key does not change the opened path |
| `SetZoneAttributes` | name/icon/configuration into the same store; a URL icon is stored verbatim and never fetched |
| `mtmhhsetup` | fixed `netsettings.txt` |
| `/advconfig` | fixed `/jffs/netsettings_mod.txt`, two `atoi` integers |
| `/set-cr-netstart-dest-mac` | `/var/run/...`, tmpfs |
| `/raw`, `/reboot`, `/device_account/registration*` | all 403 while the unit is locked |
| `management/reboot`, `management/factoryReset` | `ERROR_NO_PERMISSION` (WS) |

A find looks like one of:

- a caller of `RSystemSettingsBase(RSystemPropertiesImpl*, u32, const char *name, …)`
  or `RSettingsFile(const char *)` / `sonos::SettingsFile(const char *)` where
  `name` is request data rather than a literal;
- a writer reached from a SOAP action or `:1443` route that takes a *path*
  argument (`REncryptedFileWriter::xcreate` is the only path-taking writer in
  `anacapad`; its two callers take fixed paths today);
- a route in the lock-gated family that becomes reachable only after `/unlock`
  — worth re-checking the moment a signature exists, but not a substitute.

Verification is then two requests and one boot: write the payload above, force the
short (rejected-image) reboot, then check 2323.

### Zone names are stored raw; escaping happens only at render time

Setting `DesiredZoneName` to `A<B&C"D` (XML-escaped in the SOAP body) stores the
literal characters — `/info` returns `"name":"A<B&C\""` and `/status/zp` renders it
as `A<![CDATA[<]]>B<![CDATA[&]]>C&quot;D`. So the value is persisted unescaped and
the renderer escapes per output format.

That makes the settings file itself the place where a crafted name could matter
(entries are written as `<Key …>value</Key>` with the value inserted raw). The
zone name sits behind a fixed key, so it cannot change *which* key is written, but
a value containing the entry delimiters is the obvious next injection to try.
Confirmed unchanged: `/advconfig` still handles exactly `FirstZP` and
`PriorityBridge`, both `atoi`-reduced, into `%s/netsettings_mod.txt`.

### Settings entries: exact format, and why key injection does not work

The writer format is in `anacapad` as a literal:

```
<%s LastUpdateDevice="%s" Version="0">\n%s</%s>\n
```

key, device id, **value inserted raw**, key again. No escaping of the value.

Turning that into an extra key needs a newline in the value, and the value is
forced to one line: both the SOAP path (`&#10;` entities) and the WS proxy path
(real `\n` in the JSON string) store `a b c` for `a\nb\nc`. Sending
`x</probe1><inj2 …>9</inj2><probe1 …>` therefore stays inside the value: SOAP
`GetString` returns it verbatim (`x &lt;/probe1&gt; &lt;inj2 …&gt; …`), and after a
reboot the injected keys (`inj1`, `inj2`) still answer UPnP `800`.

Two useful by-products: plain `SystemProperties#GetString` **returns the value**
(the `:1443` proxy does not — it only reports `result`), and a value carrying
markup is stored raw, so the settings file can be made non-well-formed without any
visible harm.

### H4 measured: an SSDP reply alone does not make a player fetch a description

A responder on 1900 answered a player's own `M-SEARCH` (`ST
urn:schemas-upnp-org:device:ZonePlayer:1`, then `…:service:ContentDirectory:1`)
with a well-formed `200 OK` whose `LOCATION` pointed at a LAN HTTP sink, `ST`
echoed, `USN` a fresh UUID, plus `X-RINCON-HOUSEHOLD` set to the player's own
household. The player queries repeatedly (also from port 1901 for
ContentDirectory) and **never fetched the description** — not on a normal boot,
not on a forced soft reboot, not with the household header.

Control: a generic UPnP client on the same collision domain (the phone/desktop
Spotify app) *did* fetch `/xml/device_description.xml` from the same sink and sent
`User-Agent: Spotify/…`, so the reply is well formed and accepted by a stock
client. Sonos players are simply stricter: a device that is not already in their
topology does not get its `LOCATION` read.

Consequence for H4: triggering the device-description parser from a cold device
needs a topology foothold first (household membership), not just a forged SSDP
answer. That is the same wall as the mDNS negative.

### There is a JSON settings writer, but it is behind the registration flow

The 57.23 `anacapad` carries `handleRestrictedSettingsWrite`, which maintains
`settings/netsettings.json` and `netsettings.json.tmp` and reports
`json parse error %d, %s`; it sits next to the `device_account/registration`
code (`regState`, `customerId`, `macAddr`). So the firmware does have a
request-driven **file write** — just of a fixed name inside the settings
directory, and reached from registration.

Routes probed for it on a locked unit, all on :1400: `GET`/`POST`
`/settings`, `/netsettings`, `/syssettings`, `/regcert`,
`/settings/netsettings.json`. Every one answers 404 or 405; `/settings` is a
consistent 405 for POST. So none of them writes anything while the unit is
locked. (A run of `000`s during this looked like a crash but was the unit
mid-auto-update — checked with a before/POST/after triplet, which gives
200/405/200.)

One more protocol note: this bench unit keeps *landing back* on `57.22-67080`
after every deliberate `57.23` push. The cloud offers an update it accepts each
boot; expect the version to revert and re-check before measuring.

### Recovery from the wedged `:1400` state (measured 2026-10-23)

The wedged state survives a reboot of the *service*: `advconfig` writes restart the
TLS listener but `:1400` stays accepting-less. Only a **device reboot** clears it.
The reachable reboot lever is the updater, driven over `:1443`:

1. The TLS listener on `:1443` serves the whole HTTP surface for the routes in its
   allowlist (`/status/*`, `/xml/*`, `/notify`, `/getrs`, `/diaglevel`, `/advconfig`,
   `/api/*`); `/`, `/reboot` and `/jobs` answer 403 there, so the job page cannot be
   used.
2. Trigger `BeginSoftwareUpdate` through the WS passthrough instead of `:1400`:
   handshake `wss://<ip>:1443/websocket/api`, header
   `X-Sonos-Api-Key: 123e4567-e89b-12d3-a456-426655440000`, subprotocol
   `v1.api.smartspeaker.audio`, frame
   `[{"namespace":"upnpZoneGroupTopology","command":"call","householdId":…,"playerId":…},{"method":"BeginSoftwareUpdate","input":[{"name":"UpdateURL","value":…}, {"name":"Flags","value":"0"}]}]`.
   `result:0` comes back and the unit fetches the image.
3. **`Flags` is not critical** (0 and 1 both worked); the first attempt failed only because nothing was fetched within the observation window. Keep `Flags=0`. With `Flags=1` plus an empty
   `ExtraOptions` the call also answers `0`, but nothing is fetched and nothing
   reboots. With `Flags=0` the download starts within seconds (`ESTABLISHED` to the
   server, 5.6 MB image).
4. A genuinely signed older image (57.22-67080, above the 56 `MinCompatibleVersion`)
   is accepted; the unit downloads it, reboots twice, and comes back with `:1400`
   answering again. Restoring 57.23 afterwards uses the same trigger with the signed
   57.23-74170-1-16 image.

`Flags=0` + the signed 57.22 image was used on `.185` after ~2 h of a wedged `:1400`;
the port answered again on the downgraded build, so the procedure is the working
recovery path for a wedged unit, independent of the `/notify` fault.

### LAN-position plays (owner controls the network) — tested 2026-10-23

Owning the LAN was supposed to open MITM-shaped routes, so the three cheapest were
measured. Two are dead, one gives a useful map:

- **Album art is not fetched by a screenless S1.** `SetAVTransportURI` with an
  `upnp:albumArtURI` on our server and a playable `res` on the same host: the player fetched
  the audio twice and the JPEG zero times. Artwork is the controller's job on this class.
  Re-test only on a unit with a display (S2 One/Move) before calling it dead generally.
- **Players do not fetch a description from an M-SEARCH answer.** They *do* search: a 60 s
  passive listen collected 23 `ZonePlayer:1`/`ContentDirectory` M-SEARCHes from .5/.6 players
  (plus DIAL from non-Sonos hosts), which means the vector exists. Replying unicast with a
  `ZonePlayer:1` 200, a matching USN and a LOCATION on our server got 23 answers out and
  **zero description fetches**. The household topology the players already hold is
  authoritative, so a stranger's LOCATION is ignored. (Same conclusion the earlier NOTIFY
  probe reached from the other direction.) Reaching the description parser therefore needs a
  rogue device that is *accepted into the topology*, not just advertised.
- **`OnlineUpdateBaseURL` is not a SetString key** — the POST answers 400 for it, while
  arbitrary keys 200. It is a replicated setting owned by `/testenv` (the page carries an
  `OnlineUpdateBaseURL override` field and a 120 s replication warning), and `/testenv` is
  itself behind the devmode flag. So redirecting the *manifest* fetch at the daemon is
  gated on devmode too.

Net: controlling the LAN buys observability and the HLS/media fetches, not a new
pre-authentication parser on S1.

### The live 57.23 daemon has exactly seven stack-`strcpy` sites

A bounded audit of the *running* build is only possible now that the binaries are
decrypted. Sizing it first: `anacapad` imports `strcpy`, `sprintf`, `sscanf`,
`memcpy` (plus its own `snprintf_append`), and there are 37 `strcpy` call sites
in total — but only **seven** copy into a stack destination, which is the set that
can smash a frame:

| site | source | first read |
|---|---|---|
| `0x41b124` | `s3+161` into `sp+76` | fills a `sockaddr_un`-shaped area (`sun_path`, ~104 bytes) — unbounded copy into a fixed path field |
| `0x43f090` | `sp+24` into `s0+6108` | object field, source is a stack buffer |
| `0x4548f4` | into `s3` | after a successful `rename`, destination is a pointer not a frame slot |
| `0x47aa8c` | string returned by `0x4cb3e0` | into `sp+160` in a 360-byte frame, beside another buffer at `sp+227` |
| `0x47aa98` | same shape | second copy in the same function |
| `0x47add4` | `s0+99` | into `sp+24` in a 200-byte frame; reads `*(s0+356)` nearby |
| `0x4cf3e0` | `ioctl(SIOCGIFCONF)` data | interface names, kernel-bounded |

The three to read properly are `0x47aa8c`/`0x47aa98` (string-returning call into a
frame), `0x47add4` and `0x41b124`; the rest are either kernel data or
caller-supplied pointers. None is proven request-driven yet, and `sprintf` (24
sites) is the next block if these are clean.

### The `0x41b124` copy is in the OAuth account-add path, which is reachable unauthenticated

The function containing it is `AddOAuthAccountX`'s handler (frame 184 at
`0x41ad84`; its own strings are `AddOAuthAccountX`, `untX`, `untToken`). It builds
a file path with `snprintf`, calls `sonosFileReadInt`, and then

```
strcpy(sp+76, s3+161)        ; s3 = a0 from 0x41b0c8, field at +161
```

— an unbounded copy into a path-sized field near the top of a 184-byte frame, from
an object field rather than a literal, so the length depends on whatever filled
`+161`.

`AddOAuthAccountX` answers **unauthenticated** (500 + UPnP 402, i.e. it parses and
rejects, rather than 403), so this path is drivable from the LAN. Getting past the
402 is the blocker: `AccountType` 2311 with plausible token/key/device-id lengths
(120 / 32 / 32) still 402s in 0.27 s, so the validation is local and shape-specific
— the next step is to recover the accepted shape from the handler (`0x41ad84`),
not to guess. `AccountType`/`AccountToken`/`AccountKey`/`OAuthDeviceID`/
`AuthorizationCode`/`RedirectURI` are the six arguments.

### `/notify`: the fault is a missing read-terminator, not a crash

A clean, freshly booted 57.23 (`.185` after the recovery in the previous section)
re-runs the matrix: `NOTIFY` empty → 400 alive, `NOTIFY` 2 KiB plain → 400 alive,
`NOTIFY` 2 KiB propertyset → 400 alive, `POST` propertyset → 405 alive, 256 KiB
`POST` → no response but the port keeps serving. So nothing *crashes*; requests
that carry a body can **hang the worker**.

The reader stops only on a short `read()`. It never consults `Content-Length`, and
a `0`-byte read (client half-close) does not end the loop either, so a request whose
body is delivered in full-size fragments loops and then blocks after the client's
last byte; the connection stays open and the handling thread is gone for good.

Measured on the clean unit with plain bodies:

| body | result |
|---|---|
| 1 000–2 500 B | 405 (fast) |
| 3 072 B, 4 096 B, 5 120 B, 6 000 B, 20 000 B, 64 KiB, 256 KiB | usually no response |
| same 20 000 B at `--limit-rate 1k` (1 KiB fragments) | 405 — a short first read stops the loop |
| same 20 000 B at `--limit-rate 4k` (4 KiB fragments) | no response |

The threshold is not crisp because it depends on how the bytes arrive, which is what
the fragment test shows: the buffer is a couple of KiB, and any body that is
delivered in full-buffer reads hangs. Concurrency then does the rest: 40 parallel
`POST /notify` with 256 KiB took ordinary `GET /status/VERSION` down from 10/10 to
4/12 and then 0/12; the port reopened as the hung sockets were reaped, but a thread
that loops on a closed socket stays blocked. That is the mechanism behind the
30-minute outages, and why only a restart (device reboot) clears it.

### The account helper's 16-byte field, and a live 200 from `AddAccountX`

`0x41b0c0` (136-byte frame) builds a socket address and then does
`strcpy(sp+76, s3+161)`: the memset only covers `sp+24..sp+92`, the family word is
written at `sp+92`, so the destination field is about **16 bytes**, and saved
registers start at `sp+116` with `ra` at `sp+132`. Anything over ~15 characters
writes past the field; ~56 characters reaches `ra`. It is called from three sites
(`0x41b27c`, `0x41b3b8`, `0x41b674`) with an object whose `+161` field is used
elsewhere as a filename component.

`AddAccountX` (`AccountType`, `AccountID`, `AccountPassword`, `AccountUDN`) answered
**200 unauthenticated** on the unit — the account-add path is drivable from the LAN.

Reading caution for whoever continues: after the first success, every subsequent
`AddAccountX` answers 402, as does `RemoveAccount` with only `AccountType`. That
looks like "account already exists"/missing-argument rather than a length check, so
do not read the 402s as a cap on `AccountID` — the sequence has to be
remove-then-add with the right arguments before length can be bisected.

### `AddAccountX` argument shape: one-character `AccountID` only, for now

Reproduced on two units, one of them freshly booted and account-free:
`AddAccountX(2311, AccountID="1", …)` answers 200, and `"12"`, `"1234567890"`,
`"Z"*60` all answer 402. The same id with a 60-character `AccountPassword` also
402s, while a byte-identical repeat of the working tuple keeps returning 200
(idempotent re-add). `RemoveAccount(2311, "1")` is 806 throughout, so the stored
key is not the plain id.

So the handler is shape-strict and the guesses are not the shapes, which means the
`strcpy` into the ~16-byte field is not yet shown to be reachable with an
overflow. The next step is to take the real argument shapes from controller traffic
for an actual account add instead of guessing, then re-run the length series.

### Two caveats on H13's premise, worth settling before more device time

1. **`AddAccountX` may not be the path the app uses.** The S1 controller carries
   `RUpnpSPAddAccountX` / `SCOpAddAccountX` / `SCIOpAddServiceAccount`, and for a
   real music service the `AccountID`/`AccountPassword` values come back from the
   service (SMAPI), so they are long — while the daemon accepts a one-character id
   today. Either the daemon's check is on a different field than assumed, or the
   real flow goes through `AddOAuthAccountX`. The controller binary is the place to
   read the actual argument shapes from.
2. **Offset `+161` has no direct store in the daemon.** Searching the whole
   disassembly for stores at `161(reg)` finds none, so that field is filled by a
   bulk copy, sits inside a larger object, or is an inline string buffer. The
   16-byte size used so far is derived from the helper's frame layout (memset to
   `sp+92`, family word written at `sp+92`, `ra` at `sp+132`), not from the field's
   own declaration — the *overflow* arithmetic is sound, the *field-size* claim is
   an inference.

Both are cheap to check against the controller and the decrypted binary; neither
needs the device.

### Correction: the helper belongs to `AddOAuthAccountX`, not `AddAccountX`

Re-reading the three call sites (`0x41b27c`, `0x41b3b8`, `0x41b674`) against their
strings: all three sit in the `AddOAuthAccountX` family (`0x5186D8` /
"AddOAuthAccountX"). So the `strcpy(sp+76, obj+161)` path is exercised by the
**OAuth** action — the one that has 402'd for every shape tried, whereas
`AddAccountX` is the plain one that 200s with a one-character id.

That also explains the shape-strictness: OAuth account adds are **stateful** in
Sonos. The flow is `GetWebCode` → the user authorises at the service → the client
comes back with a code, and `AccountKey` is provisioned material, not something a
caller can invent. So `AddOAuthAccountX` almost certainly needs a prior step's
output (or a real service entry) before it will accept arguments, which is why six
guessed shapes all land on 402.

Consequence: H13 has two viable routes rather than one — either drive
`AddOAuthAccountX` after a `GetWebCode` (needs the returned code), or read the real
values out of controller traffic for a genuine add. Guessing the values is done.

### Spotify is service id 9, and the service list is readable and pushable pre-auth

`MusicServices#ListAvailableServices` answers **unauthenticated** and returns the
whole 102-entry descriptor XML with `Id`, `Name`, `Uri` and `SecureUri` per
service. **Spotify is `Id="9"`** (a second, `Id="12"`, is also named Spotify).
Every earlier account probe used 2311, which is not in this list at all — that
alone explains a good share of the 402s.

`UpdateAvailableServices` also accepts a caller-supplied descriptor list
(`AvailableServiceDescriptorList`) unauthenticated and answers 200, but a minimal
custom `<Service Id="999" …>` entry did not survive: the following
`ListAvailableServices` still showed the original 102 and no entry 999. So the call
is accepted and the payload is normalised away — it needs the real descriptor shape
(or the companion `AvailableServiceTypeList`) before a service we host can be
registered, which is the interesting goal because the device would then talk SMAPI
to a URI we choose.

With the right service id the OAuth path still does not open:
`GetWebCode(9)` → 800, `GetSessionId(9)` → 806, `AddOAuthAccountX(9, …)` → 402. So
it needs genuine session state, not just the correct type.

### `UpdateAvailableServices` is inert, and harmless to try

Replaying the device's own 102-entry descriptor back through
`UpdateAvailableServices` — verbatim, with an extra `<Service Id="999" …>` appended,
with `AvailableServiceTypeList` alongside, and both entity-escaped and raw —
always answers 200 and always leaves the in-memory list **empty** for the rest of
that boot: `ListAvailableServices` then reports 0 services and no 999, in every
variant.

A soft reboot restores all 102 (so the payload never becomes the stored state, and
the device rebuilds the list on boot). Practical upshot: the call is safe to
experiment with on a house player — worst case the service list is empty until the
next boot, which a rejected-image soft reboot fixes in under a minute.

So registering a service we host is still open, and the payload shape is not the
one the SCPD implies (it declares *no* arguments at all). Getting it right is worth
it: a registered entry makes the device speak SMAPI to a URI we choose.

### What a real service entry looks like, and why the replay still fails

Taken from the device's own list, a service element is:

```
<Service Id="294" Name="Radio Javan" Version="1.1"
         Uri="https://rjsonos.app/musicservice" SecureUri="https://…"
         ContainerType="MService" Capabilities="29889025"/>
```

`ContainerType` is `MService` (not `MusicService`), and `Capabilities` is a 32-bit
mask, not `0`. With those copied onto our own entry (id 999, URI at a LAN sink), a
single-entry delta push still answers 200 and `GetSessionId(999)` still answers 806,
so the action neither registers the service in RAM nor makes the device talk to the
URI.

Status: `UpdateAvailableServices` is accepted and effectively inert whichever way
it is driven — full list, delta, escaped, raw, correct attribute shape. The device
rebuilds its list from elsewhere on boot. The remaining source of truth is the
controller's own payload (or the on-disk service file the handler would write),
not the SCPD, which declares no arguments at all.

### The device SCPD is stale; the controller confirms both arguments exist

The S1 controller carries `RUpnpMSDUpdateAvailableServices` and the literal pair
`AvailableServiceDescriptorList` / `AvailableServiceTypeList` (alongside
`ListAvailableServices`). So the action really takes both, and the argument-less
declaration in the device's own `MusicServices1.xml` is simply out of date — the
same staleness that hid the true service ids.

Neither name is a `SystemProperties` key either (`GetString` on both, and on
`AvailableServices`, is 800), so the list is not readable or writable that way.

Consequence: the remaining unknown is only the *content* contract — the device
accepted every payload shape and applied none, including its own list replayed
verbatim. Reading the controller's payload builder (or the on-disk service file)
is the way in, and it needs no device access.

### H13 status block (the account path)

Target: `strcpy(sp+76, obj+161)` in `0x41b0c0`, reached from the
`AddOAuthAccountX` family. Frame arithmetic is sound (memset to `sp+92`, family at
`sp+92`, `ra` at `sp+132`), but the copy has never been fed a long value.

Measured dead ends — do not re-spend:

| attempt | result |
|---|---|
| `AddAccountX` with any id but `"1"`, any length, alpha or numeric | 402 |
| `AddAccountX` same id, 60-char password | 402 |
| `AddOAuthAccountX` with 6 args, plausible token/key/device-id/length shapes, ids 2311 / 9 | 402 |
| `GetWebCode(9)` / `GetSessionId(9)` | 800 / 806 |

Live leads, in order:

1. the controller's payload builder for `AddOAuthAccountX` (`RUpnpSPAddAccountX`)
   and for `UpdateAvailableServices` (`RUpnpMSDUpdateAvailableServices`) — both are
   in the app binary and both replace guesswork with a replay;
2. the object behind the three helper callers (`0x41b27c`, `0x41b3b8`, `0x41b674`)
   to settle what `+161` actually is (no direct store exists for that offset);
3. the remaining stack-`strcpy` sites `0x47add4` and the `0x47aa8c`/`0x47aa98`
   pair, then the 24 `sprintf` sites.

Read `MusicServices#ListAvailableServices` first on any unit: it is unauthenticated
and gives the true service ids (Spotify 9/12), which several earlier attempts got
wrong.

#### Relaunching the css exec VM (after a css restart)

`/opt/s57` and the guest `/tmp` do not survive. The working launch is:

```
pkill -f qemu-system-mips; sleep 2
cd /tmp/s1emu
nohup qemu-system-mips -M malta -m 256 -kernel vmlinux49 -initrd initrd49e.gz \
  -drive file=debian_mips_run4.qcow2,format=qcow2,if=ide \
  -append "root=/dev/sda1 console=ttyS0" \
  -net nic,model=e1000 -net user,hostfwd=tcp::2311-:22 -nographic > rN.out 2>&1
```

`initrd49e.gz` is the only initrd with the NIC modules (`initrd49d` boots with no
`eth0`). Then, in the guest (root/root, port 2311):

- `/opt/s57` is rebuilt from the host with
  `tar -C <57.23-…-1-12 tree> -cz bin lib | ssh … "tar -C /opt/s57 -xz"`, followed by
  the **full rootfs `/lib`** (the updater needs `libmbedtls`/`libmbedcrypto`) and
  `chmod +x /opt/s57/lib/ld.so.1` (tar restores 0644 and the loader then fails with
  `exec: Permission denied`).
- `/VERSION`, `/LEGACYCOMPATVER`, `/MINCOMPATVER` must be created from the rootfs
  (`57.23-74170`, `36.0-00000`, `56.0-00000`) or the compat block compares against
  a bogus default.
- mtd: `mtd.ko`, the `nand*`/`crc`/`bch` modules, `nandsim.ko parts=1`, eight renamed
  `mtdram` copies, `/dev/mtd0..15` via `mknod c 90 2n`, `/dev/mtdr0`, the 576-byte
  MDP written to `mtd0` with `bs=2048 conv=sync`, `/tmp/bootsection=0`,
  `/tmp/bootgeneration=1`.
- The unpatched `upgrade` still fails compat on the stub MDP (`Failed compatibility
  check`); copy it and zero the `beqz` at file offset `0x7d7c` for local runs. It then
  reaches `no decryption key (48)` because the stub carries no model keyring.

### Verdict on `UpdateAvailableServices`: accepted, never applied

Eight payload variants, every one answered 200 and every one left the device
unchanged (`ListAvailableServices` empty for that boot, `GetSessionId(999)` 806, no
request to a LAN SMAPI sink):

1. full 102-entry list replayed verbatim, entity-escaped
2. same plus an extra entry
3. same plus `AvailableServiceTypeList`
4. same, descriptor raw instead of escaped
5. single self-closing entry with `ContainerType="MService"`
6. single entry plus `Policy` + `Manifest` children (the real entries are *not*
   self-closing — they carry `<Policy Auth=… PollInterval=…/>` and
   `<Manifest Version=… Uri=…/>`)
7. 6 plus a matching type list of just `999`
8. 7 plus `AvailableServiceListVersion` at 2, 9999 and 999999

After a reboot the list is the original 102 with no `999`, so the cloud's boot-time
fetch wins over anything this action stores. Nothing about the call is harmful.

Best current explanation: the action is storage-only and the device re-reads the
service list from the cloud on boot, and/or it needs the cloud's exact payload
(including per-service manifests) to consider the list valid. Registering a service
that makes the device speak SMAPI to our URI therefore needs either the exact cloud
artifact or a device kept offline across the boot.

### The `sprintf` block is low-yield, and SUBSCRIBE has two traps

All 24 `sprintf` call sites resolve to numeric formats (`%u`, `%d`, `%x`, `%zd`,
`%u.%u.%u.%u`, `SID`, `HTTP/1.1 %d `, static date formats). The only `%s` formats
are the HTTP client's `"%s:%u"` (`0x4235b8`, `0x4237d4`), which write into
`obj+6117` from a string pointer at `obj+6088` — a host:port builder. Everything
else is bounded by the specifier.

That host comes from a GENA callback in the reachable path, and **SUBSCRIBE only
accepts a literal IP** there: `http://192.168.4.160:19181/cb` answers 200, while
`http://aaaa:9999/cb` and even a 20-character dotted-less name answer 412. So the
`%s` is at most 15 characters in that path and the builder is not the smash.

Two traps worth recording, both hit here:

- **the method is `SUBSCRIBE`, not `POST`.** `curl -X POST` to an `/Event` path
  gives 501 with an empty body on every event endpoint; `curl -X SUBSCRIBE` gives
  200. Several earlier "NOTIFY/SUBSCRIBE" results that looked like refusals may
  just have been POSTs.
- a hostname callback is rejected with 412, so a LAN callback must be an IP
  literal.

Net: the whole `sprintf` block can be closed off; the remaining S1 copy surface is
the two `strcpy` sites at `0x47aa8c`/`0x47aa98`/`0x47add4` already listed.

### H12 write side confirmed: the live binary only `stat`s the file

In 57.23 `anacapad` the string `/jffs/system/Configure.dev` has exactly **two** code
references, both in one status/report builder around `0x4502e0`/`0x4503b4`:

- `0x4503b4` calls `sonosFileExists("/jffs/system/Configure.dev")` — a pure
  existence probe whose result is turned into a string with the append helpers
  (`0x499664` / `0x499688`) and handed to a report builder at `0x49afe8`;
- `0x4502e0` composes the neighbouring field.

Neither opens it for writing, and the string is absent from `libsonosutils`' write
set except as a preserve-table entry. So nothing in the running daemon can create
`/jffs/system/Configure.dev`; it is placed externally (by a dev build, by an admin,
or by whatever primitive we find). That is the clean, final shape of H12: **find a
write with a chosen name, then the boot chain does the rest**.

The composed status string is not surfaced by `/support/review` or `/status/zp`
(grepped both for `confdev`/`configure` — absent), so it is a report/telemetry
field, not a remote oracle.

### S2 local API: a generic pre-auth UPnP executor (2026-10-23)

The compiled command table at `0x26a002` namespaces the UPnP passthrough, and REST does the same
thing over HTTPS with only the published static key:

```
POST /api/v1/players/{playerId}/upnp<Service>
X-Sonos-Api-Key: 123e4567-e89b-12d3-a456-426655440000
{"method":"<Action>","input":[{"name":"<arg>","value":"<v>"}]}
-> {"_objectType":"upnpResponse","parameters":[…],"result":0}
```

Verified live on 86.10-81100: `SetString` then `GetString` round-tripped a value through
`upnpSystemProperties`, `GetZoneAttributes` answered through `upnpDeviceProperties`, and — the one that
matters — **`ZoneGroupTopology#BeginSoftwareUpdate` was accepted and the player fetched our file**.

That is the useful asymmetry: on S2 the native `update/beginSoftwareUpdate` and `management/reboot` are
`insufficient_scope` (403) with the static key, but the *passthrough* reaches the same update path with no
token, no CSRF page and no SOAP envelope. `GET …/settings` is free too (playerUI/playerBasic), `POST`/`PUT`
on `settings/updateAllSettings` is 405, so settings are written via the passthrough, not the native verb.

Practical notes: `playerId` must be the device's own UDN (`/status/zp`); GET on a passthrough path is 405;
namespaces seen are `upnpDeviceProperties`, `upnpSystemProperties`, `upnpZoneGroupTopology` (call/subscribe/
renew/unsubscribe). Absent on S1 57.23, whose local API is read-only.

Pre-auth write inventory through the passthrough, all exercised live: `SystemProperties#SetString` (arbitrary
keys), `DeviceProperties#SetZoneAttributes` (rename/icon/configuration, `result` 0), and
`ZoneGroupTopology#BeginSoftwareUpdate`. Still gated: `management/reboot`, `factoryReset`, and
`/api/v1/devices/{deviceId}/householdUpdate/update` (403 `insufficient_scope` for `{}` and for both
`updateUrl`/`url` bodies). `OnlineUpdateBaseURL` is not a SystemProperties key on S2 either (SetString
812, GetString 800), so the manifest origin cannot be redirected by setting alone there.

`ContentDirectory#CreateObject` is not the queue-add path on 57.23: at `/MediaServer/ContentDirectory/Control`
(the bare path 405s) it faults **713** for every DIDL shape tried, while `AddURIToQueue` is what writes the
queue and its `savedqueues.rsq` store.

### Unauthenticated surface map, 86.10-81100 (2026-10-23)

The household now runs **81100**, not the 80260 the local artifacts came from, so the map was taken
live over HTTPS 1443: `200` — `/info`, `/status`, `/support/review`, `/support/directsubmit`, `/devmode`;
`405` — `/mtmhhsetup` (POST is the live verb); `423` — `/unlock`; `401` — `/getsetting`, `/testpoint`,
`/support/networkmatrix`; `403` — `/`, `/support/aggregate`, `/testenv`, `/advconfig`, `/fcs`,
`/diagmsg`, `/logger`, `/jobs`, `/reset`, `/ssh/fingerprints`, `/raw`; `404` — `/getrs`.

Differences from 57.23: `/advconfig` and `/getrs` are closed on S2, `/support/aggregate` likewise, and
`/support/networkmatrix`, `/fcs`, `/logger`, `/ssh/fingerprints`, `/raw` are names S1 does not have.
Adds nothing pre-auth beyond what is listed, but it is the starting set for an instruction-level diff
between 80260 and 81100 — the piece still missing from that comparison.

### Strategic conclusion: H12 is not a pre-auth path on S1

Putting the pieces together:

- no code in the running daemon writes `/jffs/system/Configure.dev` — the string has
  two references, both `sonosFileExists`/status composition;
- every write surface that *is* reachable pre-auth produces a fixed filename — the
  settings store (`/jffs/settings/<class>`, encrypted), `/advconfig`
  (`/jffs/netsettings_mod.txt`, two integers), `/set-cr-netstart-dest-mac`
  (`/var/run`, tmpfs);
- the surfaces that could emit a *named* file are all 403 while the unit is locked:
  `handleRestrictedSettingsWrite` behind `device_account/registration`, `/raw`,
  `/device_account/*`.

So on a locked S1 player, H12 cannot be reached at all — it is an **after-root (or
after-unlock) persistence mechanism**, not an entry vector. Its value is that it is
the *best* persistence available: `/jffs/system/Configure.dev` is in the jffs
preserve table (`0x1101`), so unlike the tmpfs flag it survives firmware updates and
re-establishes telnetd, dropbear and the updater bypass on every boot.

Allocation of effort follows: on S1 the entry vector is the vendor key or a memory
bug (H13 and the media path); H12 is what to install once one of them lands. On S2
the same file may be *pre-auth* reachable, because `/raw` exists there and its route
family is richer — worth a check on `.112` before assuming the S1 conclusion holds.

Checked, and S2 does not break the pattern: `/raw` is 403 on a live 86.10 player for
`/raw`, `/raw?filename=…`, `/raw/<path>`, `/raw/system/Configure.dev` and
`POST /raw`. So the raw-file interface is auth-gated on both generations, and the
strategic conclusion above holds for S2 as well — H12 stays an after-root
persistence mechanism, not an entry vector, on this fleet.

### Where the 402 comes from (and where it does not)

`0x41ad84` — the function holding the copy — contains **no** references to
`AccountToken` / `AccountKey` / `AccountType`, so it is a *sub-function* of the
`AddOAuthAccountX` handler; the name-driven argument parsing happens earlier in the
same handler. The parameter capacities visible near the copy are `128`, `128`, `4`
(so three of the arguments are bounded at 128 bytes or less, and one at 4).

There is **no literal `402` anywhere in the handler**, so the error is raised by the
`RCRInParam` layer before the handler's own logic — i.e. a name/order/capacity
mismatch, not a value judgement. Six value shapes (short, 120-char token, 32-hex
key, 32-hex device id, one-argument, three-argument, ids 9/12/2311/2049/2548/
5198492) all land on it, so the parameter *names* or *order* are what differs from
the SCPD, not the data.

That re-frames the remaining work: recover the handler's name table (it is in the
binary, referenced five times per name, just not in this function) rather than
fuzzing more values.

### The argument capacities are generous — order/names are the only variable left

Reading the parameter fetches in the `AddOAuthAccountX` handler: each is
`lui a1,0x52; addiu a1,…` followed by a `li a2,<cap>`, and the caps are

```
64, 0x802 (2050), 0x802, 66, 0x402 (1026), 0x802, 0x1a (26), 0x802
```

— roughly eight parameters, most allowing two kilobytes. Every value shape tried
(120-char token, 32-hex key, 32-hex device id, short forms) is far inside those, so
**capacity is definitively not the 402**.

The name operands point into a `.rodata` table region (0x5286xx holds pointer tables,
not strings), which matches the earlier finding that action and argument names are
assembled rather than stored as literals — the handler walks a shared table by index.
So the remaining unknown is the *table order and keys*, and the way to read it is the
table itself (or the controller's call), not more values.

### `AddAccountX` takes long values and stays healthy

On a unit with no prior account (`.137`, first ever add):
`AddAccountX(AccountType=2311, AccountID="1", AccountPassword="p", AccountUDN="U"×200)`
→ **200**, player healthy. Repeating with a **2000**-character UDN → 200 again,
healthy, and the same id "1" was accepted both times even with a different UDN.

So `AccountUDN` is accepted at the two-kilobyte class the handler's capacities
imply, and nothing in that call path overflows: either the copy is not in
`AddAccountX` (it belongs to the OAuth sibling, per the caller strings) or its
destination is far larger than the ~16-byte field is. Long values alone are not
enough — the OAuth action has to be made to accept its parameters first.

That closes the value-side experiments on the account path. Everything now points at
one artefact: the parameter table the OAuth handler walks, or a real call to it.

### Handler parameter plumbing (structural facts)

The `AddOAuthAccountX` handler drives a parameter-list object, and reading that
plumbing gives three concrete shapes for anyone continuing:

- it advances an index at `+4` and hard-stops at **16 params** (`sltiu v0,v1,16`);
- a parameter descriptor lives at `obj + idx*0x30 + 0x2ec` (stride `0x30`);
- the `lui a1,0x52; addiu a1,…` operands point at a `.rodata` region of **code
  pointers** (a dispatch/vtable-like table, with `0x4ee214` as the common stub), not
  at name strings — which is why resolving them as strings comes back empty.

So the names are not reachable by dereferencing those operands; they are resolved
through the dispatch table. Combined with the earlier result that capacity, service
id and argument count are all ruled out, the 402 has to come from a *required-param
or ordering* rule encoded in that table — i.e. read the table, or capture one real
call.

Also ruled out: the `:1443`/WS `upnpSystemProperties#call` proxy is no softer than
raw SOAP for this action — the same six `input[]` pairs give the same UPnP 402. The
proxy only changes the transport, not the handler or its parameter table.

### Refinement: the issuer is a global, and a prefix test runs before the inspector

Reading the caller around `0x603cf8` corrects the "iss is compared to reader state"
framing. The sequence is:

1. a map lookup on the parsed stream, then `FUN_00603aac` is re-entered to read the
   value at `sp+447` with length **17** — the claim's JSON value;
2. `rfind(<claim>, <global c-string>, 0)` (`0x520578`) is called on that value with
   the global at **`0x7048e8`**, i.e. `starts_with`;
3. only if it returns 0 is the inspector `FUN_006021e4` called; otherwise control goes
   to `0x603c94`.

`0x7048e8` is in `.bss` (`0x6fa8f0..0x7056f0`) and its address appears as a relocation
in the `.data.rel.ro` table at `0x46fa14` alongside `0x704828/0x704844/…`, so it is a
member (an inline `std::string` buffer) of a global auth/context object, not a
constant. The reader fills the context on the way through, but the *prefix* that gates
the inspector comes from that object.

So a forged `iss` must **begin with** whatever the global holds at runtime; "Sonos"
very likely fails that prefix test and lands on the non-inspector path, which is also
the most plausible source of the `invalid_token` answer. Next: identify that global's
value — the code does not reference the struct through `lui`, so it is reached through
`gp`; the writer of `0x7048e8` is the thing to find (it is a `std::string` assignment,
not a constant), and on a live S2 unit the household id is the obvious candidate to
try as an `iss` prefix.

**Expiry is not a usable sub-oracle.** Two live probes with `exp` an hour in the
past (`iss` = `urn:dev:ops:44974-zp-X` and `iss` = `Sonos`) both answer the same
blanket `invalid_token` / "Access token is invalid" as a valid `exp`, so the HTTP
description does not distinguish inspector codes 2/5/6. Only a full pass (a 200 and
the reboot) is observable from outside; treat every other outcome as opaque and
read the code, not the response.

Recorded plumbing for whoever continues: `sp+64` is not a separate claim — the 72
bytes at `sp+32` are `memset` to zero at `0x603ba0` and `sp+64` is that struct's
`+32`; the reader `0x603aac` fills it from the claim named by a **17-byte** string
carried at `sp+440` (stream at `sp+264`, view `{sp+447,17}`), and only then does the
`rfind` at `0x603c8c` prefix-test it and the inspector run at `0x6021e4` with
`(out=sp+264, ctx=s1, pointer=sp+516, state=sp+32)`. Naming that 17-byte claim is
the last unknown before the code can be driven by construction.

### `AddOAuthAccountX` has ten arguments — and it is not the ARP checker

Decompiling the real action wrapper (`FUN_00481330`, which is what the dispatcher
calls) shows the handler fetch-and-set sequence, in order:

```
AccountType, AccountToken, AccountKey, OAuthDeviceID, AuthorizationCode,
RedirectURI, UserIdHashCode, AccountTier, AccountUDN, AccountNickname
```

and the served SCPD declares exactly those ten, in that order. Earlier probes used
six — three (`UserIdHashCode`, `AccountUDN`, `AccountNickname`) were never sent,
which is *one* reason every one of them returned 402.

**Correction to the earlier H13 note:** `0x41ad84` is not a sub-function of this
handler at all. Its decompilation is an ARP-checker initialiser
(`/proc/sys/net/ipv4/neigh/<if>/…`, a pipe, `arpchecker` logging). The `strcpy` at
`0x41b0c0` copies `obj+0xa1` — an **interface name**, not an account field — into a
16-byte stack buffer (`siocgif*` ifreq block: `struct ifreq` is 0x44, memset; strcpy
target `ausStack_3c[16]`). So the overflow fires only if the interface name exceeds
15 characters, and the account work does not feed it directly.

### What the 402 actually tests

The `li 0x192` sites for this action sit in the dispatcher at `0x4bdcf0`-`0x4be5xx`
(one per parameter) and all have the shape

```
jal 0x4be6fc        ; validate+set one parameter
bnez v0, <next param>
li a1, 402          ; else fail the whole call
```

so a **single rejected parameter** aborts with 402 before the handler body runs — which
is why every partial shape failed on its first argument.

`0x4be6fc` takes the argument object, reads its `content-length` header, and runs
`0x4a4200`, a chunked copy of exactly that many bytes (4096-byte blocks, via a
reader virtual and a decoder `0x4a408c`) into the parameter's storage, then
finalises with `0x4a4120`. Its verdict is "did the copy complete": 402 is raised
when the declared `content-length` does not match the bytes the reader yields, or
when the decoder rejects them — not when a value is semantically wrong.

That is a mechanical, not semantic, gate: the argument content has to arrive in the
form the reader expects. `SetString` and `AddAccountX` use the same machinery and
accept ordinary text, so for this action some argument is being rejected on its
content-length pairing rather than its name.

Refinement from the SCPD itself: of the ten declared arguments, `AccountUDN` and
`AccountNickname` are `out`; the **input** set is eight, with `AccountType` and
`AccountTier` typed `ui4` and the rest `string`. The state variables carry no
allowed-value lists or ranges, so the types impose nothing beyond "numeric" for the
two ui4 fields.

Eight inputs, numeric ids, realistic OAuth-shaped content (GUID device id, 40-char
code, 32-hex hash, `https://` redirect) and ids 9/2311 all still produce 402. So the
rejection is *not* documented constraint either — the per-parameter validators
(`0x4bdccc` onwards, one `li a1,402` each) fail on something the SCPD does not
describe. The next step is to walk those validator cases in order and see which one
returns zero first, rather than trying more content.

### 86.10 daemon in the guest: reaches settings init, needs real `/jffs`

The 86.10 rootfs section decrypts with the model-12 key and unpacks as squashfs
(`unsquashfs`), and the daemon starts under the guest loader with its own libs once
**libflash's `MEMGETINFO` constant is patched to `0x40204d01`** (MIPS16 literal at
file `0x15f8`, halfword `4024` → `4020`; stock `mtd.ko` accepts that value, a patched
`mtd.ko` carrying `0x4024` does not). Without the patch it aborts with `SIGIOT` on
`ENOTTY`, exactly like 57.23.

With the patch it gets past flash init and dies later at **`0x4a801c`**
(`lw a0,0(v0)` after `lw v0,448(s0)`, i.e. a null chain) once it starts reading the
settings/schema objects. It also logs `dnssd_clientstub … /var/run/mdnsd` (no mDNS
socket in the guest). Seeding an empty `/jffs/localsettings.txt` /
`syssettings.xml` and mounting tmpfs on `/jffs` does not clear it, so the null is in
the schema data, not the two files.

That is the state to resume from for running the 86.10 token path locally: the
binary, its libs and its config are staged at `/opt/s86` in the guest, paths linked
from `/opt/{bin,lib,conf,htdocs}`, and the flash ABI is the only patch applied.

**The 17-byte name is not a perf-counter.** The pointer table at `0x6af878` holds
`AUTH_IS_AUTHORIZED`, `AUTH_PARSE_DEVICE_TOKEN`, `AUTH_POLICY_TABLE_CACHE_FETCH`,
`COMMAND_*`, `PLAYER_VOLUME_SET_VOLUME` — all longer than 17, so none is the string
the view `{sp+447,17}` names. The block that builds it is `0x603c38–0x603c44`
(`6049e8(sp+516)`, `604fe8(sp+32)`, then `Stream(sp+264, sp+440)`), i.e. `sp+440` is
the `std::function` object the stream reads through and `sp+447` its capture. Read
`604fe8`'s field list for the 17-byte capture instead of the counter table.

The daemon is **not** fully stripped of names in the useful sense: its `.dynsym`
exports 1081 functions, including `sonos::localsettings::*`
(`SchemaDataHelper::IngestSchemaSchemaData`, `SchemaData`, `AttributeDetail`, …), so
`nm -D -C` gives real names for exported code even though `.symtab` is gone. The
null-deref at `0x4a801c` sits in that localsettings/schema area, consistent with a
missing per-model schema: `opt/localsettings/models/` is empty and the MDP we write
is the s1emu stub without model data, and 86.10's `anacapa.conf` has no `HackModel`
knob to force one (unlike the older conf). Filling `models/` (or a real MDP) is the
next thing to try before the daemon will finish settings init.

### The OAuth body found — and what it actually reads

The action-name table (`.rodata` 0x51f9f0, entries of `{name_ptr, name_len, 1}`)
puts `AddOAuthAccountX` at index 1 behind `AddAccountX`, 15 actions in all. The body
is `FUN_004bd39c`, and it reads exactly four arguments:

```
AccountType   via FUN_004a43ec  (integer getter)  -> local
AccountToken  via FUN_004a4460  (string getter)   -> 2052-byte buffer
AccountKey    via FUN_004a4460                    -> 2052-byte buffer
AccountUID    via FUN_004a43ec  (integer getter)  -> local, then flag byte set
```

then bounds both strings with `0x406510` at **< 0x801**, and calls the owner virtual
`+0x2c(parent, type, uid, token, key)`; zero from that virtual is success, anything
else is a 402. So `AccountUID` is a real input that the SCPD does not list, and the
body itself imposes a 2048-byte ceiling per credential.

The pre-body validators (0x4bdccc and its chain, one `li a1,402` each) run first:
each calls `0x4be6fc` (the content-length copy) and a per-type coercion virtual, so
402 before the body means the *request-shaped* copy fails, not the body's logic.

Tried and still 402: the exact four inputs; the full ten; chunked transfer-encoding;
HTTP/1.0; absent `charset`. The four-input shape is the correct minimum, so the next
step is `0x4be6fc`'s header source — where the `content-length` on an argument object
is set — rather than more shapes.

### Token lane: the composer and the pre-gate are now exact; two values are built locally

Read on the 86.10 build (`captures/firmware/s2_overlay/bin/anacapad`, MIPS16 — plain
`objdump` mis-decodes it, `-m mips:16` is required; Ghidra will not form functions in
that region, so the caller is read as assembly).

`FUN_00520578` is a one-line `std::string::rfind` wrapper and the object compared
against is the `.bss` `std::string` at `0x7048e8`, static-initialised from the literal
at `0x666b40`, `urn:dev:ops:44974-zp-`. So the gate is a **prefix test**: `iss` must
begin with that URN or the inspector is skipped entirely.

The composer `FUN_006020f8` is confirmed instruction by instruction: `0x602094`
appends `'{'` (`FUN_005f7420(...,0x7b)`), `0x602074` appends a string, and `'}'`
(`0x7d`) closes. The order is

```
{ owner { luid <luid> } hhid <hhid> network_hash <network_hash> }
```

with `<luid>` = `param_2+0x48`, `<hhid>` = `param_2+0x30` (both filled by the policy
callback from `iss`), and `<network_hash>` = the third argument.

At the call site (`0x603cf2`) the third argument is `sp+516`, filled immediately
before by `0x603940` — so **`network_hash` is computed locally**, not read from the
token. `0x603940` builds a string and hands it to `0x51b9b4`; `0x603964` is its
companion. Those two, plus whatever fills the policy struct's `+0x30`/`+0x48`, are the
whole remaining surface.

Measured blind (all `401 invalid_token`): `iss` suffixes RINCON / deviceId / serial;
`mhhid` as the bare household id, `Sonos_<hhid>`, the bare brace form with empty
`luid`/`network_hash`, with the household id in the network slot, and with
`locationId` in either slot. A single-token probe cannot score these — the reply is
one bit — so the next work is the two builders, not more combinations.

### From the blasty A113X research: storage format, OTA manifest, noexec

The talk slides (Sonos One / Lenovo clock, A113X) document two things that transfer:

- **Secure-storage key entries are TLVs.** The EL3 parser takes a blob whose first
  block is `TYPE_PARAM_HEADER(1)` containing `TYPE_ENCRYPTED_SIZE(2)`, then entries of
  `TYPE_KEY_DEFINITION(3)` with `NAME_SIZE(4)`, `NAME_DATA(5)`, `VALUE_SIZE(6)`,
  `VALUE_DATA(7)`, `KEY_TYPE(8)`, `BUFFER_STATUS(9)`, `HASH_DATA(0xa)`, all `u32 type /
  u32 len / value`. On-device the model RSA key is stored encrypted in **MDP3**
  (`sonos_blob_encdec`, AES-256-GCM with a key derived from the OTP `AES GCM HWKEY`;
  MDP3 offsets 0x580/0x680 on that platform). That is very likely what sits *inside*
  the `886499ca` envelope of our type-8 store record: the envelope is the transport,
  the TLV above is the payload. Worth building next in the VM.
- **The OTA manifest** is `https://update.sonos.com/firmware/latest/default-1-1.ups`,
  returns a TLV manifest with a URI base, and the query string (`cmaj/cmin/serial/...`)
  is **not validated** (serial `111111111` works). A `.ups`/manifest path therefore
  exists that is independent of a direct `.upd`, and any field read from the manifest
  rather than the package is a candidate for bypassing the patch-list gate.
- `/jffs` is mounted **noexec** on these devices (`sonos_allow_mount_exec`), so a
  persistence payload must be a script interpreted by init (`Configure.dev` is `sh`'d)
  or the mount flag must be flipped; a binary dropped in `/jffs` will not run.

## 2026-10-24: public prior art maps onto our open lanes

Two public sources name exactly the bug classes already open here, so the CVEs can be
used instead of re-derived:

- **SSA-2024-0002** (Sonos, 2025-02-12) covers three Pwn2Own 2024 findings:
  **CVE-2025-1048** remote code execution in the **HLS** handling (RET2 / ZDI),
  **CVE-2025-1049** RCE in **MPEG-TS** parsing (InfoSect / ZDI), and CVE-2025-1050 an
  **SMB2 use-after-free** (Viettel; S2 only). All three are fixed in Sonos
  **83.1-61240** (S2) and **57.22-61162** (S1) — i.e. the fix line is the same number
  our patch-train floor already sits on.
- **NCC Group, "Listen Up" (BlackHat USA 2024)** has a whitepaper and slides on
  remotely compromising Sonos One over the air (MediaTek WPA2 driver,
  CVE-2024-20018) and on breaking Era-100 secure boot through U-Boot
  (**CVE-2023-50810**) to reach EL3 and load unsigned images.

Consequences for this project:

- our HLS daemon-kill (`frankensonos-re-hls-kill-cnk`) is CVE-2025-1048's class and
  our TS probes are CVE-2025-1049's; both should be worked against the public
  descriptions rather than from scratch;
- **every one of those is patched on our builds** (57.23 and 86.10 are far above the
  fix line), and the patch-train floor is the same 57.22-61162 — so reaching a
  vulnerable build needs a *downgrade below the floor*, which is the same obstacle as
  installing anything unsigned. A downgrade attempt to 57.22-59130 was made: the
  device accepted `BeginSoftwareUpdate` (200, after sending all three arguments —
  `UpdateURL`, `Flags`, `ExtraOptions`; one or two arguments give 402) and started
  fetching, but the transfer aborts at ~131-147 KB on today's routed path, so the
  version verdict is not yet in.
- CVE-2023-50810 is Era-100/Era-class U-Boot; our Play:5/Play:1 fleet is MIPS and
  predates it, so the EL3 route applies only if an ARM model is acquired.

The eMudhra intermediate key remains the live lane for *this* fleet, since it needs
no downgrade.

### S2 write surface: the 403 gate is the devmode statement bitset

86.10-81100, same lane as `docs/RE_WRITE_PRIMITIVES.md`. The route gate is a bit
test `ZZ0018` (libsonoscrypto `0xad8c`, imported once) behind
`sonosMuseRESTStream::checkPerm` `0x4f3824`, with wrappers `0x4f2080` (bit 22)
and `0x4f2094` (bit 18). The tested bitset is the **raw devmode statement**
copied to the devmode object: `RDevMode::update` `0x4f3a40` memsets 1024,
memcpy's the statement only after `ZZ0137` verifies (`0x4f3bbc`/`0x4f3bc6`),
stores the length at `+0x400`, and refuses a statement of 1025 bytes or more
(`0x4f3b56`). So bit 18/22 have exactly one setter: a verified devmode
statement. Gate map: bit 18 = `/advconfig`, `/testenv`, `/diaglevel`,
`/diagmsg`, `/logger`; bit 22 = `/reset`, `/reboot`, and the whole page family.
`/devmode` is open because its handler tests no bit.

Only fixed names are written pre-auth: the settings entry (`SetString`), and
`/tmp/devmode.tmp.bin` + `debug/devmode.bin` from the devmode store
(`0x4f3ef8..0x4f3f40`, `O_WRONLY|O_CREAT|O_TRUNC` 0600, after verification).
A request-chosen *name* exists on the read side only —
`/status/ZPLocalSettingsFile/<name>` joins `*(0x7004c4)` with the name
(`0x4d4448`) and `..` is honoured once (`../VERSION` reads `/jffs/VERSION`);
its three callers are constant names. There is no request-driven writer for
that builder. `:1443` mirrors this: `/raw` 403, `/device_account/registration`
`[/state|/status]` is read-only 200 JSON, POST verbs 405.

### anacapad serves in the css guest (2026-10-24, no live unit touched)

The DCMS abort does **not** reproduce with the binary that tree actually holds.
`/opt/s57/bin/anacapad` hashes to `captures/firmware/s1_rootfs/opt/bin/anacapad`
(1 451 072 B, MIPS, `md5 beB283F…`) — the 34-era daemon, not a 57.23 one — and
`/status/VERSION` answers `57.23-74170` from the surrounding tree. That build
never reaches the regcert gate; `ls /jffs/regdevicecert` is still empty and no
`keyval` run was needed.

What it takes to run it in the guest:

1. stage a complete `/opt/s16` from the image (bin, conf, htdocs, lib) and keep
   the **patched** `libflash.so.1` (ABI `0x4020`);
2. let the guest's own loader do the work — `/lib/ld-uClibc.so.0` is the 45.1
   `0.9.28` one; invoking the rootfs's `ld-uClibc-0.9.26.so` by hand segfaults in
   the loader at `base+0x1070`;
3. `LD_LIBRARY_PATH=/opt/s16/lib` and run `./bin/anacapad -c conf/anacapa.conf`
   from `/opt/s16` (it needs `log/`, `conf/mime.types` and `htdocs/xml` beside it).

Result: 11 threads up, **`0.0.0.0:1400` and `:1443` listening**, and both SOAP
verbs answer — `GetZoneAttributes` returns
`<CurrentZoneName>Unnamed Room</CurrentZoneName>`, and `SetZoneAttributes`
(GovRoom) answers 200 and is reflected by the next Get.

**HLS is not in this binary.** `strings` finds no `hls`/`EXT-X`/`mpegurl`, and no
`lw/sw` with the ring offsets `0xc508/0xc50c/0xc510/0xc514` exists. The queue
functions we emulated (`0x10192754` et al.) are **PPC-only, 1-16 build**; a gdb
breakpoint there cannot hit in a guest running the MIPS 34-era daemon. Getting the
real field pairs needs the 1-16 (or a 57.x MIPS HLS) `anacapad` in a guest with
the same staged tree — the 1-16 image's own libs are PPC and the guest is MIPS.

### PPC guest recipe (2026-10-24, timed out before the port opened)

Setup that is already in place on `css` and works:

- `qemu-system-ppc` **is installable** (`10.2.1`, `apt-get install qemu-system-ppc`) — the
  earlier note that only MIPS was available is stale;
- PPC kernel obtained: `linux-image-3.2.0-4-powerpc_3.2.78-1_powerpc.deb` from
  `archive.debian.org/debian/pool/main/l/linux/`, unpacked to
  `/tmp/ppcboot/boot/vmlinux-3.2.0-4-powerpc` (20 MB, boots to userland);
- boot line that reaches `READY-PPC`:
  `qemu-system-ppc -M mac99,via=pmu -cpu G4 -m 512 -kernel vmlinux-3.2.0-4-powerpc
   -initrd ppcrun.gz -append "rdinit=/init console=ttyPZ0,9600"
   -net user,hostfwd=tcp::14000-:1400 -net nic -nographic -no-reboot`
  (start it with `setsid … &` or it dies with the ssh session; also raise the ssh
  timeout past the `sleep`, or the caller kills it);
- initramfs = `/tmp/ppcx` (unpacked `ppcinitrd3.gz`) with the 1-16 rootfs at
  `opt/s16` (shipped from `/tmp/s1root16`); the tree is rooted so the daemon is
  `/opt/s16/opt/bin/anacapad` and its conf `/opt/s16/opt/conf/anacapa.conf`, libs
  `/opt/s16/lib` (PPC, so no ABI problem: **the 1-16 libflash needs no `0x4020`
  patch** — no `3c164024` exists in it, unlike the 57.23 library).

Two init bugs found and fixed — keep them fixed:

1. the initramfs busybox has **no arithmetic support**, so `$((i*2))` aborts with
   "you disabled math support" and panics; write minors literally;
2. `mknod`/`ln` are not applets on `PATH` — call `/bin/busybox mknod` and
   `/bin/busybox ln`.

State reached: init runs, `/opt/s16/opt/bin/anacapad -c conf/anacapa.conf` starts
under `LD_LIBRARY_PATH=/opt/s16/lib` and prints `Error 1 opening LED device`
(cosmetic), but **:1400 never opened** and the VM was found dead on the next
check — the run was cut by the timebox, not by a diagnosed failure. Next step is
to run the same initrd with `-serial file:` and a longer-lived `setsid`, confirm
whether the process aborts at the DCMS/regcert gate, and only then add
`keyval`-made `/jffs/regdevicecert/{cert.xml,prod/cert.xml,prod/metadata.txt}`.
The mtd nodes below are placeholders on `mtd0`; the guest kernel has no mtdram
module, so `MEMGETINFO` behaviour is still unproven here.

### PPC guest: the daemon reaches the port binds, then SIGABRTs (2026-10-24)

Progress past the earlier recipe, and the corrections that made it run at all:

- **`pkill -f qemu-system-ppc` kills the ssh shell** whose command line contains
  the string; use `pkill -x`. Most of the earlier "the VM died" was this.
- run the VM from a script under `tmux new-session -d` — `setsid … &` inside an
  ssh command is unreliable.
- the daemon resolves `/opt/bin`, `/opt/conf`, `/opt/htdocs`, `/opt/lib` and
  `/opt/log` **absolutely**: symlink them to `/opt/s16/opt/*` and `/opt/s16/lib`
  and run from `/opt`. The initramfs ships `/opt/log` as a *file*, so `rm -f`
  then `mkdir -p` it, or the daemon dies with `Bad PidFile value`.
- `0x10007710` waits forever on `/dev/chk` (ioctl `0x4004f301` == 6550). With no
  driver, patch it: `li r3,1; blr` at file offset `0x7710`, or nop the branch at
  `0x78cc`. Without this the daemon spins at `0x100078e4` and never starts.
- **libflash shim** (`-shared -fPIC -nostdlib`, exports the eight dlsym names
  `hal_flash_get_mdp`, `…_get_mdp2`, both `_wrapper` forms, `info_init`,
  `info_exit`, `mdp_write`, `sonosMdpSerialToString`, plus the data symbols
  `bytes_so_far`/`total_bytes`). Two traps: the shim **must carry a SYSV hash**
  (`-Wl,--hash-style=both`) or this glibc's dlopen rejects it, and it needs
  `-Wl,-soname,libflash.so.1`. With only GNU hash the daemon segfaults on the
  NULL handle before any call.
- `DiagLevel default=7` in the conf is what makes the startup trace readable.

State reached: modules load, `anacapa is starting on port 1400/1443/1843`, then
`Aborted` (exit 134) ~40 ms later, before anything accepts.

**The DCMS strings are not in this binary.** `regcert: loaded:%s`, `BAD_CERT`,
`MISMATCH_*` are absent from the 1-16 `anacapad` (only `/regcert` and `regcert`
exist), so the earlier DCMS analysis came from a different build and the abort
here is a different fatal. It is not the regcert gate: `/jffs/regdevicecert` was
populated with a self-signed cert and a `SID=` metadata file and the abort is
unchanged.

Tooling note: the qemu gdbstub accepts `target remote` but `handle`/`continue`
never stop on a guest signal in batch mode; the reliable instrument is the
monitor socket — start qemu with `-monitor unix:/tmp/mon,server,nowait` and
`echo "info registers" | nc -U /tmp/mon` to sample NIP (that is how the
`/dev/chk` spin was identified).

#### Locating the abort (2026-10-24)

The abort call is not reachable by symbol: this binary's `.plt` is **all zeros in
the file** (the loader fills it at run time), so `objdump -d -j .plt` prints an
empty body. Resolve it through `.rela.plt` instead: `abort` is relocation index
203, and its GOT slot `0x10546688` sits inside the `.plt` range, so the stub is
`0x10546678` and the two `bl` call sites are `0x101e36c0` and `0x102caf28`.
NOPing both is *not* enough — the abort is reached from libc.

The decisive move is to patch `abort` itself in `libc.so.6` (vaddr `0x354a4`,
first word `9421fed0` → `4e800020` = `blr`). With that, the daemon stops dying
and keeps running indefinitely. The trace then shows where it was: the fatal is
in the **Spotify/registration init**, right after `spotesdk … Starting`,
`Failed to open inputs token (1)` and the `internalevts` subscriptions.

State with `abort` neutralised: the process survives and the timed-job manager
runs (`advertise` every 60 s), but the HTTP listener never comes up — the
`starting on port 1400/1443/1843` line is absent and `advertise` fails with
`failed to open local description http://127.0.0.1:1400/xml/device_description.xml
(0x81000004)`. So the abort sits inside the server-start path and returning from
`abort()` skips it rather than resuming it.

Sampling note: `-d int` gives the exception PC stream but the abort syscall
(`tgkill`) does not appear in it; sampling NIP over the monitor is too slow
(~200 ms per `info registers`) to catch a 40 ms window. Patching `abort` in libc
is the reliable way in.

#### Forcing the append in the guest (gdb mechanics, reusable)

The queue append at `0x10191fbc` can be driven by hand once the queue ctor
`0x10192364` has run; no listener and no network are needed.

- **gdb cannot read or write the queue pages** (`Cannot access memory at
  0x48557150`) even when stopped there, but **`restore <file> binary <addr>`
  does write**. It is byte-order aware: the blob must be big-endian, so a
  `0x00000010` word is the file bytes `00 00 00 10`.
- The **monitor `x` reads work only while the CPU is stopped in a user thread**.
  Stopped in the idle task (`0xc0012df4`) every user address is unmapped and all
  reads fail. `set $pc` on the idle task is also lost on resume — the idle
  context is restored from its saved state, so the append never runs. Drive the
  append from a breakpoint hit by the daemon itself (the ctor is such a point).
- `-ex continue` returns immediately in batch mode unless `set target-async off`
  is issued first; with it, `continue` blocks and stops at breakpoints. A
  breakpoint left installed at an address whose page is momentarily unmapped
  aborts the resume with `Cannot access memory` — delete it after the hit.
- `gva2gpa <vaddr>` works while stopped and is the only way to get a physical
  address; the monitor has no `write` command in this build.

Result of the forced run (record `+0x20 = 0x2f2f2f2f`, `+4 = 'etd '`, `+8 = ' tel'`,
`+12 = 'netd'`, `+0xC50C = 16`, `+0xC510 = 0`): the first `memcpy` destination is
`ring+0xC508` — the header — and the array pointer at `ring+0xC528` becomes
`0x2f2f2f2f`; the second destination is `0x2f2f2f2f + 0x65746420*0xC50 =
0x583ff92f`. The guard at `0x10191fe4` proceeds when `+8 < +0xC` (count <
capacity), not the other way round.

#### The advance path is never wired (lane closed: proven but latent)

The HLS advance/append cluster `0x1018f084` (fill), `0x1018f738` (bitrate
downgrade), `0x1018f7d4` (previous segment) and `0x1018f858` (next segment, the
one that pushes the pending record) is **referenced nowhere in the image**:

- no `bl` to any of them,
- no 32-bit word anywhere in the file points into `0x1018f000`–`0x1018fa00`,
- no `.rela.*` entry has one of them as addend,
- no `lis`/`addi` pair constructs their address.

The class vtables the ctor installs are ordinary absolute-pointer tables
(`+0x2024` → `0x10468e84` = `{0x1015e22c, 0x1015e24c, 0x1015bc58, "GetMediaInfo"}`,
`+0x17C8` → `0x10468e58`, `+0x17D0` → `0x10468e38`), and none of their entries is
in that cluster.

So the pending push is never reached at run time in this build: the counter at
`record+0x2304` (incremented per segment by `0x1018f238`/`0x1018f21c`, capped at
3, zeroed only by the base initialiser `0x10158114`) never gets read, and the
split's armed branch stays dead. The corruption is real and reproducible by hand
— first append lands on `ring+0xC508` and destroys the array pointer — but no
playlist shape can trigger it while the advance slot is unwired.

#### Correction: the advance IS wired — through vtable slot 23

The earlier "no address word" search was wrong because it looked for the advance
function itself. The advance is reached through a **wrapper**, and the wrapper is
the vtable entry:

- `0x1018bdbc` is the wrapper: `bl 0x1018f858` (advance, `r4 = 0`) at
  `0x1018bde4`, then an **unconditional** push at `0x1018bdf4` — so the pending
  record is pushed on every call of the wrapper, whether or not the advance
  succeeded.
- `0x1018bdbc` is slot 23 (`+0x5C`) of the vtable at `0x1046d898`, installed by
  the ctor at `0x1013c7b8` / `0x1013ca08`.
- Slot 23 is called at `0x1000b064` in the playlist parser at `0x1000abdc`,
  through the global object at `0x10545c44` (`lwz 3, 0x5C44` → `lwz 9, 0(3)` →
  `lwz 9, 92(9)` → `bctrl`).

The gate is a chain of four `strcmp` calls (PLT `0x10546270`) against the
suffix-merged strings at `0x1046e6a0`, `0x1046e6a8`, `0x1046e6b0`, `0x1046e6b8`
(`"t"`, `"lid media playlist: %s: line=%s"`, `"a playlist: %s: line=%s"`,
`"st: %s: line=%s"`); all four must compare unequal for the slot to be called,
otherwise the parser takes the path at `0x1000ad78`.

So the lane is wired, not dead: the trigger is reaching that block in the
parser, and the four string comparisons are what to attack next.

#### The slot-23 caller is /audio_tap, not /advconfig

The route descriptors are in the file at `0x1052d338` and `0x1052d354` and are
`{name, handler}` pairs:

```
0x1052d338  "/audio_tap"  0x1000abd8
0x1052d354  "/advconfig"  0x1028a724
```

So the device-name chain and the slot-23 call live in the **/audio_tap** handler.
It parses the `tap` query parameter (`strcmp(param, "tap")` at `0x1000ac80`, value
copied to a stack buffer at `0x1000af38`), then compares that value against
`htain`, `htaoutl`, `htaoutr`, `htaouts`, `linein`, `codecout`. A match diverts to
`0x1000ad78`; an **unknown** name falls through to the slot-23 call at
`0x1000b064`, which advances the segment index and pushes a pending record.

So the on-demand trigger is `GET /audio_tap?tap=<unknown-name>`, not
`/advconfig` — and `/advconfig`'s POST form carries no device selector at all
(only `csrfToken`, `FirstZP`, `PriorityBridge`), which is why a POST there cannot
arm the queue.

#### The tap path is the only caller — the HLS write needs an unlock

Exhaustive reference count for the advance and its wrapper:

| symbol | `bl` callers | data words |
|---|---|---|
| wrapper `0x1018bdbc` | 0 | 1 — only vtable slot `0x1046d8f4` |
| advance `0x1018f858` | 1 — only the wrapper (`0x1018bde4`) | 0 |

So the only way in is slot 23 of the vtable at `0x1046d898`, and the only caller of
that slot is `0x1000b064` in the `/audio_tap` handler. The object is a
process-wide singleton created at init (`0x10008cc0` stores it to `0x10545c44`),
so it exists during radio playback — but nothing else ever invokes the slot.

The handler's permission gate runs first: `0x1000afd0: bl 0x103933b0`, which
`stat`s `/tmp/device_unlocked_flag` (`0x1049704c`) and returns 1 only if it
exists; on 0 it logs `AudioTap: permission denied` (`0x1043e684`) and answers 403
before any device-name compare. Creating that file in the guest makes the gate
pass, but the daemon still has to be serving for the GET to arrive — and it
aborts before binding, so the tap route is not reachable end-to-end yet.

Conclusion: the HLS header write needs the unlock path (q57 / devmode lane); no
pre-unlock route reaches the wrapper.

#### Who writes /tmp/device_unlocked_flag

Exactly one writer. The path string `0x1049704c` has one occurrence and three
code uses:

| site | call | role |
|---|---|---|
| `0x102a8cb4` | `open(path, 0x40 /*O_CREAT*/, 0644)` | **creates the flag** |
| `0x103933c4` | `__xstat(3, path, buf)` | the `/audio_tap` gate |
| `0x100c1d20` | trace log | diagnostic only |

The writer sits in the function at `0x102a89d0`, whose nearby string is
`failure when exiting %s config mode (%d)` (`0x104689d8`) — i.e. the flag is
created on **config-mode exit**. That function is itself unreferenced statically
(no `bl`, no data word), so it is a thread entry / callback started by the
config-mode flow rather than by an HTTP route.

So the chain is: config mode exit → flag created → `/audio_tap` gate passes →
slot 23 → advance + pending push → next playlist parse arms the split → header
write. Nothing else in the image creates the flag, and no plain HTTP route does.

**The devmode/config-mode statement is therefore the single precondition for the
whole HLS write**, and it is the lane's real next step.

#### GCModel S16 in the conf removes the abort; the guest serves

Adding `GCModel S16` to `conf/anacapa.conf` (the file uses `Key value`, next to
`Port`) and dropping the `0x1036ba88` branch patch lets the model switch take a
real arm: no `Aborted`, the daemon stays up, and 1400/1443 bind.

Proof from inside the guest:

```
HTTP/1.1 200 OK
Server: Linux UPnP/1.0 Sonos/57.23-74170 (ZP100)
...
<u:GetZoneAttributesResponse ...><CurrentZoneName>GovRoom</CurrentZoneName>
<CurrentIcon></CurrentIcon><CurrentConfiguration>1</CurrentConfiguration>
```

Note the Server header reports **ZP100**, not S16 — so the value is not taken
literally as a model name; it selects a code that lands on the ZP100 arm. The
switch no longer reaches its default, which is what matters.

Caveat: the host forward `tcp::14000-:1400` does not work (curl from the host
gets 000) even though the guest listens; test from inside the guest with `nc`.

#### Exploit run, first attempt (blocked at AVTransport)

Done: `/tmp/device_unlocked_flag` created (0644), payload playlist built and served
on `127.0.0.1:8099` (40 entries, 711-byte segment name: 32-byte
`busybox  telnetd -l/bin/sh -p23 `, `13 dd 51 c8` at 0x20, `10 54 69 b8` at 0x28
and 0x30, zero pad to 0x2c0, stub, `.ts`).

Blocked: the device description (`/xml/device_description.xml`) lists only
`DeviceProperties`, `SystemProperties`, `ZoneGroupTopology` and `GroupManagement`
— **no AVTransport**. `POST /MediaRenderer/AVTransport/Control` answers 405 with
`Allow: GET, HEAD`, so `SetAVTransportURI`/`Play` cannot be issued and nothing
plays.

`GET /audio_tap?tap=test` answers **404** (not 403, so the flag gate passes and
the route is GET-only — a POST gets 405 with `Allow: GET, HEAD`). Whether the
slot-23 push actually ran on that 404 is unverified; port 2323 is closed.

Next: get AVTransport registered (it is missing from the description, so the
service never came up), then re-run steps 3-5.

#### GCModel does not select the model

`GCModel 34` (the S16 code) behaves exactly like `GCModel S16`: no abort, but the
Server header still reports **ZP100** and the description still lists no
AVTransport. `GCModel` is not a conf key at all — in the binary it appears only
in serialisation (written out), never read from `anacapa.conf`, and neither
`/jffs/localsettings.txt` nor `/jffs/syssettings.xml` contains it.

So the model comes from the flash shim, which returns zeros for every call. The
shim needs to hand back a real MDP/model code instead; that is the remaining
blocker for AVTransport and therefore for the play step.

#### What the daemon reads from the MDP (shim contract)

The shim symbols are called with a **caller-supplied buffer**, not a returned
pointer: `hal_flash_get_mdp_wrapper(buf)` at `0x1036b2d4` / `0x1036bb18` fills a
512-byte buffer (`memset` then call), and `hal_flash_get_mdp2_wrapper(buf)` at
`0x1036b2dc` / `0x1036bb20` fills a second buffer. The daemon then reads:

- `lwz 9, 0(buf2)` — a **magic**, compared against `0xCA989B4A` and `0xFA87B921`
- magic `0xCA989B4A` and `obj+0x180 < 3` → model code at **`buf2+0x44C`**
- magic `0xFA87B921` → model code at **`buf2+0xA10`**
- the code is stored to `obj+0xE8`

The shim now zeroes both buffers, writes magic `0xFA87B921` at `+0` and `34` at
`+0xA10`. Result: still `ZP100` and still no AVTransport, so either the wrapper
is not on the startup path or the code is consumed elsewhere. The offsets above
are the contract to work from; the payload itself is unproven.

#### The switch table is code→name, and S16 is not code 34

The name table is at `0x104f64a4` (`S13 S14 S15 S16 S17 S18 S19 S22 … S28`). The
S16 row (`0x1036b9c8`) is **not** guarded by an `r9` compare: it is guarded by
`cmpwi 7, 11, 0` at `0x1036b9bc`, where `r11` is set at `0x1036b9a0`-`0x1036b9b8`
to 1 only when `r9 == 20` **and** `obj+0x118 < 3`. So:

- `r9 == 34` → `S28` (the arm the abort currently lands on)
- `S16` needs `r9 == 20` and `obj+0x118 >= 3`

Shim set to write `20` at both `+0x44C` and `+0xA10` with magic `0xFA87B921`:
still `ZP100`, still no AVTransport. So the MDP code is not reaching the switch —
the shim path is not the one that feeds it, or `obj+0xE8` is overwritten before
the switch runs.

#### Breakpoints: neither the wrapper nor the switch runs

With gdb attached from boot (`-S`), breakpoints at `0x1036b2e8` (right after
`hal_flash_get_mdp2_wrapper`) and `0x1036b424` (first compare of the model
switch) are **never hit** — not during startup, and not on a
`GET /xml/device_description.xml`. The daemon stays up and keeps listening; the
model stays `ZP100`.

So the shim symbol is not the path that feeds the model, and the switch function
is not on the startup or description path either. The MDP payload added to the
shim (`magic 0xFA87B921`, code at `+0x44C`/`+0xA10`) is therefore inert until the
real caller is found.

Next: find who calls the function containing `0x1036b424` at all (it has no
`bl` callers — check for a vtable slot or a timer), and separately which of the
eight shim names is actually `dlsym`'d (have the shim log each entry point to a
file).

#### libflash is never loaded — the shim is moot

With a logging shim (each of the eight entry points appends its name to
`/tmp/shim.log`), the file is **never created**, and `/proc/<pid>/maps` contains
**no `libflash` mapping**. The daemon runs and serves without it.

So the daemon never `dlopen`s libflash in this configuration: the model does not
come from the shim, and the MDP payload added there is dead weight. All shim work
is moot until something makes the dlopen happen.

The switch function itself starts at `0x1036b298` (frame `-4128`) and has **156
`bl` callers** — it is a general-purpose routine, and `0x1036b424` is a deep
sub-path, not the entry. That is why the breakpoint there never fired: the
function runs often, the model-switch block does not.

#### obj+0x114 is the guard, and it is code-set, not config-set

`0x1036b3cc: lwz 11, 276(31)` loads the guard; `0x1036b404: addi 0,11,-16;
cmplwi 0,1` lets the switch run only for **16 or 17**. The field is zeroed by the
block initialiser at `0x10289268` (which clears `0x100`-`0x118`) and written by
~38 sites — a device-class value, not a config key. No conf key feeds it.

Practical route: patch `0x1036b3cc` from `lwz 11, 276(31)` (`817f0114`) to
`li 11, 16` (`39600010`) so the switch block is always entered. Note `r9` at the
switch is a boolean set by the guard, not the model code — the code comparisons
come later in the chain.

#### Forcing the guard does not change the model

`0x1036b3cc` patched to `li 11, 16`: no abort, but the Server header is still
`(ZP100)` and the description still has no AVTransport. So the guard is not what
selects the model — the chain lands on the ZP100 arm either way.

The switch function is called from the Server-header builder at `0x10369064`
(`li 3, 784` → allocator `0x10434640` → `bl 0x1036b298`), and its return value is
the model name formatted into `Linux UPnP/1.0 Sonos/%s (%s)` (`0x104f62a0`) at
`0x10369090`. So the name is a *return value* of that function, produced by the
arm taken inside it — the selection is the later code comparison, not the guard.

#### The switch arms do not set the Server-header model

NOPing the `r9 != 20` branch at `0x1036b9a8` (forcing the S15/S16 arm) also leaves
the header at `(ZP100)` and the description without AVTransport.

Reason: in the header builder the model name is **`r30`, the function's own
parameter** (`0x10369078: mr 6, r30`), formatted at `0x10369090`. The
`bl 0x1036b298` at `0x10369064` fills a freshly allocated 784-byte object
(`li 3, 784` → `0x10434640`), it does not produce the name. So the switch arms
configure that object; the name comes from whoever calls the header builder.

Next: find the caller of the builder and what it passes in `r30`.

#### The header model is a config string, not the switch

`r30` in the builder is a **local stack buffer** (`addi 30, 1, 16`), filled by
`0x1038bb38`. That getter reads the config key **`/VERSION`** (`0x1043d1c4`) and
falls back to the global buffer at `0x10644bf4`. The switch call only configures
the freshly allocated object; it never produces the name.

The guest has no `VERSION` key, and adding `VERSION S16` to `anacapa.conf` did
not change the header (still `ZP100`), so the conf is not the store the getter
reads — it wants the persisted `/VERSION` property.

So the model name and the model *code* are separate: the code drives the switch
(object config), the name comes from `/VERSION`. AVTransport registration is
tied to whichever of the two actually gates the service list.

#### Readers of obj+0x11C (28 sites) — none is a range gate

`lwz ...,284(rX)` occurs 28 times. The two nearest the description path are:

- `0x103d8ef8` — reads `+0x11C` and `+0x118`, adds them and shifts (`srwi 9,9,1`)
- `0x1014b294` — `not` + `srwi 31`, i.e. a `code != 0` boolean, then a `strcmp`

Neither compares the code against a range, so there is no range gate to force.
The field is used as a boolean/offset by its readers, not as a model selector —
which is consistent with the arm patches changing nothing.

The service list is therefore gated by something else (a flag or a different
field), not by `obj+0x11C`.

#### The description is a static file — there is no service-list flag

`/xml/device_description.xml` is a **static file** (`/opt/htdocs/xml/device_description.xml`,
3661 bytes, read-only, **0** occurrences of AVTransport). The service list is not
built at runtime, so no flag gates it and there is nothing to force.

Adding an AVTransport `<service>` block to the file makes it appear in the
description, but `POST /MediaRenderer/AVTransport/Control` still answers **405
with `Allow: GET, HEAD`** — the path is served as a static path, so the control
handler is not registered either.

So the missing AVTransport is two separate gaps: the static description, and the
runtime route registration. Editing the file closes only the first.

#### AVTransport IS in a route table — it is just not activated

There are two route tables. The extra-routes table at `0x1052d300` (32-byte
entries `{name, handler, 6 zeros}`) holds `/audio_tap` (`0x1052d338` →
`0x1000abd8`) and `/advconfig` (`0x1052d354` → `0x1028a724`).

The MediaRenderer services live in a second table at `0x1052ce68`, and
`/MediaRenderer/AVTransport/Control` **is** there (`0x1052ce84`) with handler
`0x1028a478` — the same handler as RenderingControl (`0x1052ce6c`) and
GroupRenderingControl (`0x1052cea4`).

But the route is not active: `GET` → **404**, `POST` → **405** (`Allow: GET, HEAD`,
i.e. the static handler answers). So the MediaRenderer table is registered
conditionally and that condition is the gate — not the model code, not the
description file.

Next: find what walks the `0x1052ce68` table and the branch that skips it.

#### The service table is halfword-indexed, not walked

The MediaRenderer/MediaServer table starts at `0x1052cd18` (28-byte entries
`{name, handler, 6 zeros}`, all sharing handler `0x1028a478`) and is reached at
`0x103d0914` (`addi 18, 18, -13032` → the base). It is not linearly walked: the
code does `lhax 14, 10, 18` at `0x103d0930`, a **halfword offset lookup** into
the table, so entries are selected by an index, not iterated.

That means the gate is the *index* the caller passes, not a branch around a loop.
AVTransport sits at `0x1052ce84`; whatever index maps to it is what the caller
must supply.

#### The index is a caller-supplied loop value

`0x103d08cc` takes the index in `r10` (7th argument), scales it (`mulli 10,10,6`)
and uses it as the halfword offset into the table (`lhax 14,10,18`). Its only
caller is `0x103c6710`, where `r10` is `r30` — a pointer/counter advanced by 4 in
the enclosing loop at `0x103c6094`-`0x103c60bc`.

So the service set is whatever that loop enumerates; AVTransport is absent from
the enumeration, not filtered by a branch. The enumeration itself lives in the
`0x103c5f..` region.

#### The service list is object data: count at +0x140, array at +0x770

The loop's bound and source are both arguments, supplied at `0x103c5c74`-`0x103c5c98`:

```
103c5c74: lwz 8, 320(31)      ; count = *(obj+0x140)
103c5c80: addi 4, r29, 1904   ; source array = obj+0x770
103c5c84: slwi 0, 8, 2
103c5c8c: lwzx 7, 21, 0       ; element = *(base + count*4)
103c5c90: subfic 11, 8, 0
103c5c94: adde 8, 11, 8
103c5c98: addi 8, 8, 1        ; bound = (count != 0) + 1
```

So the service set is **runtime data on the object**, not a constant list: the
count lives at `obj+0x140` and the index array at `obj+0x770`. AVTransport is
missing because it is not in that array — the array is populated elsewhere, and
that population is the real gate.

To add AVTransport: append its index to the array at `obj+0x770` and bump
`obj+0x140` (or patch the count/array in the guest).

#### Service-list proof attempt: breakpoint not reached

Breakpoint at `0x103c5c9c` (immediately before the list build call) was **not
hit** on a `GET /xml/device_description.xml`, so the list built there is not the
one the description uses — the description is the static file, and this list
feeds something else (likely the UPnP service registration, which is exactly what
is missing).

The index encoding is also not yet pinned: `mulli 10,10,6` then `lhax 14,10,18`
over a table whose first words are `0x1043d7c0`-style pointers, so `index*6` is
not a plain 6-byte stride and needs decoding before an index can be appended.

Next: break where the *registration* happens (the caller of `0x103c5fcc` at
`0x103c5c9c` is one; `0x103c5cd0` the other), and decode the index encoding from
the table dump before writing anything.

#### Correction: the `lhax` table is 0x1050cd18, not 0x1052cd18

`lis 18, 4177` is `0x1051`, and `addi 18, 18, -13032` gives **`0x1050cd18`** — I
had been reading `0x1052cd18`, which is the service-entry table and a different
structure.

At `0x1050cd18` the layout is 6-byte records of halfwords and `lhax` reads the
first halfword of record `index`, which is a signed offset from the base:

```
idx 0 → 0x5fe4 → 0x10512cfc
idx 1 → 0x5bb9 → 0x105128d1
idx 2 → 0x578e → 0x105124a6
```

Those targets hold **float arrays** (`0x3f04b9ad` = 0.51…), so this table is a
float/coefficient table, not the service list. The service entries are the
28-byte `{name, handler, 20 zeros}` records at `0x1052cd18`.

So `0x103d08cc` indexes a float table; the service registration path is a
different function and still needs locating.

#### Record table start found; walker still not located

The 28-byte record table starts at **`0x1052cd18`** — first entry
`/AlarmClock/Control` → `0x1028a478`, preceded by zero padding and, before that,
an unrelated `/diag` → `0x1028be00` entry at `0x1052cce0`.

The table has **no direct data reference**. The only `-13032` (`0xcd18`)
constructions are `0x10267620` (base in `r3`, a parameter) and `0x103d0914`
(which turned out to be the float table). Neither is a walker of this table.

So the registration path is still unfound: the table exists, is uniform, and is
referenced only by computed address from a register — not by a `lis`/`addi` of
its own base.

#### The enumeration DOES include AVTransport

The list is a hardcoded store sequence at **`0x100bd0a8`** (frame `-320`), building
an array of name pointers on the stack. Extracted in full:

```
/AlarmClock/Control            /MediaRenderer/RenderingControl/Control
/AudioIn/Control               /MediaRenderer/AVTransport/Control   ← present
/MusicServices/Control         /MediaRenderer/GroupRenderingControl/Control
/QPlay/Control                 /MediaRenderer/Queue/Control
/MediaServer/ConnectionManager/Control   /MediaRenderer/VirtualLineIn/Control
/MediaServer/ContentDirectory/Control
```

So AVTransport **is** enumerated — the gate is after the list build, not in it.
The list is built and then something filters or fails before registration.

Next: follow what consumes this stack array (the call after the stores) and where
the entries are dropped.

#### The consumer is a lookup, not a registration loop

At `0x100bd3a8` the loop bound is **11** (`cmplwi 7, 29, 11`) — all entries are
visited. Each iteration strcmps the request path (`*(r27+8)`) against the entry
name (`*(r28+12)`) at `0x100bd39c`; on a match it branches to `0x100bd424` and
skips the call. When the loop finishes without a match it calls
**`0x102b0058`** at `0x100bd3cc`.

So the drop condition is the strcmp at `0x100bd39c`: **match → `0x100bd424`,
no match → `0x102b0058`**. Nothing is filtered from the list; the list is only
used to classify the request path.

`0x102b0058` is therefore the thing that decides what happens for a path that
matched no service — and for `/MediaRenderer/AVTransport/Control` that is the
branch actually taken, since the route answers 404.

#### The lookup at 0x100bd39c is not on the request path

Breakpoint at `0x100bd39c` was **not hit** by a
`POST /MediaRenderer/AVTransport/Control`. So the 11-entry lookup is not what
classifies incoming requests — it runs somewhere else (likely a one-off service
inventory build, not the per-request dispatcher).

That means the 404/405 for the control URL comes from a different dispatcher
entirely, and the whole `0x100bd0a8` path is a side structure.

Next: find the actual request dispatcher — the code that maps a URI to a handler
per request — and break there instead. The route tables at `0x1052cd18` /
`0x1052d300` are the data it must consult.

#### The tap handler breakpoint does not hit either

Breakpoints at `0x1000abd8` (the descriptor's handler pointer) and at its
runtime-mapped twin `0x40abd8` both **fail to hit** on a
`GET /audio_tap?tap=test`, while the route answers 404 and the daemon is alive
(11 threads).

So the descriptor's function pointer is not the code that runs for the request —
either the descriptor table is not the live routing table, or the handler is
reached through another indirection that rewrites the address.

That is the current wall: three different "obvious" call paths (the 11-entry
lookup, the descriptor handler, its runtime twin) all fail to fire for a request
that demonstrably reaches the server and returns a response.

#### 404 producer not located

The 404 body (`<HTML><HEAD><TITLE>Error 404</TITLE>`) is neither a file in
`/opt/htdocs` nor a literal in the binary — `strings` finds no "404" and no
"Error 404". The nearest templates are `HTTP Error %d` (`0x1043e710`, no direct
reference) and a set of lowercase `<html><head>` pages.

So the error page is assembled from parts (likely `Error %d` with 404) and the
builder was not found in the time available. The dispatcher-by-behaviour probe
therefore still stands as the next step: break on the code that formats the
error body, not on a string that does not exist.

#### The error formatter is not hit either

The 404 page template is at `0x10523e94`:
`<HTML><HEAD><TITLE>Error %d</TITLE></HEAD><BODY><H1>Error %d</H1><P>%s</P></BODY></HTML>`.
Its only construction is `0x1040cc24` (inside the function at `0x1040cbd0`), used
by the `snprintf` at `0x1040cc38`.

Breakpoint at `0x1040cc38` on a `GET /audio_tap?tap=test`: **not hit**, even
though the response is a 404 built from that exact template.

So the 404 for this request does not come from that formatter either. Four
anchors have now failed to fire on a request the server demonstrably answers:
the 11-entry lookup, the descriptor handler, its runtime twin, and the error
formatter.

That pattern — every candidate address cold on a live request — points at the
breakpoints themselves rather than at the code: the daemon's text is mapped at
`0x00400000`-based addresses per `/proc/pid/maps`, while the file vaddrs are
`0x10000000`-based, and gdb accepts both without error. A breakpoint that is
silently inserted at an unmapped alias would never fire. Verifying one known-hot
address (e.g. the trace logger `0x10415590`) would settle whether breakpoints
work at all in the current setup.

#### Breakpoints work — but only when armed before the target runs

Sanity check: `break *0x10415590` (trace logger) **fires** when gdb is attached to
a target stopped at boot (`-S`). So the address form is correct and the vaddr
aliasing theory was wrong.

What was actually broken: **attaching to an already-running target does not arm
breakpoints.** gdb reports them as inserted, but they never trap. Every "not hit"
result taken that way is unreliable.

Redone properly (armed at boot, then request sent):
- `0x1040cc38` (404 formatter) — still **not hit**
- `0x1000abd8` (/audio_tap handler) — still **not hit**

So those two negatives are real: the handler is genuinely never called for
`/audio_tap`, and the 404 does not come from that formatter.

Rule for the rest of the lane: always start the VM with `-S`, arm breakpoints,
then `continue`.

#### Response-write anchor: neither send nor write fires

With the `-S` rule applied (armed at boot, then request sent), breakpoints on the
`send` slot (`0x10546c98`) and the `write` slot (`0x10546240`) are **both cold**
for a `GET /audio_tap?tap=test` that returns 404.

So the response does not leave through either PLT slot — it must use `writev`,
`sendto` (`0x105465f8`), or a buffered path that reaches the socket another way.

That is the next anchor to try. The pattern so far: every candidate address is
cold, so the remaining approach is to break on `sendto`/`writev` and, failing
that, on the socket accept path and walk forward instead of backward.

#### No fork — the daemon serves in-process

`set follow-fork-mode child` + `set detach-on-fork on`, breakpoint on `write`:
the breakpoint fires, `info inferiors` shows a **single** inferior (`process 1`),
and no new inferior appears. So the daemon does **not** fork per connection; the
fork hypothesis is dead.

`write` is the response path (the slot fires), and the first hits carry
`lr = 0x10415430` — the trace logger — with `r5 = 1`, i.e. single-byte trace
writes. The response write is a later hit with a socket fd and a large length;
it must be filtered for, not taken first.

So the earlier four cold results were the arming bug, not a fork: with `-S` and
breakpoints armed at boot, `write` fires normally.

#### Length filtering does not separate the logger from the response

`break *0x10546240 if $r5 > 64` hits immediately on the trace logger
(`lr 0x1041541c`, fd 9/11, `r5 = 0x41` = 65). The logger writes in 65-byte
chunks, so a length filter alone cannot isolate the response.

`if $r5 > 200` is too high — the 404 body is only ~100 bytes, so it never fires.

The discriminator has to be the **fd** (a socket, not the log fd 9/11) or the
**LR** (anything other than `0x1041541c`), not the length.

#### Response emitter found: write-all helper at 0x10406808

With the LR filter excluding both logger sites (`0x1041541c`, `0x10415430`), the
first non-logger hit is the response:

```
lr 0x1040689c   r3 4 (socket fd)   r5 0x96 (150 bytes = the 404 body)
```

The emitter is a write-all loop:

```
1040687c: add 30, 30, 3        ; advance
10406880: sub 31, 31, 3        ; remaining
10406888: bt 30, 0x104068cc    ; done
1040688c: lwz 3, 0(29)         ; fd
10406890: mr 4, 30             ; buf
10406894: mr 5, 31             ; len
10406898: bl write
1040689c: cmpwi 3, 0
104068a0: bf 0, 0x1040687c     ; loop on short write
```

Function start `0x10406808`; it has **no `bl` callers** (pointer-reached, as with
everything else in this server).

So the response path is: write-all helper `0x10406808` → `write`. Walking up from
its caller is the remaining step, and the caller is reached by pointer, so it
needs a breakpoint on the helper entry rather than a call-site search.

#### One hop up: the response writer at 0x104118b8

Breaking on the emitter entry (`0x10406804`) and reading the LR gives the caller
directly:

```
lr 0x10411af4   r3 0x10649bb8 (global buffer)   r5 2
```

The call site is `0x10411af0: bl 0x10406804`; the caller snprintf's into the
global buffer at `0x10649bb8` and then writes it. That function starts at
`0x104118b8` (frame `-1104`) and has **no `bl` callers** — pointer-reached, like
every other layer here.

So the chain is now: `0x104118b8` (response writer) → `0x10411af0` →
`0x10406804` (write-all) → `write`. The dispatcher is one more hop up, and the
same technique applies: break on `0x104118b8` and read the LR.

#### 0x104118b8 is only entered at startup, not per request

Breakpoint on the writer entry `0x104118b8`: one hit at startup with
`lr 0x10007a74` (the config/startup region), then **nothing** for the request.

But the request demonstrably passes through `0x10411af0`, which is *inside* that
function. So the function is entered at a point other than its prologue — it has
multiple entry points, like the model switch earlier.

So breaking on the prologue is the wrong anchor. The right one is the call site
already known (`0x10411af0`), or the function's internal dispatch head.

Chain so far, all confirmed by LR: `0x10411af0` → `0x10406804` → `write`. The
dispatcher above `0x10411af0` is still unread.

#### 0x10411af0 also does not hit for the request

Breakpoint at the call site `0x10411af0`: one startup hit with `lr 0x10411ae0`
(the function calling itself), then nothing for the request — even though the
emitter hit earlier carried `lr 0x10411af4`, which is the return address of that
exact call.

So the call executes but the breakpoint at its address does not trap. Two
possibilities: the address is inside an instruction that gdb cannot patch there,
or the response is emitted by a second, identical call site.

Chain confirmed by LR remains: `0x10411af0` → `0x10406804` → `write`. The
dispatcher above is still unread.

#### `finish` works where breakpoints do not — chain reached libc

`finish` from the emitter hit returned to `0x1040f540`, immediately after
`bl 0x10406804` at `0x1040f53c` — a response writer that formats into a stack
buffer and writes it.

A second `finish` returned to **`0x0fe03f04`**, in the libc region. So the writer
is invoked from libc, i.e. through a callback/trampoline, not from a direct call
in the daemon's own code.

Chain, all by `finish`/LR:

```
libc 0x0fe03f04  →  0x1040f53c  (response writer)  →  0x10406804  →  write
```

`finish` is the reliable technique here; address breakpoints keep failing to trap
even on instructions that provably execute.

Next: identify `0x0fe03f04` in libc (which callback slot it is) — that names the
framework the daemon registers its handler with, and from there the chooser.

#### Handler frame identified; the libc caller is not in uClibc

The response writer's function starts at **`0x1040f3c4`** (frame `-448`); the
write call is at `0x1040f53c` inside it. That is the handler that produced the
response.

Its caller came back as `0x0fe03f04`, which is **not** in the uClibc mapping
(`0x77ca7000`-`0x77cef000`) — so it is not a libc callback after all. `0x0fe0xxxx`
is a separate mapping that has not been identified.

Breakpoint at `0x1040f3c4` itself did not trap for the request (same failure mode
as the other entries); the frame was reached only via `finish` from the emitter.

Chain, all by `finish`/LR:

```
0x0fe03f04 (unidentified mapping)  →  0x1040f53c (handler 0x1040f3c4)  →  0x10406804  →  write
```

Next: resolve what is mapped at `0x0fe0xxxx` — that is the framework calling the
handler, and the chooser sits there.

#### 0x0fe0xxxx is not in the current maps

The live process maps only `0x00400000` (text), `0x10000000` (data),
`0x1000f000` (heap) and `0x77c…` (uClibc and friends). There is **no
`0x0fe0xxxx` region**, so the return address `0x0fe03f04` cannot be resolved
against this process.

Either the address came from a different process/boot than the one now running,
or it is a stack-resident trampoline. Resolving it needs the maps taken in the
same session as the capture.

Method for next time: at the `finish` that yields the caller, immediately read
`/proc/<pid>/maps` and match the address before detaching.

#### 0x0fe03f04 is a physical address, not a virtual one

Read in-session at the `finish` (51 map lines, full dump): there is still no
`0x0fe0xxxx` region. Every mapping is `0x00400000`, `0x10000000`, `0x1000f000` or
`0x77c…`-`0x77e…`.

So `0x0fe03f04` is a **physical** address. gdb in this setup reports physical PCs
in some modes (seen earlier: `0x64231910`, `0x40e71810`), and the `finish` return
value inherits that. It cannot be matched against `/proc/pid/maps`.

To get the virtual caller, translate it with the monitor (`gva2gpa` works the
other way; for physical→virtual the CPU's current context is needed) or read the
saved LR from the stack frame instead of trusting the reported PC.

#### Full chain from the stack (no breakpoints)

Dumping the stack at the write hit yields the return-address chain directly.
Words in the binary's range, in stack order:

```
0x1040f5d4   in the response writer (fn 0x1040f3c4)
0x1036b2d8   in the model/MDP function (fn 0x1036b298)
0x1036c418   same function
0x1028d208   MDP write path (fn 0x1028d2xx)
0x10008a1c   early init
0x104175a8   unclassified
0x1040fc98   unclassified
0x10411b70   in the writer function (fn 0x104118b8)
0x10007a74   startup
0x100086c4   startup
```

`0x0fe03f04` also appears, confirming it is the immediate caller of the writer
and that it is a physical address (no mapping).

So the response path runs through the model/MDP code (`0x1036b298`, `0x1028d2xx`)
before reaching the writer — the same function that aborted earlier. That ties
the model question and the response path together.

#### The decision: flags at obj+0x170, bits 0x20 / 0x03

`0x1036c418` is the return of `bl 0x1036b298` — a lazy getter that allocates 784
bytes, calls the switch, and caches the result in the global `0x10545ee8`.

`0x1028d208` then reads that cached object and tests a **flags field at
`obj+0x170`**:

```
1028d22c: lwz 0, 368(9)      ; flags = *(obj+0x170)
1028d230: andi. 9, 0, 32     ; bit 0x20
1028d234: bf 2, 0x1028d2f4   ; set → model path
1028d238: andi. 9, 0, 3      ; bits 0x03
1028d23c: bc 13, 2, 0x1028d2f4
```

If either bit is set it takes the path that builds the model name
(`0x1038bb38`) and logs; otherwise it logs **`MDP flag update`** (`0x1049319c`)
and skips.

So the gate is **`*(obj+0x170) & 0x23`** — a flags word, not a model code. The
model name and the service set both hang off it.

#### The flag setter writes 0x0F

`0x1005dc00: stw 0, 368(31)` with `li 0, 15` at `0x1005dbfc` — the field is set
to **0x0F**, which includes bits `0x03`, so the model path is taken.

The setter sits in the function at `0x1005dab8`, right after two calls
(`0x102f31a0`, `0x102f3938`) that initialise sub-objects at `+0x24A0` and
`+0x43D8`. So the flag is raised as part of that object's setup, not by a
request-time condition.

The same field is also used as a counter elsewhere (`0x10161fe8: addi 9,9,1` with
stride 204), so the struct is reused for two purposes — worth keeping in mind.

Next: find who calls `0x1005dab8`; that is the point where the model path becomes
reachable at all.

#### The flag setter is dead code — never called

`0x1005dab8` (the function that writes `0x0F` to `obj+0x170`): **0 `bl` callers,
0 data references**, and it is a proper function entry (`stwu`/`mflr` prologue at
`0x1005dab8`, `blr` at `0x1005dab4` before it). Nothing calls it.

The zero writer's function `0x10161350` has 3 `bl` callers, so that one runs.

So the pair resolves cleanly: **the flag is never set because the only code that
sets it is unreferenced.** The zero writer then has nothing to clear.

That is the root cause of the whole model/service chain: `obj+0x170` stays 0, the
model path at `0x1028d234`/`0x1028d23c` is never taken, the model name is never
built, and the service set never gains AVTransport.

It is the same defect class as the unwired advance path — a function present in
the image with no reference to it.

#### Forcing the flag bits does not activate the route

`0x1028d22c` patched from `lwz 0, 368(9)` (`80090170`) to `li 0, 3`
(`38000003`), forcing the model path unconditionally: no abort, but the Server
header is still `(ZP100)` and `POST /MediaRenderer/AVTransport/Control` still
answers **405**.

The description does list AVTransport now — but that is only the earlier file
edit, not the daemon.

So `obj+0x170` is not the gate for the route either. The model name and the
service registration are further apart than the flag suggests.

#### The 0x1052cd18 service table is itself unreferenced

The only two `-13032` (`0xcd18`) constructions point elsewhere:

- `0x10267620` with `lis 3, 4169` → **`0x1048cd18`** = `"pramp iirblock"` (DSP)
- `0x103d0914` with `lis 18, 4177` → **`0x1050cd18`** (the float table)

Neither is `0x1052cd18`. As a data word it has no reference either.

So the MediaRenderer service table is **dead data** — nothing reads it. That is a
third dead item in this area, alongside the flag setter `0x1005dab8` and the
advance path.

The live routing table is the other one (`0x1052d300`, which holds `/audio_tap`
and `/advconfig` and does work). AVTransport lives only in the dead table.

#### AVTransport added to the live table — still 405

The live table `0x1052d300` uses 28-byte entries `{name, handler, 5 zeros}` and
terminates at `0x1052d610` (all-zero record). The terminator was overwritten with
an AVTransport entry (`0x1043d8f8`, `0x1028a478`, 20 zeros).

Result: no abort, but `POST /MediaRenderer/AVTransport/Control` still answers
**405**.

So the live table is not the dispatcher either, or the walker terminates by a
count rather than the zero record (in which case the extra entry is ignored).

#### Stack-LR walk: breakpoint did not hit this run

Re-armed at `0x10406804` with `$r5 > 64` and `-S`: the breakpoint did not hit for
the request (interrupt landed in the idle task). No frame dump obtained.

Note the 405 observation is important and stands: **405 means the static handler
matched the path**, so a lookup before it fails and falls through. That is
consistent with everything seen — the path is being resolved by a table that does
not contain the entry, then handed to the static handler, which rejects the
method.

So the remaining work is to find that failing lookup, and the stack-LR walk is
still the right technique; it just needs a run where the breakpoint fires.

#### Unconditional breakpoint also cold; the daemon still serves

Breakpoint at `0x10406804` with no condition, armed at boot: not hit for the
request. The daemon is healthy throughout (11 threads, `/status/zp` returns 200).

So the emitter is not on the path for these responses either, or the breakpoint
mechanism has degraded again. Both have been true at different times in this
session, and the two are now hard to separate without a control.

Control to use next: break on `0x10415590` (the trace logger) in the same run as
the emitter breakpoint. If the logger fires and the emitter does not, the emitter
is genuinely off-path; if neither fires, the stub is not arming and the run is
void.

#### Control confirms the run is void: the stub is not arming

Same run, both breakpoints armed at boot: neither `0x10406804` (emitter) nor
`0x10415590` (logger control) fires, while the daemon serves normally.

So the stub is not arming breakpoints in this configuration any more. Every
"not hit" from these last runs is void, and the earlier cold results need
re-testing before any conclusion is drawn from them.

The control is mandatory from here: always arm the logger alongside the target
address. If the logger is cold, discard the run.

#### Stub proven alive; the emitter is genuinely off-path for this request

Fresh qemu, fresh gdb, logger control first: `break *0x10415590` **fires**
immediately. The stub arms normally.

Then, same session, `0x10406804` (emitter) armed and the request sent: **cold**.

So with a proven-live stub, the `/audio_tap` response does not go through that
emitter. The earlier hits there (LR `0x10411af4`, 150-byte body) must have been a
different response — most likely the 404 page for a *different* path, or a
startup-time write.

That invalidates using the emitter as the anchor for this route. The chain
`0x10411af0 → 0x10406804 → write` is real but belongs to whichever response it
was captured on, not necessarily to `/audio_tap`.

#### The 405 page is not built by 0x1040cc38 either

Control fired (stub alive), then the formatter `0x1040cc38` armed and the request
sent: **cold**.

So the 405 body comes from somewhere else again. The status text is looked up from
a table of `{text, code<<16}` pairs at `0x105243e0`-ish (`Forbidden`/0x1940000 =
404, `Method Not Allowed`/0x1950000 = 405), so the producer is generic and keyed
by status code — the text is not formatted inline.

Every "obvious" anchor for this response has now been cold with a live stub:
emitter `0x10406804`, formatter `0x1040cc38`, handler `0x1000abd8`. The response
is produced by a path none of them are on.

#### Status table structure; reader not located

The table is regular `{text, code<<16}` pairs covering 0x130, 0x131 and
0x190-0x19b (400-411): `0x194` = 404 `Forbidden`, `0x195` = 405
`Method Not Allowed`.

The base is at or before `0x105243b0` and has **no data reference**, and no
`lis`/`addi` to it was found with the obvious immediates. So the reader is
reached by a computed base like everything else here.

Not located this round.

#### strtok_r is cold too

Control fired (stub alive), then `strtok_r` (`0x10546880`) armed and the request
sent: **cold**. So the request line is not split with `strtok_r` either.

Remaining parse candidates: `sscanf` (`0x10546c78`), `strchr` (`0x10546158`),
`strstr` (`0x10546840`), `strtok` (`0x10546db8`).

At this point every anchor tried has been cold with a live control — emitter,
formatter, handler, strtok_r. That is too consistent to be coincidence: the
request is being handled by code that none of the identified structures sit on,
which strongly suggests the daemon serves through a **different HTTP stack** than
the one these tables and handlers belong to (possibly a second, newer server
compiled in, with the old one's tables left dead).

Test for that: break on `accept`/`recv` and walk *forward* from the socket read,
rather than anchoring on any handler or table.

#### recv and recvfrom are both cold

Control fired, then `recv` (`0x10546e00`) armed: cold. `recvfrom` (`0x10546260`):
cold too.

A server that answers a request must read the socket, so one of these should
fire. That leaves `read` (`0x10546ea8`) — the daemon likely reads the socket with
plain `read`, which is also why the trace logger shares it.

So the second-server hypothesis is not yet tested; `read` is the remaining
syscall to try, filtered by fd (the socket, not the log fd 9/11).

#### gdb sees exactly one thread — that explains every cold anchor

At the logger control hit, `info threads` reports:

```
  Id   Target Id                    Frame
* 1    Thread 1.1 (CPU#0 [running]) 0x10415590 in ??
```

**One thread only**, while the daemon runs 11. So gdb follows a single thread and
breakpoints only trap that one; the request is served by a thread gdb never sees.

That is the whole explanation for the cold anchors — emitter, formatter, handler,
strtok_r, recv, recvfrom. They were never on the followed thread's path. The
second-server hypothesis is unnecessary.

Fix options: per-thread breakpoints (`break ... thread N` after selecting the
serving thread), or a stub that reports all threads. The qemu gdbstub here
appears to expose only CPU#0.

#### Monitor has no thread list — and there is only one vCPU

`info threads` is not a monitor command. `info cpus` reports:

```
* CPU #0: thread_id=3908928 model=7400_v2.9
```

**One vCPU.** So gdb's "Thread 1.1" is the vCPU, not a guest thread, and the
daemon's 11 threads are all multiplexed onto that single CPU.

Consequence: a breakpoint at an address *should* trap whichever guest thread
executes it, so per-thread breakpoints are moot and the threading explanation for
the cold anchors does not hold after all.

The cold anchors remain unexplained. What is left: the stub may only honour
breakpoints in the context it is currently following, or the addresses are wrong.

#### Discriminator result: the addresses were wrong, the tooling is fine

`read` (`0x10546ea8`) **fires** — and it fired *before* the logger control, i.e.
on ordinary startup traffic. So breakpoints do trap executed code.

Therefore `recv`/`recvfrom` being cold means the server does not use them: it
reads the socket with plain `read`. The tooling is sound; those addresses were
simply wrong.

The socket read goes through a read-all helper mirroring the write-all one:

```
10406934: lwz 3, 0(28)      ; fd
10406938: mr 4, 29          ; buf
1040693c: mr 5, 30          ; len
10406940: bl read
10406944: mr. 31, 3         ; loop on short read
```

Helper entry `0x104068f4`. At the request hit the fd was 10 and the LR
`0x10406944`.

So the read side is now anchored exactly like the write side. Breakpoint at the
helper entry `0x104068f4` did not hit on the request in this run (buffered
commands), but `read` itself is a reliable anchor: filter by fd != 9/11.

#### fd-filtered read breakpoint hits on the request

`break *0x10546ea8 if $r3 != 9 && $r3 != 11` fires on the request (twice seen),
so the socket read is now a working anchor and the fd filter keeps the logger out.

`finish` from the hit was not captured — gdb queued the command while running and
the interrupt landed in the kernel (`0xc004c684`). The pattern to use is: wait for
the "Breakpoint N" line to appear, then send `finish` (never queue it while
`Continuing`).

So the read side is anchored and proven. The next run needs only the finish to
land to yield the request handler.
