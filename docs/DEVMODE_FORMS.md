# The two devmode-looking checks: pinned certificate vs statement TLV

Two different mechanisms in 86.10 `anacapad` hash a blob and compare a compiled
digest. Only one is the `/devmode` POST path.

## ZZ0218 — a pinned-certificate shortcut in the cert validator

- Caller `0x4edf34` runs only when `(*cert & 5) == 1` (a certificate type tag),
  then `ZZ0130(cert, &pem, &len)` (extract the leaf PEM) and
  `ZZ0218(pem, len)`.
- `ZZ0218` requires `len == 842` or `len == 1060` (`0xb798`, `0xbac8`), runs
  `ZZ0058(1, pem, len, digest)` — plain SHA-256 — and `memcmp`s the digest
  against two compiled 32-byte values (`0xbb18`/`0xbb28` for 842,
  `0xbb4c`/`0xbb5c` for 1060). Return is `cntlzw(x) >> 5`, i.e. 1 on a match.
- The compiled table is at `0xd8b8` and is PEM text: it starts
  `-----END CERTIFICATE-----\n\0\0-----BEGIN CERTIFICATE-----\n`.
- Its caller chain is the certificate-validation statistics object
  (`certvalStats`, log category `certval`, "Expected cert validation failure"),
  not the HTTP handler.

## The two pinned digests, extracted

Emulating `ZZ0218` (Unicorn PPC, relocations applied, SHA-256 stubs, stop at the
first `memcmp` `0xbb28`) yields the two compiled values:

```
842  -> 60dfecfc0bbb153654322dc33fc56bd5d57a617d2a4347e6785ab016c09b6d62
1060 -> bac0d0523cfa7bf82bddf1a5cbec40e60c303fe4d8d2595c101e4112269b5153
```

The 842/1060 lengths are PEM sizes of ~630/795-byte DER certificates, i.e. EC
leaf/intermediate certificates, not the 4096-bit RSA Sonos roots. No preimage is
shipped:

- the PEM template at `0xd8b8` (842/1060/1200 windows from `0xd8b8` and from the
  `BEGIN` marker `0xd8d4`) — no match;
- all 89 DER certificates in `fw/internals/fallback.rcb`, including the two
  roots the template names (`Sonos Device Authentication Root CA` at `0x4fce`,
  `Sonos Test Device Authentication Root CA` at `0x5625`) — no match;
- all 493 DER certificates in the type-22 bundle of `86.10-80260-1-12.upd`
  (record at `0x16c`, 149527 bytes) — no match;
- the S2 APK: no `BEGIN CERTIFICATE`, no `urn:sonos:`, no base64 statement
  (`KHIec`/`cR5yK`).

Each was tried as DER, as bare base64, and as PEM with 64/76-column wrapping,
LF/CRLF, with and without a trailing newline or NUL. So the pinned certificates
are issued at runtime and are not recoverable from the image; the digests are
only useful to *check* a candidate PEM offline.

## ZZ0137 — the live `/devmode` POST

Route entry: the `/devmode` string is at `0x6656b4`; the route table record at
`0x6e7034` holds `{name = 0x6656b4, handler = 0x4aa959}`. The handler is a
two-instruction thunk (`save; jal 0x4f4240; restore` at `0x4aa958`), so the real
handler is `0x4f4240`:

- `0x4f4240`: method 1 -> GET `0x4f3fe0`; method 5 -> require
  `Content-Type: application/x-www-form-urlencoded` (string `0x675dd4`,
  `strcasecmp` at `0x4f4268`) else 400, then POST `0x4f419c`; other methods 405;
  null devmode object 500.
- `0x4f419c`: rate-limits to one POST per 3 s (`sonosClockGetTime` at
  `0x4f41a6`, `sleep(3 - delta)` at `0x4f41c4`), reads at most 2048 bytes with a
  2000 ms timeout (`0x4f5990`), then calls the form parser `0x4f403c`.
- `0x4f403c`: `RparseNextQueryStringParam` over the body. Recognised keys:
  `statement` (base64, decoded into a 1024-byte buffer by `0x4ede64`),
  `button=delete`, `csrfToken` (compared with `0x4eec54` against the object's
  token). **A POST with no `csrfToken` is 400** (`local_1c == 0` ->
  `LAB_004f4160`), so a 403 from the device proves the token was accepted and
  the statement itself was rejected.
