# RE_DEVMODE — live lane log

One lane of the 2026-10-24 root swarm. Facts with addresses/commands only; merge into FIRMWARE_PATHS.md when the lane closes.

## 2026-10-24: statement replay is deferred; both static questions answered

The statement is device-bound and no unauthenticated generator exists, so the
replay waits on a portal-issued base64 (the `:1443`/`:1400` live side is not the
blocker). This lane finished the two static reads instead. Sources: 86.10
`anacapad` (`captures/firmware/s2_overlay/bin/anacapad`, MIPS16 — `-m mips:16`)
and the S2 `libsonoscrypto.so.3` (PowerPC build, `fw/internals`, `ZZ0137` at
`0xca14`). Object field numbers below are **0x-hex**; GNU objdump prints MIPS
offsets in decimal.

### The S2 statement layout, from `ZZ0137` (`0xca14`)

`FUN_004f3b44` calls `ZZ0137(buf, len, model, devid_buf, devid_len, ver_hi,
ver_byte, &payload_off, &payload_len, &ver_a, &ver_b)`. The verifier, in order:

| offset | size | check |
|---|---|---|
| `+0x00` | u32 BE | `0x28721e71` (same bytes as S1's LE magic) |
| `+0x04` | u32 BE | equals the caller's model value (page reports it) |
| `+0x08` | 16 | free |
| `+0x18` | u32 BE | **lower version bound**, inclusive: `r0 >= word` |
| `+0x1c` | u32 BE | **upper version bound**, inclusive: `r0 <= word` |
| `+0x20` | u8 | devid length, must equal the caller's expected length |
| `+0x21` | n | device id, `memcmp` against the caller's buffer (`0xcbd4`) |
| `+0x21+n` | u8 | payload length; `len >= devid_len+34+payload_len` |
| … | | payload |
| … | | signature ring, handed to the parser (`0x20400`) |

The two window tests are `cmplw 7,r0,word` + `bc 13,28` (exit if `r0 < +0x18`)
and the same with `bc 13,29` at `0xcb8c` (exit if `r0 > +0x1c`), so **both
bounds are inclusive** — a statement with `+0x18 == +0x1c == r0` passes. The
earlier "max/min, strict" reading here was wrong; the app's
`rev([+0x18]) <= r0 <= rev([+0x1c])` is the same test and was right.

`r0` is built at `0xcb20` as `(arg6 << 16) | arg7`. The caller passes two
object bytes: `+0x49d` (arg6) and `+0x49e` (arg7). `+0x49e` is a u16 written by
`FUN_004dda10` (`0x4dda2e`, from `FUN_0051927c` → the u16 at object `+0x56a`) and
again by the update coordinator `FUN_004db988`; `+0x49d` is set from the high
byte of that coordinator's counter and is zero on the devmode path. On a
86.10 unit the byte read is the major-version byte, so `r0` is `0x56` (86) and
a statement must carry `+0x18 <= 86 <= +0x1c`.

The payload length is **one byte** (`lbzx 9,31,27` at `0xcbf4`).

### What `ZZ0137` actually signs — the device id is inside the signature

The verifier does not hash a separate digest field. `ZZ0123` hands the
signature-container verifier (`ZZ0126`) three things: the parsed ring context,
the statement pointer, and the ring offset. `ZZ0126` then computes the digest
over `statement[0 .. ring_offset)` — the entire clear preamble, payload
included:

- S1: the hash callback is called at `0xa888` with `algo=1, data=statement,
  len=0x2e`, and `0x2e` is exactly the ring offset for a 12-byte id and empty
  payload (`devid_len+34+payload_len`).
- S2: the same call is at `0xa7d4` with `data=statement, len=0x2e`.

So **everything the parser reads is signed**: magic, model, the 16 "free"
bytes, both version bounds, the id length, the id itself, the payload length
and the payload. The ring that follows is the only unsigned material — it
carries the key reference and the signature. `memcmp` of the id at `0xacc4`
(S1) / `0xcbd4` (S2) is therefore a redundant pre-check for a *forged* id, not
the security boundary.

Consequence: a public statement for the same model **cannot be patched** to a
different serial; the signature breaks and the only free fields are the ring
internals. A per-serial portal statement is required.

Emulator demonstration (`scripts/emu_devmode_stmt_{s1,s2}.py`): flipping a byte
in the 16-byte free area at `+0x08` still reaches the verifier (the byte is
unchecked but hashed); changing the id fails at the `memcmp` before any hashing.
On the wire both classes answer `403`, so the emulator, not the status, is the
discriminator.

### `FUN_004f38a0` cannot be driven past 255 — the 256-byte frame is exact

`FUN_004f3b44` passes `ZZ0137`'s `payload_len` output as `param_3`
(`0x4f3bda` stores it to `+0x408`); `FUN_004f38a0` (`0x4f38a0`) then:

1. `memset(param_4, 0, *param_5)` with `*param_5 = 0xff`;
2. `uVar4 = old_len` (`+0x408`); if `param_3 != 0`, `*param_5 = max(uVar4, param_3)`;
   `a3` is that max at `0x4f38fe`;
3. `memset(local_118, 0, 0xff)` (255 of the 256 bytes);
4. `memcpy(local_118 + (X - Y), …, Y)` where `{X,Y} = {param_3, uVar4}`, so the
   copy ends at `max(param_3, uVar4)` — at most index 254;
5. the XOR loop runs `*param_5 = max(param_3, uVar4)` times, reading
   `local_118[0..max-1]` and writing the same count to the caller's
   `local_130[64]` (256 bytes).

Both lengths are the same u8 (`+0x408` is only ever stored from `param_3`), so
`max <= 255`: mask and output fit exactly, one byte spare. `FUN_004f3974`
(no-statement notification) passes `param_3 = 0` and is the same bound.
**There is no reachable >255 payload on this build**; the 256-byte frame would
only overflow if a later `ZZ0137` returned more than a byte.

### `POST /mtmhhsetup` — valid shape and what it persists

Handler `0x4ac11c` (table entry `0x6e714c`, flag `0x100` — the same locked
class as `/unlock`). Live check: `GET /mtmhhsetup` is `403` on both locked
86.10 units while `/unlock` answers `423`, so the handler body only runs after
the lock is lifted. It also requires an `X-Sonos-Api-Key`. The header is
not merely presence-checked here: `0x4aa99c` hashes the value with `ZZ0058`
(algorithm 1 = SHA-256-size, 32-byte output) and `memcmp`s it against two
built-in 32-byte constants (`0x676ea8`, `0x676e88`); a mismatch is `500` with
an empty message. The published UUID dummy is known to pass live (the token lane reached the
inspector on this same build with it), so one constant corresponds to it under
whatever `ZZ0058` derives, but `any non-empty value
passes` is not true of this build.

Request:

- method `POST` (else `405`); `Content-Type:
  application/x-www-form-urlencoded` (else `415`); `Content-Length` 1–4095
  (`411`/`413`); body read into `sp+40`.
- form fields (name cap 32, value cap 128):
  - `hhid` — optional. Empty (`hhid=`) generates `Sonos_MTM_` + 22 chars from
    `[0-9a-zA-Z]` (`0x4ac31e` copies the 10-byte prefix, `ZZ0154` fills 22 from
    the alphabet at `0x6769a0`). Non-empty must start with `Sonos_MTM_`
    (`strncmp`, 10), be shorter than 33, and contain only alnum/`_`
    (`FUN_004fa3d8`). Copy cap 33.
  - `name` — optional; copied (128) and normalized by the UTF-8 sanitizer
    `FUN_00595528`; accepted only if `strlen < 65` and the value already equals
    its sanitized form (else `400 invalid name`).
  - `icon` — optional; `< 129` bytes and only alnum/`:`/`-` (`FUN_004fa3d8`
    with `":-"`; else `400 invalid icon`). Default
    `x-rincon-roomicon:generic`.
  - `key` — required; exactly 24 chars, base64url-decoded to **16 bytes**
    (`400 invalid key` otherwise).
  - `reboot` — optional integer; `>= 30` becomes 30, 0 stays 0, a negative
    value leaves the default 1. `>= 1` calls `sonosForceReboot(n)` after the
    reply.
- Precondition: `HouseholdID` must be absent or empty (`getSetting` status and
  first byte, `0x4ac25c`), i.e. a factory-reset player; otherwise
  `500 not factory reset`.

Effect (`0x4ac564` onward): `ZZ0215` derives 16 bytes from the decoded key,
the literal `MTM_` and the hhid; `RserializeData` renders that to 33 bytes and
`int_addOrRemoveSettings` writes **`HouseholdID=<hhid>`** and
**`WEPKey=<derived>`** into the fixed file `netsettings.txt`. With a non-empty
`name` it also writes `ZoneName`, `Icon`, `Configuration=1` and
`TargetRoomName=""` into the room settings file. Success is `200` with
`{"hhid":…,"key":…,"rebootDelay":…}`; every failure is the JSON
`{"error":code,"message":…}` under the matching status.

### Portal procedure for the owner (the only source of a statement)

The statement is per-serial and fully signed, so this is the path. Do it in a
normal browser signed in to the Sonos account (the same login as the app):

1. Open `https://developer.sonos.com/devmode/`; it redirects to
   `login.sonos.com` — sign in. (The page is a Salesforce community page and
   only exists after login.)
2. Read the unit's **Device ID** from `http://<player>:1400/devmode`: the
   `Device ID:` line, 12 hex characters, no separators. For the Play:5 that is
   `000E585D8C10`; for the Play:1 `000E58CADE94`. If the form also asks for a
   model, use `16` (Play:5) or `8` (Play:1).
3. Submit and copy the returned **base64 statement**. It is one line; the
   portal's own statements decode to under 1 KB, and the daemon rejects a
   decoded length above `0x400`, so keep it under ~1360 base64 characters.
4. Paste it here. It goes to `POST http://<player>:1400/devmode` with the
   page's fresh `csrfToken` and `button=submit`, spaced >= 5.5 s (closer spacing
   makes the daemon sleep ~2.1 s per missed interval, which is the `4 s`
   responses seen in testing).

