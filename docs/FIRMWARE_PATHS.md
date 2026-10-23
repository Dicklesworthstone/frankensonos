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
self-signable.

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
   `re-afelen-live` / `re-hls-kill`. **It is not S1-only:** the same playlist on
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
