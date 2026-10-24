# RE_TOKEN — live lane log

One lane of the 2026-10-24 root swarm. Facts with addresses/commands only; merge into FIRMWARE_PATHS.md when the lane closes.

## 2026-10-23 · composer byte-for-byte (bead frankensonos-xnf)

`FUN_006020f8` is the fingerprint composer, and it is a plain ordered writer, not
a hash. Its four literals are resolved pc-relatively from the pool at `0x6021d4`:

| pool slot | literal | address |
|---|---|---|
| `0x6021d4` | `owner` | `0x677360` |
| `0x6021d8` | `luid` | `0x6af8d0` |
| `0x6021dc` | `hhid` | `0x67697c` |
| `0x6021e0` | `network_hash` | `0x6af8d8` |

The write order (each literal goes through `J(0x602050)`, each brace through
`J(0x602094)`, each claim string through `J(0x602074)`):

1. `0x602094(0x7b)` → `{`
2. `0x602050(owner)`; `0x602094(0x7b)` → `{`; `0x602050(luid)`
3. `0x602074(policy + 0x48)` — **luid**; `0x5f7420(0x7d)` → `}`
4. `0x602050(hhid)`; `0x602074(policy + 0x30)` — **hhid**
5. `0x602050(network_hash)`; `0x602074(arg3)` — **network_hash**
6. `0x5f7420(0x7d)` → `}`

`0x602074` consumes a `std::string` as `{data @ +0, size @ +4}`, so the two policy
slots are borrowed, not copied. Separators/spaces are emitted by the writer state
in `0x602010`/`0x602020` (`,` vs `:` vs plain), so the rendered text is the
`{ owner { luid <…> } hhid <…> network_hash <…> }` shape the earlier 401s already
assumed — the template was never the unknown.

What the caller supplies:

- `a3` (`0x603cf2`) is the *policy* struct; `a2` is the token. Inside
  `FUN_006021e4` the policy lands at `sp+252` and the token at `sp+248`.
- `0x60228c`: `lbu policy+0x6c` is the "mhhid present" flag; on zero the inspector
  returns code 4 without composing.
- `0x602294`: compares the composed string with the **policy's own**
  `policy+0x70` string via `std::operator==`. So `mhhid` is checked against the
  device's fingerprint, not against the token's other claims — a token cannot
  describe its own household.
- `0x60226c`..`0x602276`: `network_hash` is produced by a virtual call,
  slot 12 of the object at `ctx+16` (ctx = inspector `a1`), into `sp+128`; the
  policy itself arrives from slot 24 of the same object at `0x602210`. Both are
  live device state, so reproducing `network_hash` still needs the `ctx+16`
  implementation, not the composer.

Policy struct shape from `FUN_00604fe8` (its builder; `J(0x604f40)` zeroes a
28-byte `optional<std::string>`): `std::string` at `+8`, `+32`, `+56`;
`optional<std::string>` at `+108`, `+136`, `+164`, `+204`; three zero words at
`+192`. The composer's `+0x30` / `+0x48` are therefore offsets into the object
that slot 24 returns, not proof of a fixed layout on the policy base.

Instrument: `captures/research/hhid_emu.py` (Unicorn 2.1.4, MIPS16 via an odd
entry PC, PLT stubs patched to `jr ra` and serviced in Python). It runs the
composer; the policy builder still faults at `0x0` on the `_Znwj` path.

## 2026-10-23 (cont.) — the inspector, executed

`captures/research/hhid_emu.py` now drives `FUN_006021e4` itself: a fake `ctx`
with per-slot Python stubs, a fake provider for slots 12/24, and logging stubbed
off (`0x5a1494` returns 0, otherwise the un-started logger jumps through a null
`.bss` pointer). Results, all observed:

- The compare at `0x602294` is `composed == a3+0x70` where `a3` is the third
  argument. Matching both sides moves the result from **4** to **5**; a mismatch
  returns 4 immediately. Code 4 is exactly the mhhid fingerprint gate.
- The composed text, from the live composer under emulation with the household
  triple, is
  `{"owner":{"luid":"T7aLjgf696"},"hhid":"Sonos_wI8BAkz9WLbe5jdUaRXQSQMJW8","network_hash":"0Ij2e5ZDB"}`
  — compact JSON, no spaces, insertion order owner/luid, hhid, network_hash.