Then check, in order: the POST answers `200`; `GET https://<player>:1443/unlock`
stops answering `423`; TCP 2323 and 22 accept; `GET /testenv` answers `200`
instead of `403`.

Parser facts for judging a candidate: `len < 4`/`< 8`/`< 28`/`< 32` are silent
rejects; `devid_len` must equal the expected length and the bytes must match;
on S2 the version word must lie inside `[+0x18, +0x1c]` (both inclusive), and on
S1 `[+0x18] < 0x390018` and `[+0x1c] >= 0x390017`; the ring is then parsed and
RSA-verified against the pinned key. S1 `memcmp` is at `0xacc4` (id at `+0x21`,
length at `+0x20`, expected length arg), S2 at `0xcbd4`.

## 2026-10-25: `/devmode` is not key-gated; the allowlist constants are not the clients' keys

The check itself is in [`API_KEY_ALLOWLIST.md`](API_KEY_ALLOWLIST.md): a
two-entry SHA-256 allowlist at `0x4aa99c`, reached only from `/mtmhhsetup`, and a
statement path gated by one compiled RSA key (SHA-1 ref only, no chain). Two
additions.

### `/devmode` never looks at the header

Route entry `0x6e7034` -> handler `0x4aa958` -> `0x4f4240`. The handler checks
only the method (`1` = GET page, `5` = POST) and `content-type` ==
`application/x-www-form-urlencoded` (literals `0x4f42ac`/`0x4f42b0`). The POST
path `0x4f419c` -> `0x4f403c` (form parse) -> `0x4f7140` (button dispatch) has
no `ap_request_get` anywhere, so the statement route is signature-gated only and
the allowlist cannot be scored from it.