- `0x4f3b44` is the validator: `ZZ0137` at `0x4f3b96` with the decoded
  statement,
  the model, the device id and the version bytes; the notify path calls it at
  `0x4f3aa2`.
- `ZZ0137` requires the TLV magic `0x28721e71`, the model, a version window, the
  device id (`memcmp`), a payload and a ring; the signature is checked against
  the one compiled SPKI (`ZZ0080`).
- Return mapping in `0x4f403c`: `ZZ0137` 0 -> **403**, 1 -> **409** (already
  installed; the log line is `R_ALLOW_SSH_PUBKEY_INSTALL may n...`), 2 -> 200
  (`Statement installed`).
- There is **no** `ZZ0130` or `ZZ0218` call anywhere in the whole route chain
  `0x4f4240 -> 0x4f419c -> 0x4f403c -> 0x4f3b44`.

So the live POST uses the device-bound TLV. The PEM+digest check is a
certificate pin, and a PEM posted as a `statement` fails at the TLV magic.

## Where a statement comes from

The developer portal (`https://developer.sonos.com/devmode/`, per-serial, behind
the Sonos login) is still the only source: the statement is signed by the
compiled devmode key and embeds the unit's id. The PEM check does not mint
statements and cannot be replayed as one. The portal procedure is in
`docs/RE_DEVMODE.md`.

## The app-side flow (current Android app, `com.sonos.acr2`)

The app does not mint statements either; it receives one as a deeplink and
posts it. Decompiled from the APK:

- `DevModeDeeplinkHandler` accepts scheme `sonos` or `sonos-2`, host `devMode`,
  exactly one path segment: `set` or `clear`.
- `set` reads query parameter `statement`, replaces spaces with `+`, decodes it
  with Kotlin `Base64.Default` (`DevModeUtilsKt.devModeStatementStringToByteArray`)
  and calls `DevModeManager.trySetStatement(controllerId, bytes)`.
- `trySetStatement` runs the **same** native validator the device uses
  (`NativeDevModeJNI.validateDevModeStatement`, with the device's major/minor
  version) and only stores the statement if it validates; it is cached in a
  Room/SQLCipher `DevModeStatementRepository`.
- `clear` drops the cached statement.

So the delivery URI is:

```
sonos://devMode/set?statement=<base64 TLV>      (sonos-2:// also accepted)
sonos://devMode/clear
```

The portal page produces that URI (as a QR/deeplink) for a given serial; the app
is only a courier. A cached statement on a previously used phone would work, but
there is no app-side minting endpoint.

## 2026-10-25: the desktop app is a second courier, and no statement is published

The 57.23 macOS app has a devmode client too. It reads
`<support-dir>/debug/devmode.bin` (`snprintf("%s/debug/devmode.bin")` at
`0x10087dbe4`, `stat` + size compare at `0x10087dc04`/`0x10087dc18`, then
`/devmode.bin` under `%s/debug` at `0x10087dd7c`), and it renders the same
`Enter DevMode Code` form the device serves (format string at `0x10087e188`).
So the portal hands out a **file** as well as a deeplink; both are couriers, and
neither mints.

Guest access to the portal is closed, so the statement cannot be pulled without
the owner's session:

- `GET https://developer.sonos.com/devmode/` -> `302 /s/login/?startURL=%2Fdevmode%2F`.
- The site is Salesforce Experience Cloud; `aura://ApexActionController/ACTION$execute`
  answers `{"state":"NEW","returnValue":null}` + `No apex action available for
  <class>.<method>` for every `DevMode*` class/method guess, i.e. guest Apex is
  not exposed. `aura://ComponentController/ACTION$getPage` returns `actions: []`.
- `https://developer.sonos.com/s/sitemap.xml` lists only `/s/*` routes; `/devmode/`
  is a custom top-level route and is not in it.
- The Wayback Machine has no capture of the URL (`/web/*/developer.sonos.com/devmode*`
  returns an empty CDX set).

No published statement exists either: `gh` code search for `urn:sonos:device`,
`28721e71` and the base64 magic `KHIe` returns nothing Sonos, the `sonos` GitHub
org has no devmode repo, the NCC BlackHat 2024 whitepaper, the RomHack 2023 deck
and `blasty/sonos` only mention `/devmode` in passing, and the S2 APK carries no
statement asset.

Practical consequence: the only remaining step is an authenticated portal
session. `scripts/devmode_submit.py` already does the device half (reads the
model/id/version/csrfToken, posts with the 5.5 s spacing, scores by latency), so
one statement from the portal is a single command.
