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

## `/ssh/fingerprints` — exact request contract (86.10, handler `0x4aa9ec` -> `0x4cb910`)

Read end to end. It is a **read-only fingerprint reporter for the device's own
dropbear host key**, not an uploader — nothing is written.

Request: **GET**, no parameters and no body consumed (every input comes from the
host-key file). Route entry `0x6f7168`, flags `0x101`; the response writer is the
framework (`ap_reply_status` + `0x477d78`/`ap_write_end`).

Gates, in order:

1. `RdeviceIsUnlocked()` (imported, PLT `0x6db710`, call `0x4cb94a`) — false →
   **403** with the stock HTML `Error 403 / Forbidden` page. This is the devmode
   unlock flag `/tmp/device_unlocked_flag`; `R_ALLOW_SSH_PUBKEY_INSTALL` is the
   *statement name* that sets it (and which the persistence layer strips, see
   above), so the statement is the one-shot key to this route.
2. `sonosFileExists("/jffs/persist/ssh/dropbear_ecdsa_host_key")`
   (`0x4cb956`) — missing → **500**.
3. The host key file is read and `strtok`-split (`0x4cb780`); the type token must
   `strcmp` equal **`ecdsa-sha2-nistp256`** (`0x4cb98e`) — anything else → **500**.
   So the accepted key is an OpenSSH one-line P-256 ECDSA key
   (`<type> <base64 blob> [comment]`, whole key ≤ 656 bytes).

Response **200**, `Content-Type: application/json` (`0x667d5c`), body built with
a JSON writer:

```json
{"ecdsa-sha2-nistp256":{"pubkey":"<base64 blob>","sha256-base64":"<32 bytes>","md5-hex":"<16 bytes>"}}
```

The outer object name is the key-type string taken from the file; `pubkey` is
the blob verbatim (`0x4cb834` base64-decodes it, then `ZZ0058` with selector 1
yields the 32-byte SHA-256 rendered base64 and selector 2 the 16-byte MD5
rendered hex, into fixed 32/16-byte stack buffers at `sp+3908`/`sp+3780`).

### S1 (57.23) — same source, not the same answer

The 57.23-74170-1-12 and 57.19-46310-1-12 images both contain the route in their
table (`0x53290c`, handler `0x428998`, flags `0x11`) and the identical literals
(`/jffs/persist/ssh/dropbear_ecdsa_host_key`, `pubkey`, `sha256-base64`,
`md5-hex`, `/tmp/device_unlocked_flag`), so the contract above is the same code.
**Live, though, two 57.23-74170 players answer `404` on both `:1400` and
`:1443`** (both Play:5, `.202` and `.84`), while 86.10-81100 answers `403`
(`.4.81`, `.4.86`) — i.e. on the shipped S1 build the entry exists but is not
served, so the route is usable on S2 only.

## 2026-10-24 · provider map, first pass (MuseDeviceImplProvider, S2 86.10)

Read against `captures/firmware/s2_overlay/bin/anacapad` (MIPS16). Facts, plus two corrections
to the starting assumptions.

- The actor vtable is `0x66c038`. Slot at byte offset `0x18` (index 6, "slot 24") holds
  **`0x47b799`**, which is the ISA-bit form of entry `0x47b798`.
- `0x47b798` is `save 32,ra; jalx 0x51b9b4; li a1,20; restore; jrc ra` — i.e. **that slot's id
  is 20, not 109**. The id is consumed by the `0x51b9b4` call, not by the shared body: the body
  at `0x47b06a` overwrites `a1` at `0x47b094` (`move a1,s1`) and never reads the incoming id.
- id **109** appears at `0x612916` and `0x612954`, in the muse provider region, each followed by
  `jal 0x5fef64`. `0x5fef64` is a two-word setter, `sw a1,4(a0); sw v0,0(a0)` — it stores a
  `{ptr, len}` pair, i.e. the getter hands back a `string_view` over provider state.
- `0x51b9b4` is the shared id→string getter. **Ghidra mislabels it** as `operator_delete`
  (8 bytes) and the MIPS16 disassembly from that address is misaligned, so the id→field table
  inside it is not yet readable; a correct entry point is the next step.

So the shape is: vtable slot → trampoline that calls the getter with a fixed id → shared body
that returns the `string_view`. Whether the issuer is a constant or a settings/household string
therefore lives in the getter's id table, which is the one thing still unread.

