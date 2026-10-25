# The mtmhhsetup API-key check is a two-entry SHA-256 allowlist

Owner-directed RE on 86.10 `anacapad`. The check is at `0x4aa99c`, reached only
from the `/mtmhhsetup` handler (`0x4ac11c`). It is **not** a presence check.

## What it does

```
memset(out, 0, 32)
if (!value) fail
len = strlen(value)
ZZ0058(1, value, len, out)          // 0x4aa9b8
if (memcmp(out, 0x676ea8, 32) && memcmp(out, 0x676e88, 32)) fail
```

`ZZ0058` is a plain hash: `ZZ0062(ctx, algo)` zeroes a 112-byte context and
stores the algorithm, `ZZ0064` dispatches to `mbedtls_sha256_update` for algo 1,
`ZZ0061` to `mbedtls_sha256_finish`. Emulating it under Unicorn PPC
(`scripts/emu_zz0058.py`) reproduces `hashlib.sha256` byte for byte for two
inputs, so the two 32-byte constants are SHA-256 digests of the raw header
value. There is no salt, no HMAC, no prefix.

## What does *not* match

- the published local-API UUID `123e4567-e89b-12d3-a456-426655440000`
  (SHA-256 `203fa897…`), in any case/dash/brace/base64/binary form, with or
  without the header name;
- the second public GUID `622493a2-4877-496c-9bba-abcb502908a5`, likewise;
- every string of 8–128 bytes in the current Android app (`com.sonos.acr2`,
  423,753 unique strings), every string in the S1 controller 57.23 DMG
  (49,114), and every UUID in either.

So the accepted keys are **not shipped as literals** in either client. They are
two fixed app-side keys built at runtime; the digests in the firmware are the
only device-side oracle. "Any non-empty `X-Sonos-Api-Key` passes" is false for
this handler.

The runtime construction is now known (2026-10-25): the S2 app XORs a 36-byte
blob in `libsecrets.so` with the SHA-256 hex of the package name
(`com.sonos.secrets`); `scripts/s2_secrets_derive.py` recovers every secret, and
the resulting prod keys still do not match the digests. See the 2026-10-25
section of [`RE_DEVMODE.md`](RE_DEVMODE.md).

## Statement path: pinned key, not a chain

`ZZ0137` parses the ring and calls the verifier with the resolver at `0xaa80`.
That resolver accepts **only ref type 3 with length 20** (`a1 == 3`,
`a3 == 20`), calls `ZZ0080` — which ignores the key store and parses the single
embedded SPKI at `0xc130` — DER-writes it (`0x63bc`), and `memcmp`s the 20-byte
SHA-1 against the ref (`0xab00`). Inline key material is never used for the
verify; the x5c chain is irrelevant. The statement is gated by one compiled RSA
key, and only its SHA-1 fingerprint is carried in the ring.

## Independent re-verification (2026-10-25, lane DustyBison)

Both halves were re-read from the shipped binaries and hold.

- The digests are at `0x676e88` and `0x676ea8` (S2) and `0x50c0e4` / `0x50c104`
  (S1 57.23-74170-1-12); the S1 check is the same two-entry allowlist at
  `0x4288e0` (caller `0x42a244`), so the gate is not S2-only.
- Neither digest matches any of 23,757 distinct printable strings in the S2
  `anacapad`, the S1 `anacapad`, or `libsonoscrypto.so.3`, nor any of the 21
  UUID-shaped strings in the S1 controller, in any case/dash/brace form.
- The resolver is `0xaa80` in `libsonoscrypto.so.3`; it accepts ref types 1–4
  with digest lengths 32/16/20/28 (`0xa4a4`–`0xa4d4`), requires the ref length
  to equal the type's (`0xa4dc`), then indexes a key store at stride `0x910`
  and compares the type byte at `+0x18a4` (`0xa4e4`). Type 3 = 20 = SHA-1.
- `ZZ0080` (`0xcd58`) ignores the store: it tail-calls `ZZ0101(ptr, 0x126)`
  with the GOT slot at `0x1fee8`, which relocates to `0xdc9c` — one 294-byte
  RSA-2048 SPKI. Its SHA-1 is `8d08e900eb6578d78b1c69b551e0a50a0225d871`.
- The key is **per build**, not fleet-wide: the same 294-byte SPKI sits at
  `0xbE90` in the 34.16 lib (SHA-1 `a746c17976018c2f3a4150fdc1249b342435f596`)
  and at `0xd8b8` in the 57.23 lib (SHA-1 `f4df800df8860b4ec8bd4b944055fbe7796cdcd9`).
  A statement is therefore bound to device **and** build.
- `ZZ0218` (`0xb78c`) is a second, *unreferenced* verifier in 86.10: it accepts
  only lengths `0x424` and `0x34a`, SHA-256s the whole statement and `memcmp`s
  it against two pinned digests (`0x60dfecfc…` / `0xbac0d052…`, stored
  byte-permuted at `0xd878`). No pointer to `0x4edf32` (its only caller) exists
  in the image, so it is dead code in this build — a pure replay gate if a later
  build wires it up.

## Reproduce

```
python3 scripts/emu_zz0058.py        # prints emulated vs hashlib for two inputs
```

Live scoring of a candidate key needs an unlocked unit (`/mtmhhsetup` is behind
the devmode flag); a wrong key is `500` with an empty message, a right one
reaches the handler's 400/413 field checks.
