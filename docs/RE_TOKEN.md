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