- `a3` (fingerprint carrier, `+0x6c` present flag, `+0x70` string) is a
  232-byte claim-set built by `FUN_00604fe8` — the same layout as
  `muse::v1::t::CommandHeader` (`FUN_006083e0`): strings at `+8/+32/+56`,
  `optional<string>` at `+108`, `+136`, `+164`, `+204`, a vector at `+192`.
  So `+0x6c` is a claim slot, not a policy field.
- The issuer check compares the derived value at `0x602400` against
  `a2+8` — i.e. `iss` must equal what the provider derives.

The provider object is `ctx+16`; slot 24 (`0x602212`) returns the issuer and the
hhid/luid pair, slot 12 (`0x60226c`) returns `network_hash`. Nothing in the
binary fills an object at `+0x30/+0x48` with immediate-offset stores, so the
provider copies an existing structure — that copy is the remaining unknown, not
the template.

## Live facts (owner LAN, 2026-10-23)

The triple is broadcast, unauthenticated, in the mDNS TXT record and SSDP:

| field | value for this household |
|---|---|
| TXT `hhid` | `Sonos_wI8BAkz9WLbe5jdUaRXQSQMJW8` |
| TXT `mhhid` / `HouseholdControlID` | `Sonos_wI8BAkz9WLbe5jdUaRXQSQMJW8.T7aLjgf696_0Ij2e5ZDB` |
| TXT `locationid` | `lc_3ee386abdf5745068202801414f1a14a` |
| SSDP `X-SONOS-INFOHASH` | per-device, e.g. `gO3he3_8kpXAHD8WcNXpHRFhZbqXsn7cXGH6bXeKZ3w` |

So `mhhid` = `<hhid>.<first>_<second>`; luid/network_hash are plausibly those
two segments. 27 live combinations were rejected (`iss` = URN + RINCON / serial
/ hhid / split / location; claims `mhhid` vs `urn:dev:ops:44974-zp-mhhid`;
`exp` numeric and string; `mhhid` as JSON with each split, as the plain dotted
string, and with luid=location/infohash). All answered `401 ERROR_NOT_AUTHORIZED`
with an identical body, so a wrong claim and a wrong issuer are indistinguishable
from outside — the provider copy is what has to be read next.

## Oracles that exist (and the ones that do not)

- `POST /api/v1/players/{id}/authorization/authorizeDevice` is the only endpoint
  that runs the inspector; it is all-or-nothing (401 vs a grant). The failure
  code is logged only on `muse_token_inspector`, and none of `/status/dmesg`,
  `/status/jffs/*.log`, `/status/tmp/*` returns anything on 81100.
- `POST /api/v1/households/{hhid}/authorization/tokens` (resolveToken) answers
  `status: UNAUTHORIZED`, `statusReason: UNKNOWN` for every variant.
- `getProtectedAdminSettings` over the WebSocket answers
  `insufficient_scope` for a garbage bearer as well as for a candidate, so it
  does not reflect inspection either.
- `/status/zp` and the mDNS TXT are the useful *data* oracles:
  `HouseholdControlID`, `LocationId`, `SerialNumber`, `LocalUID`.

## 2026-10-23 (run) — slot 12/24 under the emulator, and the wire result

Stub provider, real inspector: slot 12 returns `0Ij2e5ZDB` (the TXT value of the
mhhid tail) and slot 24 returns the issuer; with `mhhid` set to the composed JSON
the mhhid compare passes and execution enters the expiration stage. The stage
after it depends on `ctx+12` slots 28/40 and `ctx+20` slot 60, which the stub
does not model yet, so it stops there without printing a result code.

Minted anyway with `iss = urn:dev:ops:44974-zp-RINCON_000E58CADE9401400` and
`mhhid = {"owner":{"luid":"T7aLjgf696"},"hhid":"Sonos_wI8BAkz9WLbe5jdUaRXQSQMJW8","network_hash":"0Ij2e5ZDB"}`:

- `POST .../authorization/authorizeDevice` → `401 ERROR_NOT_AUTHORIZED`.
- `POST .../management/reboot` with the same bearer → `401 invalid_token,
  "Access token is invalid"`.

Both checks agree with the emulator only as far as the format; the values still
come from a provider we cannot instantiate, so the next instrument is that
provider, not more permutations.