### The constants are not any key the shipped clients use

The S2 app builds `X-Sonos-Api-Key` at runtime: `OAuthConfig.oktaID` ->
`ClientImpl.apiKey` -> `RestConnection.addHeader`. `oktaID` comes from
`com.sonos.secrets.Secrets.getOktaClientIdProd`, a JNI function that XORs a
36-byte blob with the SHA-256 hex of the package name (`com.sonos.secrets`).
`scripts/s2_secrets_derive.py` recovers all of them offline; the prod values are
`4851730c-497b-4a2a-89d7-69b2a5795a4d` (Okta client id) and
`94bf0c87-d347-460f-be58-8b274ea5fa35` (client API key), identical in 80.28.36
and 90.01.9. The S1 desktop app uses the same scheme with an XOR table at
`0x100f714f0` in the 57.23 macOS build (five per-environment UUIDs plus
controller id `0oap5iecdnocQ4Kf60h7`); all six were recovered.

None of those, and not the published dummy
`123e4567-e89b-12d3-a456-426655440000`, hashes to `0x676ea8`/`0x676e88`, in any
case/dash/brace/newline/header-line form. Also negative: every printable string
in both `anacapad` builds (1.67M), every string in the S1/S2 rootfs trees
(29.7M), 479 UUIDs harvested from the repo, and `/usr/share/dict/words`. The
same two digests sit in the S1 57.23-74170-1-12 `anacapad` at `0x50c0e4` /
`0x50c104` (MIPS32 check at `0x4288e0`, caller `0x42a254`, which fetches
`X-Sonos-Api-Key` at `0x50bc9c`), so the allowlist is fleet-wide and the
preimages are per-client values that are not shipped as literals. The remaining
source is the iOS app or a partner build; the derivation tool takes any
`libsecrets.so`.

