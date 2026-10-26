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

## ZZ0137 — the live `/devmode` POST

- Handler `0x4f3b44` calls `ZZ0137` at `0x4f3b96` with the decoded statement,
  the model, the device id and the version bytes; the notify path calls it at
  `0x4f3aa2`.
- `ZZ0137` requires the TLV magic `0x28721e71`, the model, a version window, the
  device id (`memcmp`), a payload and a ring; the signature is checked against
  the one compiled SPKI (`ZZ0080`).
- There is **no** `ZZ0130` or `ZZ0218` call anywhere in `0x4f3b44..0x4f3cb0`.

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