## 2026-10-29 · CORRECTION: `0x51b9b4` is `free`, and the issuer getter is slot 16 on `ctx+4`

The 2026-10-24 note ("`0x51b9b4` is the shared id→string getter") is wrong. Read as MIPS32 —
`jalx` to an **even** target means the callee is MIPS32, which is why the MIPS16 decode was
misaligned — `0x51b9b4` is eight bytes:

```
51b9b4  lui   t9,0x52
51b9b8  addiu t9,t9,-17988      ; t9 = 0x51b9bc
51b9bc  <_ZdlPvj>               ; gp setup, lw t9,-32724(gp), b 0x51bb04 -> jr t9
```

`_ZdlPvj` is a 0x18-byte PLT stub (dynsym `0051b9bc g DF .text 00000018 _ZdlPvj`); its chain
`0x51bb04 -> 0x6db060 -> .rel.plt 0x6f6838` resolves to **`free`**. Emulating any of the
trampolines (`0x47b799`, `0x47b731`, …) with a fake `this` calls exactly one PLT entry —
`free(a0, a1)` — and writes **nothing** to `a0`. The `li a1,<id>` in the delay slot is the size
argument, not an id.

So the vtable slot at byte 24 (`0x66c050` = `0x47b799`) is a **destructor**, not the issuer getter.

### What the inspector actually does (`FUN_006021e4`, 0x6021e4–0x602400)

```
602210  a0 = sp+32 ; a1 = *(s1+16) ; a2 = *(s0+12) ; call slot 24   ; free(sp+32)
602248  lbu v0,56(sp)                                              ; flag at sp+56
602256  a1 = *(s1+4) ; a0 = sp+32 ; call slot 16                   ; <-- ISSUER
602262  a0 = s0+7 ; a1 = sp+80 ; call 0x51e434                     ; compare
60226c  a0 = sp+128 ; a1 = *(s1+16) ; call slot 12                 ; network_hash
602278  a0 = sp+192 ; a1 = sp+32 ; a2 = sp+128 ; call 0x6020f8     ; composer(out, luid, net)
602400  a0 = sp+32 ; a1 = *(token+8) ; call std::operator==        ; iss == sp+32
```

`sp+32` is written only by the slot-16 call and read by the composer (as the **luid**) and by the
issuer compare. So:

- the issuer getter is **slot 16 on the object at `ctx+4`**, not slot 24 on `ctx+16`;
- the token's `iss` must equal that getter's output, and the same value is the fingerprint's
  `luid` — i.e. `iss` and `luid` are one value, and the pre-gate only additionally requires it to
  start with `urn:dev:ops:44974-zp-`.

### The id→string getter that does exist

`0x5f7210(out, id, default)` is a real getter: it string-constructs from a literal and then
`find`s the id in it (`0x5f6f1c` ctor, `0x5f71d4` find). The literal for the id-25 call at
`0x601ffc` is `0x6649d4` = `"/opt/conf/anacapa.conf"`, so id 25 is a config-path getter, not the
issuer. The issuer's own getter is the slot-16 body on whatever class `ctx+4` holds — that class,
not `0x51b9b4`, is the next read.

### Call order confirmed under emulation, and one open point

Running `FUN_006021e4` with a logging provider object confirms the three virtual calls in the
order the disassembly shows — `RA=0x60221f` (slot 24), `RA=0x602263` (slot 16), `RA=0x602279`
(slot 12) — all on the same `this`. So the mapping in the correction above is right.

Open point, stated plainly: the caller passes `a1 = sp+32` of its own frame (`0x603c76`), and the
policy builder `0x604fe8` was called on that same `sp+32` (`0x603c3e`), so the inspector's `ctx`
and its `a3` policy are the same address. But `0x604fe8`'s first act is `0x5fef64(a0, 8, lit)`,
which stores `{lit, 8}` at `{+0, +4}`, and the inspector then does `lw a1,4(s1)` / `lw v0,0(a1)` —
i.e. it treats `+4` (= 8) as an object pointer and would read address 8. Emulating with a real
built policy faults exactly there. So either the policy is not the ctx, or something rewrites
`+4` between the build and the call. That is the next thing to read, and it gates the slot-16
body.
