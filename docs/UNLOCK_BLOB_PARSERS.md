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

## The gate is broken on these units

Every `GET` page on both units says `Fuse Value: 0x1` and **never emits
`Challenge:`** — the branch that prints the challenge is the one where the flag
was set. `ZZ0071` (the RNG wrapper) or the serialize step returns 0, so the flag
stays 0 and every POST dies at the state check with `400`. A `/devmode` GET or a
3 s quiet gap does not change it. Consequence: no status can currently separate
a parse failure from a state failure, and no blob can verify.

## Matrix (20 cases, `:1443`, `/info` checked after each)

| case (raw body) | GET | POST | notes |
|---|---|---|---|
| no body | 423 | 400 | state gate |
| `confirm=` (8) | 403 | 400 | |
| odd 1 hex char | 423 | 400 | would fail deserialize |
| 2 bytes hex | 403 | 400 | |
| 1,024-byte body | 403 | 400 | |
| 2,048-byte body | 423 | **reset** | exactly the cap; reader closes early |
| 2,049-byte body | 403 | 413 | |
| 65,536-byte body | 423 | **reset** | |
| `confirm=zzzz` (non-hex) | 423 | 400 | |
| percent-escaped `deadbeef` | 403 | 400 | |
| embedded `%00` | 423 | 400 | |
| `confirm=` mid-body (`x=1&confirm=…`) | 403 | 400 | prefix `strncmp` fails |
| `confirm=` at the end, then `&x=1` | 423 | 400 | trailing bytes are not hex |
| duplicate `confirm=` | 403 | 400 | only the raw body is scanned |
| devmode magic + bad ring | 423 | 400 | never reaches a ring parser |
| `+` terminator | 403 | 400 | |
| `/mfgunlock` empty | 423 | 400 | |
| `/mfgunlock` 4 KB body | 423 | 413 | |
| `/mfgunlock` 64 KB body | 403 | 413 | |
| `/mfgunlock` duplicate | 423 | 400 | |

The exact body limit is `0x800` bytes: at exactly the cap the reader closes
while the client is still writing (reset); one byte over answers `413`. Sizes
are raw body bytes including the 8-byte `confirm=` prefix, hence 2,048 rather
than the 0x400 the decoded buffer holds.
`/info` answered `200` after every case; no case crashed or wedged the daemon.

## What is still needed

A unit whose `ZZ0071` challenge path works (or a build where the flag is
seeded another way) to score the hex/percent/strip parser and the verifier.
Until then, the fields above are only bounded, not exercised past the gate.
Harness: `scripts/unlock_blob_fuzz.py`.
