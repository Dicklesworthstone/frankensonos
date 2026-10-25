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
two fixed app-side keys (likely fetched or built at runtime); the digests in the
firmware are the only device-side oracle. "Any non-empty `X-Sonos-Api-Key`
passes" is false for this handler.

## Statement path: pinned key, not a chain

`ZZ0137` parses the ring and calls the verifier with the resolver at `0xaa80`.
That resolver accepts **only ref type 3 with length 20** (`a1 == 3`,
`a3 == 20`), calls `ZZ0080` — which ignores the key store and parses the single
embedded SPKI at `0xc130` — DER-writes it (`0x63bc`), and `memcmp`s the 20-byte
SHA-1 against the ref (`0xab00`). Inline key material is never used for the
verify; the x5c chain is irrelevant. The statement is gated by one compiled RSA
key, and only its SHA-1 fingerprint is carried in the ring.

## Reproduce

```
python3 scripts/emu_zz0058.py        # prints emulated vs hashlib for two inputs
```

Live scoring of a candidate key needs an unlocked unit (`/mtmhhsetup` is behind
the devmode flag); a wrong key is `500` with an empty message, a right one
reaches the handler's 400/413 field checks.
