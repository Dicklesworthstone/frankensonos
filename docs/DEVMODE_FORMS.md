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
- `ZZ0137` returns the signature-verify result directly (S1 `0xad78`-`0xad84`
  loads the compiled SPKI and jumps to the verify, then returns its value), so a
  403 is a parse **or** binding/signature failure, never a transport problem.
- The 409 branch is effectively dead in this build: it needs
  `ZZ0018(payload, len, 0x2f0000)` (`0x4f3824` -> bit test, bit 0 = LSB of the
  last payload byte) and `0x2f0000` is bit 3,076,096, while the payload is at
  most 1024 bytes, so the first test is always 0 and the `&&` short-circuits.
  Treat 200 as the only success.
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
session. `scripts/devmode_submit.py` is the one-command flow: it reads the
model/id/version/csrfToken, takes the statement as a deeplink/base64/file/stdin,
validates it offline with `scripts/emu_zz0137.py` before spending a POST, posts
with the 3.2 s spacing, and probes the unlock surface on success.

## Validator location and the version window

`ZZ0137` is an **undefined import** in `anacapad` (`Elf32_Sym` index 832, value
0, `shndx` 0; PLT slot `0x6db5d0`, GOT `0x6f6164`), resolved from
`libsonoscrypto.so.*`. The S2 copy of that library is not in this repo — only
the S1 `.so.1` is — so the S2 validator body has to be read on the device.

The S1 body (`ZZ0137` at `0xab54` in `57.23-74170-1-12/libsonoscrypto.so.1`)
shows the window check exactly: the lower bound at `+0x18` must be `< 0x390018`
(`0xac0c`) and the upper bound at `+0x1c` must be `>= 0x390017` (`0xac50`), i.e.
the statement must cover `57.23` exactly. There is no PEM branch: the first
compare is the magic (`lwl`/`lwr` of `0x28721e71` at `0xabac`/`0xabb8`).

## Offline triage of a candidate statement

`scripts/emu_zz0137.py` runs the real `ZZ0137` (S1 lib, MIPS32) under Unicorn
with the `.MIPS.stubs` trampolines patched to handlers (`memcmp`, `memset`,
`memcpy`, `mbedtls_sha256_*`) and reports the first failing check by name:

```
scripts/emu_zz0137.py statement.bin <device-id-hex> <model> <major> <minor>
```

It prints `ACCEPTED`/`REJECTED` plus the last check reached (magic, model,
version bounds, device id, payload, ring, signature verify). The signature verify
itself is real code but cannot pass without the private key, so a statement that
reaches it is structurally sound and fails only on the signature.

The ring is a nested structure parsed by `ZZ0121` (`0x9f24`): u32 magic
`0x621da74d`, u32 entry count (< 9), u16 version (must be 1), u8, u8, u32
(< 9), then a second count block and a u8 that must be 1 or 2, then 264-byte
entries. The entry body is not fully mapped yet; a real statement from the
portal carries it, so the triager is usable without that mapping.

## 2026-10-26: LAN survey — no player is unlocked; portal inputs for each

`/debug/devmode.bin` is the post-unlock cache (the S2 writer builds
`/tmp/devmode.tmp.bin` and renames it to `debug/devmode.bin`, literals at
`0x285fca`), so a 404 is a reliable "locked" probe. Six players answer on
`192.168.4.0/24`; **all six 404** on `:1400` and the two S2 units also 404 on
`:1443`:

| addr | model | device id | version | `/debug/devmode.bin` |
|---|---|---|---|---|
| .26 | Bridge | `RINCON_000E584F3B8801400` | — | 404 (no `/devmode`) |
| .81 | 8 | `000E58CADE94` | 86.10 | 404 |
| .84 | 16 | `000E5880108A` | 57.23 | 404 |
| .86 | 20 | `7828CA9221DC` | 86.10 | 404 |
| .201 | 5 | `000E5816C16A` | 57.23 | 404 |
| .202 | 16 | `000E585D8C10` | 57.23 | 404 |

`/testenv` and `/ssh/fingerprints` are `403` on every player that serves them,
matching the lock state.

The portal form wants exactly the model and device id above, so any one row is a
complete submission. No public statement endpoint exists either: `/devmode/`,
`/devmode/<id>/devmode.bin`, `/devmode/devmode.bin` and
`/devmode/?deviceId=…` all return the 269 KB login page.

No published statement was found in any searched source: `gh` code search for
`urn:sonos:device`, `urn:sonos:device:`, `28721e71` and `KHIe`; `gh` issue
search; the `sonos` org repos; `darkarnium/sonor` and `trulyspinach/sonos-fenway`
(the custom rootfs has no `devmode` string); the NCC BlackHat 2024 whitepaper,
the RomHack 2023 deck and `blasty/sonos`; the Sonos community search (3 hits,
none about devmode); Reddit; and the Wayback CDX set for the URL.

## What the statement binds to (settles "is it replayable?")

The statement is bound to the **player's own Device ID**, not to the app/controller:

- `GET /devmode` (`0x4f3f44`) renders `Device ID: %s` from `0x4f3d60`, which copies
  6 bytes from the identity singleton (`0x4fa320` -> `0x595ccc`) and formats them
  into 20 characters (`0x4fa220`, length `0x14`) — the MAC-derived device id.
- The POST path passes the same object field: `ZZ0137(statement, len, model,
  obj+0x414, *(obj+0x494), major, minor, ...)` at `0x4f3b96`, and `ZZ0137`
  `memcmp`s it against `statement+33` at `0xacb0`.

