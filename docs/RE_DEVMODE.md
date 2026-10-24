# RE_DEVMODE — live lane log

One lane of the 2026-10-24 root swarm. Facts with addresses/commands only; merge into FIRMWARE_PATHS.md when the lane closes.

## 2026-10-24: statement replay is deferred; both static questions answered

The statement is device-bound and no unauthenticated generator exists, so the
replay waits on a portal-issued base64 (the `:1443`/`:1400` live side is not the
blocker). This lane finished the two static reads instead. Sources: 86.10
`anacapad` (`captures/firmware/s2_overlay/bin/anacapad`, MIPS16 — `-m mips:16`)
and the S2 `libsonoscrypto.so.3` (PowerPC build, `fw/internals`, `ZZ0137` at
`0xca14`). Object field numbers below are **0x-hex**; GNU objdump prints MIPS
offsets in decimal.

### The S2 statement layout, from `ZZ0137` (`0xca14`)

`FUN_004f3b44` calls `ZZ0137(buf, len, model, devid_buf, devid_len, ver_hi,
ver_byte, &payload_off, &payload_len, &ver_a, &ver_b)`. The verifier, in order:

| offset | size | check |
|---|---|---|
| `+0x00` | u32 BE | `0x28721e71` (same bytes as S1's LE magic) |
| `+0x04` | u32 BE | equals the caller's model value (page reports it) |
| `+0x08` | 16 | free |
| `+0x18` | u32 BE | **lower version bound**, inclusive: `r0 >= word` |
| `+0x1c` | u32 BE | **upper version bound**, inclusive: `r0 <= word` |
| `+0x20` | u8 | devid length, must equal the caller's expected length |
| `+0x21` | n | device id, `memcmp` against the caller's buffer (`0xcbd4`) |
| `+0x21+n` | u8 | payload length; `len >= devid_len+34+payload_len` |
| … | | payload |
| … | | signature ring, handed to the parser (`0x20400`) |

The two window tests are `cmplw 7,r0,word` + `bc 13,28` (exit if `r0 < +0x18`)
and the same with `bc 13,29` at `0xcb8c` (exit if `r0 > +0x1c`), so **both
bounds are inclusive** — a statement with `+0x18 == +0x1c == r0` passes. The
earlier "max/min, strict" reading here was wrong; the app's
`rev([+0x18]) <= r0 <= rev([+0x1c])` is the same test and was right.

`r0` is built at `0xcb20` as `(arg6 << 16) | arg7`. The caller passes two
object bytes: `+0x49d` (arg6) and `+0x49e` (arg7). `+0x49e` is a u16 written by
`FUN_004dda10` (`0x4dda2e`, from `FUN_0051927c` → the u16 at object `+0x56a`) and
again by the update coordinator `FUN_004db988`; `+0x49d` is set from the high
byte of that coordinator's counter and is zero on the devmode path. On a
86.10 unit the byte read is the major-version byte, so `r0` is `0x56` (86) and
a statement must carry `+0x18 <= 86 <= +0x1c`.

The payload length is **one byte** (`lbzx 9,31,27` at `0xcbf4`).

### What `ZZ0137` actually signs — the device id is inside the signature

The verifier does not hash a separate digest field. `ZZ0123` hands the
signature-container verifier (`ZZ0126`) three things: the parsed ring context,
the statement pointer, and the ring offset. `ZZ0126` then computes the digest
over `statement[0 .. ring_offset)` — the entire clear preamble, payload
included:

- S1: the hash callback is called at `0xa888` with `algo=1, data=statement,
  len=0x2e`, and `0x2e` is exactly the ring offset for a 12-byte id and empty
  payload (`devid_len+34+payload_len`).
- S2: the same call is at `0xa7d4` with `data=statement, len=0x2e`.

So **everything the parser reads is signed**: magic, model, the 16 "free"
bytes, both version bounds, the id length, the id itself, the payload length
and the payload. The ring that follows is the only unsigned material — it
carries the key reference and the signature. `memcmp` of the id at `0xacc4`
(S1) / `0xcbd4` (S2) is therefore a redundant pre-check for a *forged* id, not
the security boundary.

Consequence: a public statement for the same model **cannot be patched** to a
different serial; the signature breaks and the only free fields are the ring
internals. A per-serial portal statement is required.

Emulator demonstration (`scripts/emu_devmode_stmt_{s1,s2}.py`): flipping a byte
in the 16-byte free area at `+0x08` still reaches the verifier (the byte is
unchecked but hashed); changing the id fails at the `memcmp` before any hashing.
On the wire both classes answer `403`, so the emulator, not the status, is the
discriminator.

### `FUN_004f38a0` cannot be driven past 255 — the 256-byte frame is exact

`FUN_004f3b44` passes `ZZ0137`'s `payload_len` output as `param_3`
(`0x4f3bda` stores it to `+0x408`); `FUN_004f38a0` (`0x4f38a0`) then:

1. `memset(param_4, 0, *param_5)` with `*param_5 = 0xff`;
2. `uVar4 = old_len` (`+0x408`); if `param_3 != 0`, `*param_5 = max(uVar4, param_3)`;
   `a3` is that max at `0x4f38fe`;
3. `memset(local_118, 0, 0xff)` (255 of the 256 bytes);
4. `memcpy(local_118 + (X - Y), …, Y)` where `{X,Y} = {param_3, uVar4}`, so the
   copy ends at `max(param_3, uVar4)` — at most index 254;
5. the XOR loop runs `*param_5 = max(param_3, uVar4)` times, reading
   `local_118[0..max-1]` and writing the same count to the caller's
   `local_130[64]` (256 bytes).

Both lengths are the same u8 (`+0x408` is only ever stored from `param_3`), so
`max <= 255`: mask and output fit exactly, one byte spare. `FUN_004f3974`
(no-statement notification) passes `param_3 = 0` and is the same bound.
**There is no reachable >255 payload on this build**; the 256-byte frame would
only overflow if a later `ZZ0137` returned more than a byte.

### `POST /mtmhhsetup` — valid shape and what it persists

Handler `0x4ac11c` (table entry `0x6e714c`, flag `0x100` — the same locked
class as `/unlock`). Live check: `GET /mtmhhsetup` is `403` on both locked
86.10 units while `/unlock` answers `423`, so the handler body only runs after
the lock is lifted. It also requires an `X-Sonos-Api-Key`. The header is
not merely presence-checked here: `0x4aa99c` hashes the value with `ZZ0058`
(algorithm 1 = SHA-256-size, 32-byte output) and `memcmp`s it against two
built-in 32-byte constants (`0x676ea8`, `0x676e88`); a mismatch is `500` with
an empty message. The published UUID dummy is known to pass live (the token lane reached the
inspector on this same build with it), so one constant corresponds to it under
whatever `ZZ0058` derives, but `any non-empty value
passes` is not true of this build.

Request:

- method `POST` (else `405`); `Content-Type:
  application/x-www-form-urlencoded` (else `415`); `Content-Length` 1–4095
  (`411`/`413`); body read into `sp+40`.
- form fields (name cap 32, value cap 128):
  - `hhid` — optional. Empty (`hhid=`) generates `Sonos_MTM_` + 22 chars from
    `[0-9a-zA-Z]` (`0x4ac31e` copies the 10-byte prefix, `ZZ0154` fills 22 from
    the alphabet at `0x6769a0`). Non-empty must start with `Sonos_MTM_`
    (`strncmp`, 10), be shorter than 33, and contain only alnum/`_`
    (`FUN_004fa3d8`). Copy cap 33.
  - `name` — optional; copied (128) and normalized by the UTF-8 sanitizer
    `FUN_00595528`; accepted only if `strlen < 65` and the value already equals
    its sanitized form (else `400 invalid name`).
  - `icon` — optional; `< 129` bytes and only alnum/`:`/`-` (`FUN_004fa3d8`
    with `":-"`; else `400 invalid icon`). Default
    `x-rincon-roomicon:generic`.
  - `key` — required; exactly 24 chars, base64url-decoded to **16 bytes**
    (`400 invalid key` otherwise).
  - `reboot` — optional integer; `>= 30` becomes 30, 0 stays 0, a negative
    value leaves the default 1. `>= 1` calls `sonosForceReboot(n)` after the
    reply.
- Precondition: `HouseholdID` must be absent or empty (`getSetting` status and
  first byte, `0x4ac25c`), i.e. a factory-reset player; otherwise
  `500 not factory reset`.

Effect (`0x4ac564` onward): `ZZ0215` derives 16 bytes from the decoded key,
the literal `MTM_` and the hhid; `RserializeData` renders that to 33 bytes and
`int_addOrRemoveSettings` writes **`HouseholdID=<hhid>`** and
**`WEPKey=<derived>`** into the fixed file `netsettings.txt`. With a non-empty
`name` it also writes `ZoneName`, `Icon`, `Configuration=1` and
`TargetRoomName=""` into the room settings file. Success is `200` with
`{"hhid":…,"key":…,"rebootDelay":…}`; every failure is the JSON
`{"error":code,"message":…}` under the matching status.

### Portal procedure for the owner (the only source of a statement)

The statement is per-serial and fully signed, so this is the path. Do it in a
normal browser signed in to the Sonos account (the same login as the app):

1. Open `https://developer.sonos.com/devmode/`; it redirects to
   `login.sonos.com` — sign in. (The page is a Salesforce community page and
   only exists after login.)
2. Read the unit's **Device ID** from `http://<player>:1400/devmode`: the
   `Device ID:` line, 12 hex characters, no separators. For the Play:5 that is
   `000E585D8C10`; for the Play:1 `000E58CADE94`. If the form also asks for a
   model, use `16` (Play:5) or `8` (Play:1).
3. Submit and copy the returned **base64 statement**. It is one line; the
   portal's own statements decode to under 1 KB, and the daemon rejects a
   decoded length above `0x400`, so keep it under ~1360 base64 characters.
4. Paste it here. It goes to `POST http://<player>:1400/devmode` with the
   page's fresh `csrfToken` and `button=submit`, spaced >= 5.5 s (closer spacing
   makes the daemon sleep ~2.1 s per missed interval, which is the `4 s`
   responses seen in testing).

Then check, in order: the POST answers `200`; `GET https://<player>:1443/unlock`
stops answering `423`; TCP 2323 and 22 accept; `GET /testenv` answers `200`
instead of `403`.

Parser facts for judging a candidate: `len < 4`/`< 8`/`< 28`/`< 32` are silent
rejects; `devid_len` must equal the expected length and the bytes must match;
on S2 the version word must lie inside `[+0x18, +0x1c]` (both inclusive), and on
S1 `[+0x18] < 0x390018` and `[+0x1c] >= 0x390017`; the ring is then parsed and
RSA-verified against the pinned key. S1 `memcmp` is at `0xacc4` (id at `+0x21`,
length at `+0x20`, expected length arg), S2 at `0xcbd4`.