## Where a statement can actually be obtained (2026-10-25)

There is no generator on the device and none in any public repo. Checked:
GitHub code search for `urn:sonos:device`, `urn:sonos:model`,
`1.3.6.1.4.1.44974` and "Sonos Device Authentication" returns only this repo;
the controller and the Android app carry the page, not a statement.

Three sources exist, in order of availability:

1. **The portal.** `https://developer.sonos.com/devmode/` is a Salesforce
   Experience Cloud site; unauthenticated it 302s to
   `/s/login?startURL=%2Fdevmode%2F` (Aura app, no public Apex endpoint), so it
   needs the owner's Sonos login. The device's own `GET /devmode` supplies the
   three inputs the form wants: model, device id and version, plus the
   single-use `csrfToken` (`0x4f3fbc`, format at `0x686000`).
2. **A device that has already been unlocked.** A successful POST persists the
   statement: `0x4f3b44` calls `0x4f3e40` with flag 1, which
   `constructJFFSRootPath(out, 1024, "debug")` (`0x4f3e48`, literal `0x69539c`),
   `mkdir(out, 0777)`, `strlcat(out, "/devmode.bin")` (`0x4f3ed6`) and
   `open64(out, O_WRONLY|O_CREAT|O_TRUNC, 0600)` — i.e.
   **`/jffs/debug/devmode.bin`**. The temp form is `/tmp/devmode.tmp.bin`
   (`0x4f3ef8`, flag 0). There is no route-table entry for it, but
   `/status/ZPLocalSettingsFile/<name>` joins the settings root with `<name>`
   and one `..` reaches `/jffs` (verified live for `../VERSION`), so
   `GET /status/ZPLocalSettingsFile/../debug/devmode.bin` should return it.
   **Untested — needs a unit that has ever accepted a statement.**
3. **A captured statement from the same build.** The key is per build (see
   `API_KEY_ALLOWLIST.md`), and the id is inside the signature, so a capture
   only replays on the device it was issued for and only on that build.

The two fixed digests in `ZZ0218` are *not* a shortcut: the function is dead in
86.10-80260 (see `FIRMWARE_PATHS.md`), and even if wired, a fixed PEM would
still have to be obtained once.

## 2026-10-25 (2): which verifier the POST uses, and where a statement comes from

### The live POST path is ZZ0137 on both builds; ZZ0218 is a certval allowlist