So a statement is replayable on the *same* player (re-POSTing is idempotent) but
not portable: another player's statement fails the id compare. An
already-unlocked player is only useful if it is one of ours.

## Published-statement search (2026-10-26)

Negative across every public corpus reachable:

- GitHub code search: `"sonos://devMode"` and `"devMode/set?statement"` return
  only this repo; `filename:devmode.bin` returns 0; `KHIec` is too generic
  (7056 hits, none Sonos).
- Wayback: no capture of `developer.sonos.com/devmode*` and none of the old
  `SoCo/SoCo/wiki/Developer-Mode`.
- Sonos community search (`en.community.sonos.com/search?q=devmode`) returns 3
  unrelated threads.
- No `devmode*.bin` anywhere under `~/Library/Application Support`, `~/.local/share`
  or `~/.config`, and no `sonos.com` cookie in the Chrome profile.

The LAN is also empty this session: a full `192.168.4.0/24` ping sweep answers
only the Mac itself, so `.86`, `.81` and `.202` are powered off and no
`/debug/devmode.bin` can be fetched.

## Live end-to-end run against the units (2026-10-26)

All three units answered. The flow works up to the statement content; the exact
failure point is the ring body, not the transport, the form or the id.

```
$ scripts/devmode_submit.py 192.168.4.86
model     20
device id 7828CA9221DC
version   86.10
csrf      NSDPWQEIFdB5tQ9ZZBWOSeU0WD183xof
```

- `GET /debug/devmode.bin` -> **404** on all three (`.86`, `.81`, `.202`), so
  none is unlocked; `GET /unlock` -> **403** on all three.
- A 579-byte hand-built TLV with the real model, id and version passes the
  offline gate's magic/model/version/id/payload checks and stops at
  `ring parse` on every unit.
- Forcing the POST (`--no-validate`) gets `HTTP 403` in 0.04 s with the stock
  `Error 403` page — a parser reject, matching the offline verdict.

The offline validator is the S1 library, whose version window is hardcoded to
57.23 (`lower < 0x390018`, `upper >= 0x390017`). `emu_zz0137.py` now takes a
`WINDOW_LO,WINDOW_HI` argument and `devmode_submit.py` passes the device's own
version, so the gate is correct on 86.10 too.

## 2026-10-26 (2): end-to-end run of `devmode_submit.py` against .86

Ran the whole flow against `192.168.4.86` (model 20, `7828CA9221DC`, 86.10).
Stages and exact failure points:

1. `devmode_submit.py 192.168.4.86` — page fetch, parse and portal block all
   work (`model 20`, `device id 7828CA9221DC`, `version 86.10`, fresh csrf),
   exit 0. This is the point a real run stops at today: no statement exists.
2. `--statement <fake>` — the offline validator stops it before any POST:
   `REJECTED (ZZ0137 -> 0, last check: model)`, exit 3. Correct, the validator
   is model-8-specific and the blob was synthetic.
3. `--statement <fake> --no-validate` — reaches the device: `HTTP 403 in 0.04s`,
   classified `rejected before the signature check: malformed blob`, exit 1.

A hand-built TLV that satisfies every preamble check the parser makes (magic
`0x28721e71`, model, version window, devid length + `memcmp`, payload length,
ring magic `0x621da74d`) still fast-rejects, so the remaining gap is the ring's
inner layout, not the transport or the preamble. The preamble checks were read
from the S1 parser (`0xabac` magic, `0xabe8` model, `0xac14`/`0xac58` version
bounds — lower is *strict*, `[+0x18] < 0x390018`; `0xac84` devid length;
`0xacb0` `memcmp`; `0xacd4` payload bound; `0x9fc8` ring magic).

## 2026-10-26 (3): the ring check, and what a valid ring is

The stop is `ZZ0121` (`0x9f24` in the S1 lib), the ring parser. Its layout, read
end to end:

```
ring:
  +0x00 u32 BE 0x621DA74D
  +0x04 u32 BE count        ; must be <= remaining, and >= what the parse consumes
  +0x08 u16 BE 1            ; version, exact
  +0x0a u8, +0x0b u8        ; copied verbatim
  +0x0c u32 BE nkeys        ; < 9
  +0x10 nkeys x 0x108 key record
        u32 BE nkeys2       ; < 9, may be 0
        nkeys2 x 0x108 key record
        u8 nsig             ; 1 or 2
        nsig x { u32 BE len (< 0x800), u8 type, len bytes }
```

Every `0x108` record repeats the same triplet — magic `0x621DA74D` (`0x922c`),
`count` <= remaining (`0x9244`), `version == 1` (`0x9254`) — then a `u16` at
`+0xa`, a `u32 n < 9` at `+0xc` and `n` nested records at `+0x10`.

The check that a hand-built ring fails is the **final one at `0xa3c4`**: the
declared `count` at `ring+4` must be `>= cursor - ring_start`, i.e. it must cover
every byte the parse consumed. A ring with `count = 0` parses the header and then
dies there. `nkeys2 == 0` is fine (`0xa3bc` branches straight to the verify).

A valid ring therefore needs `count = len(ring)` and a signature block; the
emulator's `build()` in `scripts/emu_devmode_stmt_s1.py` emits exactly that and
its `valid` case reaches the verifier (`verify=True`).

Caveat found while testing: the latency classifier is **not** conclusive on S1.
The emulator-valid blob with a `0xAA` filler signature still answers `403` in
`0.03 s`, so "fast = malformed" only holds on S2. The script now says so.
