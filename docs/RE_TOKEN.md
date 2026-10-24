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

## 2026-10-23 (provider map) — the ctx services and where they come from

The inspector's `ctx` (`a1`) is a stack struct built by the muse command
dispatcher `FUN_0047ad64`; services live at `ctx+4/+8/+12/+16/+20`. Two hard
facts from that builder:

- `0x47aeb8`-region calls `MuseDeviceImplProvider::getInstance` (`0x5fb44c`) and
  `getTargetIdProvider` (`0x5fb4ec`), then calls **slot 16** on the result into
  `sp+10684` (a string, disposed at `0x47b140`). That string is the
  **target id** and is not the issuer.
- The provider trio is a singleton of `MuseDeviceImplProvider` with getters
  `getActorProvider` (`0x5fb4a8`, member **+0**), `getTargetIdProvider`
  (`0x5fb4ec`, member **+8**) and `getTargetValidator` (`0x5fb534`, member
  **+16**); the setter is `0x5fb608` (shared_ptr stores at +0/+8/+16). Only
  three providers exist — there is no separate "policy provider".

The actor provider's class vtable is installed from the literal at `0x480518`
into `service+21644` (`0x47f624`). That literal is **`0x66c038`, in `.rodata`,
not `.data.rel.ro`** — this is why the earlier vtable sweeps found nothing:
they only covered `.data.rel.ro`. The table, with its `offset-to-top` at
`0x66c030` (`-0x548c = -21644`, so the vptr belongs to the +21644 subobject):

| slot | 0 | 4 | 8 | 12 | 16 | 20 | 24 | 28 | 32 |
|---|---|---|---|---|---|---|---|---|---|
| | `0x480bcd` | `0x480bf5` | `0x47b4e1` | **0** | **0** | `0x47b6ed` | `0x47b799` | `0x47bf4d` | `0x47b8a1` |

Two consequences:

1. **`ctx+16` cannot be the actor provider** — slot 12 (the call at `0x60226c`)
   is a null entry, and so is slot 16. Whatever `ctx+16` is, it is not this
   class.
2. The slot bodies are *trampolines*, not methods: e.g. slot 24 is
   `0x47b799: sb s0,30(a0); lb v1,14(s0); li a1,109; b 0x47b06a` — it stashes a
   **static base held in `s0`** at `a0+0x1e` and dispatches on an ID (`109`).
   Emulating `0x47b799` directly (zeroed `this`, `out`/`key` on the heap) writes
   the empty string and reads nothing of `this`, because `s0` is unset. So the
   slot cannot be run without the live service object; the derived bytes are a
   live read, not a static one.

`grep`-able corroboration for the value pair: `"Network Hash [%s], Muse
Household Id [%s]"` (`0x66f498`, logged from `0x48d3dc`) is emitted by the
household-settings path and prints the network hash next to the household id —
i.e. both are stored household state, consistent with `mhhid = hhid.luid_net`
and with the mDNS TXT values.

### Vtable sweep, corrected (`.rodata`, trampoline-aware)

The real vtables live in `.rodata` (`~0x665000-0x6bda40`) with a null typeinfo
word at `vptr-4`; 445 pass the shape test. Slot bodies are frequently
trampolines (`sb s0,30(a0); lb v1,14(s0); li a1,<id>; b <impl>`) that branch to
the implementation, so a scan must resolve at least one hop. Nine tables put a
0x30/0x48 struct writer behind slot 24:

```
0x66c694 -> 0x482a80   0x66e2dc -> 0x48a13c   0x66e300 -> 0x48a194
0x673c44 -> 0x4a227c   0x678c08 -> 0x4b9704   0x680318 -> 0x4dbd14
0x682444 -> 0x4e35c0   0x6b3760 -> 0x619658   0x6b4c2c -> 0x61cb1c
```

`0x6b3760` is a settings-cache class: slot 6 compares `this+80` against the
literal `"cacheSettings"` (`0x6b35ec`) and hands back `this+396` — i.e. a keyed
getter, which is the shape "getPolicy(key)" the inspector calls.

Running those slots with a zeroed object and a live `s0` (the claim value the
inspector keeps in `s0` — the trampolines stash it at `out+0x1e`) returns empty
strings for every one of the nine: each needs the constructed object. So the
issuer bytes remain a live read; the tables above are what to instrument next,
`0x6b3760` first.

## 2026-10-23 (stop point) — live values tried, R_ALLOW_SSH_PUBKEY_INSTALL

Two more live issuers, both only with the JSON `mhhid` that already reproduces
slot 12: `urn:dev:ops:44974-zp-RINCON_000E58CADE9401400` and the bare
`RINCON_000E58CADE9401400` — both 401. The dispatcher's target-id string
(slot 16 at `0x47aeb8`) is not exposed by `/status/zp`, the mDNS TXT or the
`groups` WS reply, so it could not be tried as an issuer; `/status/zp` does
carry `LocalUID`, `SerialNumber`, `HouseholdControlID` and `LocationId`.

**Honest stop point.** Static work has reached the limit: every remaining value
(issuer, hhid, luid, network_hash) is produced by the live service object the
inspector receives at `ctx+16`. A future emulator run needs:

- the object type behind `ctx+16` (nine `.rodata` candidate vtables are listed
  above, `0x6b3760` first) **constructed**, not zeroed — zeroed objects make
  every candidate return the empty string;
- its fields, which the callback reads as: issuer at `+0`, `hhid` at `+0x30`,
  `luid` at `+0x48`, "mhhid present" flag at `+0x18`, and — on the same
  dispatcher `ctx` — the expiration service at `ctx+12` (slots 28 and 40) and
  the time source at `ctx+20` (slot 60);
- the easiest real construction is the provider setter `0x5fb608`
  (`MuseDeviceImplProvider` +0/+8/+16), or the service constructor around
  `0x47f62x` that installs the actor vptr at `service+21644`.

## `R_ALLOW_SSH_PUBKEY_INSTALL` — what consumes it

- It is a **device-properties statement**, not a config key: the settings
  persistence layer logs `Removing persistent statement with
  R_ALLOW_SSH_PUBKEY_INSTALL` (`0x685e9c`, string use at `0x4f3b10`) and
  `R_ALLOW_SSH_PUBKEY_INSTALL may not be persisted` (`0x685efc`, use at
  `0x4f3cc4`), i.e. the statement is filtered out on load so it acts as a
  one-shot gate rather than stored state.
- The consumer endpoint **is `/ssh/fingerprints`**: it is a real route
  (`0x6f7168`, handler `0x4aa9ec`, flags `0x101`) that calls into `0x4cb910`,
  which builds a JSON body keyed `pubkey` (`0x67b8d8`, used at `0x4cba10`)
  together with the dropbear host key path
  `/jffs/persist/ssh/dropbear_ecdsa_host_key` (`0x67b898`, used at `0x4cba88`)
  and answers 200 or 403. So the pubkey install is driven through that route,
  and the `R_ALLOW_…` statement is what the persistence layer refuses to keep.