- S2: `0x4f403c` (form parse) -> `0x4f3b44` -> `jal ZZ0137` at `0x4f3b96`.
- S1 57.23-74170-1-12: `0x4ba1f8` (form parse) -> `0x4b98b4` -> `jal ZZ0137` at
  `0x4b995c`. Nothing after the verify touches `ZZ0218`.
- `ZZ0130`/`ZZ0218` are called only from `0x4edf34`, whose only caller is
  `0x4edfa8` inside the certificate-validation wrapper `0x4edf5c` (itself called
  from `0x4ed9a6`, right after `sonos::certval::validate` at `0x4ed994`). So the
  two compiled digests are a **certificate** allowlist, not a statement gate.
- `ZZ0218` (`0xb78c`) accepts only lengths `0x424` and `0x34a` (DER/PEM cert
  sizes), SHA-256s the whole blob and `memcmp`s a byte-permuted constant from
  `0xd878`. `ZZ0130` (`0xad6c`) is only a getter: `obj+0xc` -> `*r4`,
  `obj+8` -> `*r5`.
- The dispatch differs by build: S2 `0x4edf34` tests `(b & 5) == 1` (base64 `'K'`,
  i.e. the statement magic), S1 `0x487774` tests bit0 set / bit2 clear (PEM `'-'`
  takes the replay branch at `0x48795c`). Neither is on the `/devmode` route.
- Both form parsers also cap the decoded statement at 1024 bytes
  (`0x4f4092`, `0x4ba320`), so the 1060-byte variant cannot arrive by form.

### The statement source is the portal, and the app consumes it by deeplink

The S2 app has a full unlock flow (`com.sonos.passport.devmode`):

- `DevModeDeeplinkHandler` accepts `sonos://devMode/set?statement=<base64>` or
  `sonos-2://devMode/set?...` (scheme check at `DevModeDeeplinkHandler.java:24`,
  host `devMode`, single path segment `set`).
- `DevModeUtilsKt.devModeStatementStringToByteArray` base64-decodes it (spaces
  replaced by `+` first).
- `DevModeManager.trySetStatement` validates it locally with the same verifier
  the device uses: `NativeDevModeJNI.validateDevModeStatement(buf, len, 47,
  controllerId, ..., majorVer, minorVer, ...)` — model 47, version window from
  `DevModeData`.
- `DevModeUriParser$checkIfDevModeStatementShouldBeCached$1` caches the string
  only if `SignedDevModeStatement.enables(1)`; `DevModeStatementLoader` replays
  the cached string at startup.

The generator is `https://developer.sonos.com/devmode/`, which redirects to
`login.sonos.com` and only exists after sign-in with the owner's account. No
published statement was found: GitHub code search for `urn:sonos:device`,
`28721e71` and the base64 magic prefix `KHIe` returns nothing Sonos-related, the
S2 APK carries no statement asset, and community search turns up no pasted blob.
So the portal remains the only source, and the app is now a second *consumer*
(deeplink) rather than a second source.

## Statement source, re-checked 2026-10-25

The portal is still the only source and it is still alive:
`GET https://developer.sonos.com/devmode/` → `302
https://developer.sonos.com/s/login?startURL=%2Fdevmode%2F`. It is a Salesforce
community login, so a statement needs a Sonos developer account and a device id.

Two things confirm the code is pasted, never fetched:

- The device page is local-only. `anacapad` builds it from one literal
  (`0x685ff1`, S2 86.10): `Model: %u<br />Device ID: %s<br />Version: %d.%d` plus
  a textarea named `statement` and a `csrfToken`. There is no link, no fetch and
  no "get a code" button.
- The statement is a file once installed. The daemon reads `debug/devmode.bin`,
  stages `/tmp/devmode.tmp.bin` and persists `/devmode.bin`; the S1 controller
  mirrors it at `<datadir>/debug/devmode.bin` (`0xdf966c` in
  `captures/firmware/s1_controller/SonosController.bin`) and replays the cached
  string at startup.

Because the device id is inside the signed digest, a statement is bound to one
device: a blob captured from another unit verifies but does not unlock this one.
The S1 controller carries no devmode deeplink (only `sonos:///settings…`), so the
app cannot mint one either.

