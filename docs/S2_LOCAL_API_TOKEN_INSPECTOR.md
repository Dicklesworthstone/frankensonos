# S2 local API and the assertion inspector (86.10)

Owner LAN, owner devices. All identifiers below are from the owner's own
system; replace with placeholders before publishing elsewhere.

## The API needs an API key and an assertion

`https://<player>:1443/api/v1/...` is a local API distinct from the UPnP
control port. Two independent things are checked:

1. `X-Sonos-Api-Key` — any non-empty value is accepted by the local
   instances used here, but a *missing* key fails earlier with
   `ERROR_API_KEY_VALIDATION_FAILED`.
2. A bearer assertion (`Authorization: Bearer <JWT>`), or the device-grant
   call below.

Device authorisation endpoint:

```
POST /api/v1/players/{RINCON_...}/authorization/authorizeDevice
{"assertion": "<JWT>", "grantType": "urn:ietf:params:oauth:grant-type:jwt-bearer"}
```

- wrong `grantType` value → `403 ERROR_NO_PERMISSION`;
- correct grant type and a token that fails inspection → `401
  ERROR_NOT_AUTHORIZED`, body reason `Unauthorized.`;
- the `management/*` and `platformInternal/*` routes with a bearer token
  return the same 401 plus `WWW-Authenticate: ... invalid_token`.

## The inspector

`FUN_006021e4` in the MIPS `anacapad` (86.10; base `0x400000`) is the
assertion inspector. It is reached from `0x603cf8` after the JWT payload is
parsed into a token struct. Its checks and result codes, in order:

| code | condition | message string |
|------|-----------|----------------|
| 4 | `mhhid` absent or not equal to the household from the policy callback | "Device token not minted in this HH." |
| 5 | `exp` absent, or the numeric `exp` does not match its string form | "Device token missing/invalid expiration time." |
| 2 | `exp` in the past relative to now with the rotation marker set | "Token is expired / security settings have changed" |
| 6 | `iss` not equal to the issuer derived from the device policy | "Device token not minted by this device." |
| 0 | all checks pass | "Device token is valid." |

Claim names are a contiguous table at `0x6aff98`: `jti, iss, sub, aud,
iat, exp, systemId, riat, rexp`; the household claim key `mhhid` is at
`0x675746`. The inspector stores `iss` at token+0, the household at
token+0x6c/0x70 and the expiration at token+0xa4/0xa8; the final issuer is
compared against a value returned by a policy callback with a 3600 s
window, so `iss` is **device-derived**, not a constant.

## Chain source

The published eMudhra DV intermediate key (Oct 2026 incident) lets a leaf
be forged and chained to a root that is present in the S1 57.23 bundle
(`emSign ECC Root CA - G3`). The mint helper is
`captures/research/emudhra/mint_jwt.py` (RS256 and ES256 variants). See the
`emudhra` directory for the chain files.

## Current status

Against two 86.10-81100 players the forged assertion is rejected with the
generic 401 before any claim-specific feedback; varying `iss` (device UID,
household id, control id, `Sonos`, `https://sonos.com`), `mhhid` vs `hid`,
minimal versus full claim sets, and `exp` values all give the same answer.
So the remaining unknown is **pre-claim**: the exact issuer expectation
and, most likely, whether the 81100 trust bundle still contains the root we
chain to.

Next steps: extract the 81100 root bundle and compare it with 80260; obtain
one real device token to copy the claim encoding and the exact `iss`; then
re-test with the matching root and claim names.

## Structure from the decompiler (2026-10-23, same lane)

Two findings that change the search, from `FUN_006021e4` / `FUN_00604a20` /
`FUN_00605084` and the `.bss` initialiser at `0x46fa14`:

1. **The compared claim is `hhid`, not `iss`.** The inspector's only string compare
   in the failure path is `std::operator==(rendered claim, *(param_3 + 8))` where
   `param_3` is the parsed JOSE header and `+8` is slot 0 — and the header table
   (`FUN_00604a20`) registers `x5c`, `alg`, `kid`, so **`param_3 + 8` is `kid`**.
   The `hhid` check earlier in the function compares against a *computed* brace
   fingerprint `{owner{luid<…>}hhid<…>network_hash<…>}` built by `FUN_006020f8`.
2. **The token and its claims are `urn:`-shaped.** The claim formatter
   (`FUN_005f7210`) substitutes a tag that is a `.bss` `std::string` (`0x7048e8`,
   initialised at `0x46fa14`) holding **`urn:dev:ops:44974-zp-`**, and the same
   object is the needle of a `rfind(…, 0) != 0` prefix test that runs *before* the
   inspector and skips it on a mismatch. Sonos claim namespaces in the build:
   `urn:sonos:environment|user|hhid|udn|idtype|device`.

Payload field table (`FUN_00605084`): `jti iss sub aud iat exp riat rexp scp`.

Tried live on `.4.81` and still `invalid_token`: plain and `urn:dev:ops:44974-zp-`
prefixed claim names, bearer with and without the `urn:` prefix, `kid` as the UDN
and as `urn:dev:ops:44974-zp-<UDN>`, `idtype=device`, and the braced `hhid`
fingerprint with the household id filled in. Nineteen combinations, all identical.

`resolveToken` is not the oracle (it answers `success:true` with
`statusReason: UNKNOWN` for every variant). Since structure alone no longer
separates the failures, **an emulator run with the real daemon and its log is the
next instrument**; every reason string with its address is in `FIRMWARE_PATHS.md`
(`0x6af900` … `0x6af9ec`) and the logger category is `muse_token_inspector`.

Also note the live household is `Sonos_wI8BAkz9WLbe5jdUaRXQSQMJW8.T7aLjgf696_0Ij2e5ZDB`
and the location is `lc_3ee386abdf5745068202801414f1a14a`; the older `Sonos_qN6HO…`
value is stale. On the WebSocket channel `playerId` belongs in the **header** and
must be the UDN — a payload-side `playerId` answers `Missing playerId`, a locationId
answers `Invalid parameter: playerId`.
