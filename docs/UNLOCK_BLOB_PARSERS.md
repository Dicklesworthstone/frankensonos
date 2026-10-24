# S2 unlock-blob parsers: `/devunlock` and `/mfgunlock` (86.10)

Owner-directed RE on `.86` (Play:1, model 8) and `.81` for `/mfgunlock`, which
`.86` does not serve. Both routes take an attacker blob pre-auth, but **on these
builds neither ever gets past its own state gate** — this is a gate record, not
a verifier fuzz.

## Reachability

- Only on the secure listener. `GET /devunlock` on `:1400` is `403`;
  `GET /mfgunlock` is `404`. On `:1443` they answer.
- `GET /devunlock` → `423`, 293 bytes, `Serial: …`, form field `confirm`.
  `GET /mfgunlock` → the same shape on `.81` (`.86` serves `404`; `POST` to
  either is `405` when the method is wrong).

## Handler contract (from the code)

`/devunlock` → `0x4ccdac` → dispatcher `0x4ccee8` → `FUN_004ccac8` (POST) →
`FUN_004cc81c`. `FUN_004ccac8` first does `strncmp(body, "confirm=", 8)` and
returns `400` on a mismatch, so the body must **start** with that literal. The
value then goes through:

1. `'+'` is rewritten to NUL, so it terminates the value (URL-encoded space);
2. `sonosDecodePercentEncodedStr(value, out, 0x800)`;
3. every `isspace` character is stripped;
4. `RdeserializeData` — **hex**, not base64 — into a `0x400` buffer.

`FUN_004ccc30` is the shared GET: for `/devunlock` (`param_1 == 0`) it is meant
to mint a 16-byte challenge with `ZZ0071` + `RserializeData` and set the state
flag; the POST checks that flag (`0x7002f4`) and returns `400` if unset.
`/mfgunlock` (`param_1 == 2`) has its own verifier branch (`ZZ0161` + MDP).

## Why the gate never opens (read from the handler)

The state is one BSS cell at `0x7002f4`, reached through the GOT injection at
`0x4ccd98` (GET) and `0x4ccf88` (POST). **Only one code path sets it**: the
`GET` branch for `/devunlock` (`param_1 == 0`) in `FUN_004ccc30`, after

1. `ZZ0071(0x7002fc, 16)` — 16 bytes from `mbedtls_ctr_drbg_random`;
2. `strcpy(buf, "02:")` and `RserializeData(0x7002fc, 16, buf+3, 0x210000)`.

Any other route or flag clears it; the POST consumes and clears it again, and
`FUN_004cc81c` returns 400 when it is zero. `RdeviceIsUnlocked()` non-zero is a
separate 200 "Not Applicable" exit.

`ZZ0071` is the library's DRBG wrapper: the getter at `0xbdcc` aborts if the
seeded flag at `+0x31c` is unset, so the init did run — the DRBG object is
seeded by `libsonoscrypto.so.3`'s `init_array` constructor (`ZZ0146`, relocated
at `0x1fd60`). The failure is therefore at *call* time: the random call returns
non-zero and `ZZ0071` collapses it to 0, leaving the flag clear and the page on
the "Fuse Value" branch.

Nothing else arms it: a prior `/devmode` GET, repeated `/devunlock` GETs and
3 s quiet gaps all still print `Fuse Value`, and `.81` (`/mfgunlock`) behaves
the same. No route re-invokes `ZZ0146`.

**Contract for a future working unit** (`FUN_004cc81c`, before the flag check):
body must be `confirm=` followed by an **even-length hex** string; `'+'`
terminates it, `sonosDecodePercentEncodedStr` expands `%XX` into a 0x800 stack
buffer, all whitespace is stripped, and `RdeserializeData` decodes into 0x400
(max 2048 hex digits). `RserializeData` is the inverse and returns 0 when
`2*len+1` exceeds its cap. Non-hex (including real base64) and odd lengths fail
at `RdeserializeData` with 400 — indistinguishable from the flag check.

## Matrix (`.86`, 22 calls; `/info` checked after each)

Raw body bytes including the 8-byte `confirm=` prefix.

| case | GET | POST | notes |
|---|---|---|---|
| body 0 | 403 | 400 | state gate |
| body 1 | 423 | 400 | |
| body 4 | 423 | 400 | |
| body 1,032 (0x400 hex data) | 403 | 400 | |
| body 2,056 (0x800 hex data) | 423 | **reset** | over the body cap |
| body 2,057 | 423 | 413 | |
| body 65,544 | 423 | **reset** | |
| `confirm=!!!!` (invalid base64) | 423 | 400 | |
| `confirm=SGVsbG8=` (real base64) | 423 | 400 | base64 is not the format |
| percent-escaped `deadbeef` | 403 | 400 | |
| embedded `%00` | 423 | 400 | |
| `confirm=` at start | 423 | 400 | |
| `confirm=` in the middle | 423 | 400 | prefix `strncmp` fails |
| `confirm=` at the end, `&x=1` after | 403 | 400 | trailing bytes are not hex |
| duplicate `confirm=` | 423 | 400 | only the raw body is scanned |
| devmode magic + bad ring | 423 | 400 | never reaches a ring parser |
| `/mfgunlock` GET | **404** | 405 | route absent on `.86` now |
| `/mfgunlock` body 0 | 404 | 405 | |
| `/mfgunlock` body 1 / 4K / 64K / bad | 404 | reset | reader still runs before the 404 |

The exact body cap is `0x800` raw bytes: at/over it the reader closes while
the client is still writing (reset); the 413 appears when the response wins the
race. `/info` answered `200` after every call; nothing crashed or wedged.

`/mfgunlock` is unstable on `.86`: it answered `405` to POST in one probe and
`404` to both methods in this run. `.81` serves it (423 with the same page), and
its 4 KB/64 KB bodies answer `413`.

**Base64 is not the wire format.** `SGVsbG8=` fails at the hex deserializer
(`H` and `=`). Only hex characters reach the 0x400 buffer.

## What is still needed

A unit whose `ZZ0071` challenge path works (or a build where the flag is
seeded another way) to score the hex/percent/strip parser and the verifier.
Until then, the fields above are only bounded, not exercised past the gate.
Harness: `scripts/unlock_blob_fuzz.py`.