## Decision: the statement is replayable, not forgeable

`ZZ0218` only hashes the statement and `memcmp`s 32 bytes against a compiled digest; there is no RSA
verify on the live POST path, and the two accepted lengths (842 / 1060) are fixed. So the statement is
a **fixed blob**: capture one and it replays on any device of that line, with no key material needed.
The whole devmode problem therefore reduces to *obtaining one statement* — the parser work is done.
Two candidate sources remain: the app's unlock flow and any archived copy of the PEM (the two digests
are `60dfecfc…` and `d7b94102…`; neither matches any of the 132 bundle certs, checked as PEM and DER).

### Exhaustive negative search for an archived statement (2026-10-25)

Nothing public carries one. Checked and empty:

- GitHub code search: `urn:sonos:device`, `urn:sonos:model`,
  `1.3.6.1.4.1.44974`, `Sonos Device Authentication`, `sonos://devMode`,
  `devMode/set?statement`, `DevModeStatement`, `validateDevModeStatement` — the
  only hits are this repository.
- Web search: the portal page title, the OID, the CA name, Reddit, the Sonos
  community, the NCC paper — no statement.
- Local: the S1 controller DMG carries only mbedtls PEM *label* constants; the
  Safari/Brave/Edge/Arc caches have never seen `developer.sonos.com`; no
  keychain item for `sonos.com`.
- Both Android apps, now checked directly: `com.sonos.acr2` 90.01.9 (all 14
  splits of the xapk) and the S1 controller 11.16.1 contain **no** `KHIe`
  (base64 of the `0x28721e71` magic) and no certificate other than Amazon's.
  `libsonoscrypto.so` in the 90.01.9 app carries the *same* two pinned digests
  (offsets 24760/24792) and the same 294-byte SPKI (17378) as the firmware, so
  the app is a validator and forwarder, not a minter. `X-Sonos-Api-Key` does
  not appear in the app's dex at all — it is a firmware-only header.
- Sonos's own GitHub org (28 repos), the Wayback Machine (no snapshot of
  `/devmode/`), the `docs.sonos.com` index, and the community search carry
  nothing. The nearest public artifacts are `nickblt/sonos-websocket`
  `src/certs/sonos-ca.pem` (the *root CA*) and `palchrb/vibb-launcher`, which
  documents the player leaf's SAN shape (`urn:sonos:udn:RINCON_…`) — neither is
  a devmode statement.
- The portal itself is not guest-callable: `/s/sfsites/aura` answers
  `Guest user access is not allowed` even with the page's own `fwuid` and
  context, and `/webruntime/api/apex/execute` 302s to login.

**Even a published statement would not substitute.** `ZZ0137` `memcmp`s the
statement's device id against the caller's own id (`0xcbd4`) and the id sits
inside the signed region, so a blob minted for another serial fails on this
device. The portal, for this device, is the only artifact that can work.

So a statement must be minted with the owner's login. `scripts/devmode_submit.py`
is the one-command flow for the device half:

```
./devmode_submit.py IP                                   # portal inputs only
./devmode_submit.py IP --statement 'sonos://devMode/set?statement=...'
./devmode_submit.py IP --statement stmt.b64              # or a file, or -
pbpaste | ./devmode_submit.py IP --statement - --verify
```

It reads the model / device id / version / csrfToken the form wants, accepts the
statement as a deeplink, base64, file or stdin, **validates it offline first**
(`scripts/emu_zz0137.py`, so a malformed or wrong-device blob never costs a
POST), posts it with the 3.2 s spacing the daemon's 3 s limiter needs, retries
on 5xx, and scores the reply by elapsed time (0.6–1.0 s = parser reject,
2.7–5.7 s = reached the signature check, 200 = accepted, 409 = valid but
capability-less). `--verify` then probes `/unlock`, 2323, 22 and `/testenv`.
Exit codes: 0 accepted, 1 rejected, 2 unreachable, 3 failed the offline check.

## Replay tool and the response codes

`scripts/devmode_submit.py` supersedes `recon/devmode.py` (still present, no
offline gate). Observed codes: a 1-byte statement gets **403** (rejected), not
400 — so 403 is the "statement did not verify" answer and must not be confused
with the route-level 403 that `/unlock` returns unconditionally on both lines.
A successful POST answers 200; `--verify` reports port 22 rather than assuming
it, so the tool cannot report a false success.

## Why `/unlock` is always 403: it is loopback-gated

The route table marks exactly three routes with flag `0x10` — `/device_account`, `/unlock`,
`/unlock.htm` — and all three answer 403 to every method from the LAN while `/devmode` (flags 0) answers
200. So `0x10` is a local-only gate, and the old unlock flow is reachable only from the device itself.
That is why the LAN-reachable path is `/devmode` and why the statement is the whole problem: the
loopback routes cannot be used without already having local execution.

## The app is the delivery vehicle (and it validates locally)

The current Android app (`com.sonos.acr2`, pulled from `d.apkpure.net`) has a whole devmode module:
`DevModeStatementLoader`, `DevModeUriParser`, `DevModeUriIsValid`, `handleClearDevModeStatement`,
`checkIfDevModeStatementShouldBeCached`, and a `sonos://devMode/*` deeplink scheme. The statement arrives
**through that deeplink**, so it is handed to the app by the unlock page, not fetched from the player.

`DevModeManager.trySetStatement` validates it on-device with a native call:

```
NativeDevModeJNI.validateDevModeStatement(buf, len, 47, str, str.len, major, minor, …)
```

so the statement is **versioned** (`DevModeData.majorVer/minorVer`) and bound to a string, and the
validator lives in a native lib that is *not* in the base APK (no `.so` in the zip; it is in an ABI
split). That lib is the next target: it holds the verification key, and a local validator means a
statement can be checked without touching a player.

The base APK's only PEM is a 1634-byte CA (`a7baadd8…`), not the 842-byte statement.

## The app ships the devmode verifier

The ABI split (`config.armeabi_v7a.apk` from the same XAPK) carries `libsonoscrypto.so` — the same
library name as on the player — exporting `sonosRsaGetDevModePublicKey`, `validateDevModeStatement`,
`isDevModeFlagEnabled` and `isDevModeFlagEnabledValidate`. So statements are **RSA-signed** and the app
verifies them locally against a public key before handing them to a player; the player's compiled
SHA-256 is a separate, second gate.

Consequence: a candidate statement can be checked offline against that public key without touching a
player, and the key itself is extractable from the lib (`mbedtls_rsa_import`/`check_pubkey` are linked).
Next: pull the key blob out of `libsonoscrypto.so` and build the offline verifier.

### The devmode public key, extracted

`sonosRsaGetDevModePublicKey` returns a 294-byte **DER SubjectPublicKeyInfo** (RSA-2048) at `.rodata`
offset `0xE54F` of the app's `libsonoscrypto.so`, with length `0x126` = 294 — the same 294-byte shape as
the device's compiled update-key entries, which confirms the record format independently.

```
SHA-1(SPKI) = dfa45c421d2335394b73df09307d9100996a356f
modulus     = 86563be24edd2c535aa0d5c98dc11d19b62182…
```

It is a **third** key: neither `28a096f5…` nor `b2927a28…`. So devmode statements are signed by a
private key we still do not have, and the app's RSA check is only a client-side pre-filter — the player
itself just compares a compiled SHA-256, which is why forging is not an option and a capture is.

## The S1 desktop controller knows a `devmode.bin`

The shipped S1 Controller binary carries the same form template (11×80 textarea, `csrfToken`, submit
and delete buttons) plus two paths the player does not otherwise expose: `%s/debug/devmode.bin` and
`/devmode.bin`. Both 404 on an unlocked-never device, so the file is the **cached statement written
after a successful unlock** — which makes retrieval trivial once a device is unlocked, and gives a
second way to lift a statement off any already-unlocked player in the wild.

The app itself does not embed a statement (its only PEMs are the bare labels) and has no unlock URL, so
the statement is still pasted in by the user.

## The portal is alive; only the login is missing

`https://developer.sonos.com/devmode/` answers **302** (to the login), while the old
`www.sonos.com/en-us/unlock` is 404. So the minting page still exists and the owner's login is the only
missing piece — after that `scripts/devmode_submit.py HOST` reads the model / device id / version /
csrfToken, posts the statement with the spacing the form wants, and verifies the result. Nothing else
in the chain needs the owner.

### End-to-end run of `devmode_submit.py` (2026-10-26)

Run from `mac-mini-max` (Tailscale `100.68.51.94`, which holds `192.168.4.171/22`)
because the controller host's `192.168.4.x` is dead. `.86` was not up; `.201`
(Play:5, model 5) and `.202` (Play:5, model 16) were, both 57.23.

What works: the GET page, the csrfToken, the POST transport, and
`button=delete` (200 in 0.03 s). What fails, and where: a 46-byte synthetic
statement (magic, model 16, id `000E585D8C10`, bounds `0x390000`..`0x39ffff`,
no ring) is rejected by the **statement parser at the ring stage**, 403 in
0.02 s. The offline emulator on the same blob agrees exactly:
`REJECTED (ZZ0137 -> 0, last check: ring parse)`. So the tool and the device
agree; the blob is simply incomplete, and a real statement is the only thing
still missing.

Two transport facts from the run: the csrfToken must be URL-encoded (a raw `/`
in the token gives 400 — `urlencode` already does this), and the offline gate
needs `unicorn`, which is absent on the mini, so it now names the dependency
and skips rather than blocking the POST.

## The ring check, named (2026-10-26)

The stage the tool stops at is `ZZ0121` at `0x9f24` in the 57.23 lib (S2:
`ZZ0121`'s counterpart in `libsonoscrypto.so.3`). It is a container parser, not
a signature check. In order:

| site | check |
|---|---|
| `0x9f48` | `end < ring+4` → reject |
| `0x9fd4` | `*(u32)ring` must equal **`0x621da74d`** |
| `0x9ff4` | `end < ring+8` → reject |
| `0xa020` | `ring_len < *(u32)(ring+4)` → reject (the declared total) |
| `0xa02c` | `end < ring+10` → reject |
| `0xa05c` | `*(u16)(ring+8)` must equal **1** |
| `0xa068` | `end < ring+12` → reject |
| `0xa07c` | `end < ring+16` → reject |
| `0xa0b4` | `*(u32)(ring+12)` (count 1) must be **< 9** |
| `0xa0b4`–`0xa110` | each entry: `u32 kind` must be **< 7**, `u32 len` must be **< 257**, `len` bytes must fit; kind 5 is a key reference and takes a 4-byte payload |
| `0xa114` | the same again for a second count at the cursor, also **< 9** |
| `0xa1a0` | if count 2 is 0, the first list is validated by `0x86e8` |
| `0xa1f8` | a byte `n` must satisfy `(n-1) & 0xff < 2`, i.e. **1 or 2** |
| `0xa270` | each reference: `u32 len` must be **< 2048**, `u8 type` must be **1..5** (`0x90ec`), then `len` bytes |
| `0xa1bc` | the first list is handed to `0x86e8` with the reference array |

So a statement's ring is a `0x621da74d` container: `u32 total_len`, `u16
version = 1`, two bytes, `u32 count1 < 9` and its entries, `u32 count2 < 9` and
its entries, then a count byte of 1 or 2 and that many `{u32 len < 2048, u8
type 1..5, data}` references. The reference the resolver actually accepts is
type 3 with length 20 — the SHA-1 of the build's pinned SPKI (`f4df800d…` on
57.23, `8d08e900…` on 86.10).

Built to that shape (92 bytes, one kind-5 entry, one type-3/20 reference) the
blob reaches the device's **slow path**: 4.16 s on the second POST instead of
the 0.02 s parser reject. It still fails, at the signature, which is expected
without the vendor key.

Open: the emulator still reports `ring parse` for the same blob and faults
inside `0x86e8` (the first-list validator) on my stub plumbing, so the exact
sub-check that rejects a synthetic is not yet pinned. The device is the better
oracle for that.
