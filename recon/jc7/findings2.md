# jc7 — instruction-level audit of the two "fixed copies" and the handshake gate

Pane omp_2 (Lane B), 2026-10-28. Binary `captures/firmware/s2_overlay/bin/anacapad`
(86.10, MIPS32 BE, `.text` at file `0x66d80` / vaddr `0x466d80`; MIPS16 code, decoded
with `mips-linux-gnu-objdump -m mips:isa32`). Read-only.

Three results. The first two retire the remaining "fixed-size copy" thesis; the third
turns the lane's practical blocker into an exact, named condition.

## 1. `execute` has no 0x2500 / 0x2800 copy — the real sizes are 37 and 40

`docs/RE_1443_JSON_LENGTH.md` §"The sink" claims, from a Ghidra render:

```
strlcpy(this + 0x6b, local_60[0], 0x2500)
memcpy(&local_144, <virtual call at *(this+0x424c)+0x20>, 0x2800)
```

Neither constant exists in `RMuseController::execute(Header const*)` (`0x4eb1a0`).
Every `li` of a size register in the whole function (`0x4eb1a0..0x4ec400`) is:

```
0, 1, 4, 37, 40, 16, 5, 4, 6, 255
```

and no `addiu a0,<this>,107` (`this+0x6b`) appears anywhere in it. The two copies that
do exist are:

**a. `memcpy(sp+9660, (*(this+0x424c))->vtable[0x20](), 40)` — `0x4eb530..0x4eb54c`**

```
4eb52c  lw   v0,9984(sp)        ; this
4eb530  lw   a0,16972(v0)       ; *(this+0x424c)
4eb534  lw   v0,0(a0)           ; vtable
4eb536  lw   v0,32(v0)          ; slot 8 (0x20)
4eb538  jalr v0
4eb53c  addiu a0,sp,9660        ; dest
4eb542  move  a1,v0             ; src
4eb544  lw    v0,-32648(s0)     ; memcpy@plt (0x6db7d0)
4eb54a  li    a2,40
4eb54c  jalr  v0
```

Fixed size, fixed destination. The only OOB shape left is a producer returning fewer
than 40 readable bytes; 40 is small enough that any member struct qualifies, and the
destination is a 40-byte field at `+40` of the `RCorrelationInfo` at `sp+9620` (which
is filled immediately before by `RCorrelationInfo::operator=` at `0x59f394`, a 37-byte
copy — the second fixed copy).

**b. `RCorrelationInfo::operator=` — `0x59f3b0` `li a2,37`.** Both sides are the same
type, so this one cannot be wrong.

## 2. `*(this+0x424c)` is the ctor's `a3`, i.e. `RMuseAuth const&`

The ctor is `RMuseController::RMuseController(RReadWriteStream&, RMuseMessageSender&,
RMuseAuth const&, RMuseContext&, optional<reference_wrapper<IAsyncCommandQueue>>,
RMuseNamespaceFactory const&, unsigned)` at `0x4e8458`. Its prologue is
`save a0-a3,80,ra,s0-s1`; the MIPS16e save layout puts `a0-a3` in the 16-byte argument
home area at the top of the frame (`80..92`) and the incoming stack arguments above it
(`96..108`). The ctor's stores line up exactly:

| source | member | type |
|---|---|---|
| `a1` | `+0x10` | stream |
| `sp+96` | `+0x18` | context |
| `sp+100` | `+0x04/+0x08` | optional (present byte + pointer, copied verbatim) |
| `sp+104` | `+0x0c` | namespace factory |
| `sp+108` | used as a value | unsigned |
| `sp+88` | `+0x4248` | `a2` = `RMuseMessageSender&` |
| `sp+92` | `+0x424c` | `a3` = `RMuseAuth const&` |

The `optional` copy is the discriminator: `lw v0,100(sp); lbu a0,0(v0); sb a0,4(s0);
lbu a0,0(v0); beqz a0; lw v0,4(v0); sw v0,8(s0)` is an `optional<reference_wrapper>`
being duplicated field for field, which only works if `sp+100` is that argument.

Cross-check on the uses: `+0x424c` is passed as the 4th argument of a virtual at
`0x4e88a6` (`lw a3,16972(s1)`) and stored into a telemetry record at `0x4e950c`;
`+0x4248` goes into a telemetry record at `0x4e92c2`.

So the producer is a virtual on the **auth object**, not on a parsed-JSON object: the
40-byte copy cannot be driven by a JSON string length at all.

## 3. The handshake gate is `state == 52` — `hasAuthToken` only gates token capture

`prepareWebsocketHeader` (`0x4e8d7c`) is a short-circuit chain: `updateHeaderTargets(this,1)`,
then `captureWebsocketCommandToken(this)`, then `activateNamespaceFromHeader(this,1)`; the
first non-zero return is the answer. `updateHeaderTargets` ends with

```
4e8d34  lw v0,10344(s1)     ; state
4e8d38  cmpi v0,52
4e8d3a  li v0,0
4e8d3c  bteqz 4e8d40        ; state == 52 -> return 0
4e8d3e  li v0,16            ; otherwise 16
```

so the gate is **`state == 52`**. `captureWebsocketCommandToken` (`0x4e7d44`) opens with the
same test and, when it fails, jumps to `0x4e7e76`, which returns 0 when the state *is* 52 and
16 otherwise — i.e. `hasAuthToken() == 0` is **not** a failure there, it only skips the token
capture. `activateNamespaceFromHeader` (`0x4e86f0`) returns 16 at `0x4e8ae2` when the
namespace cannot be resolved.

`+0x24f4` is written by `updateHeaderTargets` at `0x4e8c60` (`sb v0,9460(s1)`) from the
return value of the last call in a chain of header lookups. That call resolves through the
GOT to `hasAuthToken`:

```
gp = 0x6ffb60 (li v0,33; la v1,4efb60; sll v0,16; addu v0,v1)
4e8c52  lw v0,-28760(s0)   -> 0x6f8b08 -> 0x4e7a3d = hasAuthToken|1
4e8c5a  jalr v0
4e8c60  sb v0,9460(s1)
```

`RMuseHeaderHandler::hasAuthToken()` (`0x4e7a3c`) is `addiu a0,484` (MIPS16 two-operand
addiu, so `a0 += 0x1e4`) then `RJsonDataBinding::isPresent()` (`0x5f18dc`) — a pure flag
read (`lbu v1,33(a0); li v0,9; and; neg; srl 31`), i.e. **presence, not validity**.

### The binding at `+0x1e4` is `authorization`, and the block is not HTTP headers

The handler's binding names are a literal table at `0x600c24`:

```
namespace apiVersion command cmdId groupId householdId sessionId playerId
deviceId userId locationId corrId REDACTED authorization async intendedTargets
```

Bindings are 36 bytes apart from a base of 16, so index 13 (`authorization`) is exactly
`+0x1e4` — the field `hasAuthToken` tests. The names are **lowercase** and match the muse
frame header vocabulary (`x-sonos-method: `, `x-sonos-uri: `, `soapaction: `), not HTTP
header casing. `RAuthSplitToken` (`0x5f8e88`) then splits the value on `bearer `/`basic `
(`strncasecmp`, strings at `0x6ac2e0`/`0x6ac2e8`), skips spaces, scans to the first byte
`<= 0x20`, and requires `n >= token_len` before assigning into a `std::string`.

`checkIfValid` (`0x4e8318`) is what produces the state: `memset(out,0,32)`, `out[0] = 52`,
then `out[0] = 72` if any of the bindings at `+0xc4/+0xe8/+0x10c/+0x130/+0x154/+0x19c` is
present — by the same 36-byte table those are `householdId`, `sessionId`, `playerId`,
`deviceId`, `userId`, `locationId`, so all six must be **absent**.
`updateHeaderTargets` also sets 72 and logs `explicitTargets: too many targets` when
`*(this+0x24d8) >= 33`.

### Wire result (read-only, `.86`, 86.10-81100)

All of these answer `400` with an empty body, ~0.34 s (constant, so the handler does run):

| shape | result |
|---|---|
| upgrade, no extra header | 400 |
| + `Authorization: Bearer x` | 400 |
| + `authorization: Bearer x` (lowercase) | 400 |
| + `X-Sonos-Api-Key` / `X-Sonos-Corr-Id` / the whole outgoing header set | 400 |
| + `namespace: topology` / `zoneGroupTopology` / `household` + `apiVersion: 1` | 400 |
| upgrade + a pipelined 6-byte frame (`1001 0000`, `0101 0000`, …) as a GET body | 400 |
| `Host: lechmere.1.ws.sonos.com` | 400, but from the **static** handler (`text/html` + CSP headers) |

So an `Authorization` header alone does not move it, and neither does a header-shaped
namespace. The 400 is the `state != 52` / empty-namespace path, and the block the bindings
read is the muse frame, not the HTTP request headers.

### Next lever

`activateNamespaceFromHeader` resolves the namespace through `*(this+12)` (the namespace
factory) with `*(this+24)` (the context) and returns 16 on failure. The frame reader is at
`0x4a40b8` (8-byte prefix read via `lw v0,100(v0)` into `sp+2436`, then a second virtual,
then version/type validation with return codes 8/9/10). Reading that validator gives the
exact accepted version/type pair, which is the missing piece for a pipelined frame.


## Honest state of the bead

- u16 length truncation: self-consistent, no overrun (previous pass).
- "34148-byte object": a buffer-end argument, not an allocation (previous pass).
- "0x2500 / 0x2800 fixed copies": do not exist; the real fixed copies are 37 and 40
  bytes, both fixed on both sides, and the 40-byte source is a virtual on the auth
  object, not on parsed JSON (this pass).
- The gate is `state == 52`, not `hasAuthToken`; the state is 72 when any of
  `householdId/sessionId/playerId/deviceId/userId/locationId` is present or when the
  explicit-target count reaches 33 (this pass).
- No copy/bounds bug is currently named in the `:1443` JSON path. What is named is the
  exact pre-auth gate, the field that satisfies it, and the frame reader that has to be
  fed for the gate to be reached at all.


## 4. The frame prefix is six ASCII hex characters, not binary

The reader at `0x4a40b8` reads an 8-byte prefix (`lw v0,100(v0)` into `sp+2436`) and
validates it with three `sscanf` calls, not byte compares:

```
4a4136  lw   a1,0x66fb74        ; "%02x"
4a4138  addiu a0,sp,2436
4a413c  jal  sscanf
4a4142  cmpi v0,1               ; must parse exactly 1 field
4a4146  lw   v0,2456(sp)
4a414a  sltiu v0,7
4a414c  btnez fail              ; version must be >= 7
4a414e  addiu v0,sp,2436
4a4152  addiu a2,sp,2452
4a4156  lw   a1,0x66fb74        ; "%02x" at +2
4a4158  jal  sscanf
4a415e  ...  sltiu v1,7 ... bteqz fail   ; message type must be < 7
4a4194  ...  addiu a2,sp,2448
4a41a0  lw   a1,0x67414c        ; "%04x" at +4
4a41a2  jal  sscanf
```

So the prefix is `VVTTLLLL` in hex: protocol version (>= 7), message type (< 7), extended
header length. The extended header is then read (`could not recv extended header; expected
%u read %u`) and parsed as `name: value` lines, and the failure logs are
`Bad protocol version: %c%c`, `Bad message type: %c%c; %d`, `Error parsing extended header`.

Wire attempts with `0a010000`, `07010000`, `0a000000`, `0a020000` as a GET body, on `/api`
and `/websocket/api`, with and without `namespace:`/`x-sonos-namespace:`/`x-sonos-method:`
headers, all still answer 400 — so either the body is not where the reader looks, or the
400 is emitted before the reader runs. That is the open question for the next pass.

## 5. The 400 is synchronous and precedes any upgrade — the frame reader is post-101

Answer to the open question: **no upgrade completes before the 400.**

The route layer is a tree walker, not a flat table. `/api` is a *node* whose handler
(`0x47ab54`) builds a 40-byte route record (`0x4c3b48`) and calls the dispatcher
`0x4c43e0`. The dispatcher:

1. matches the prefix (`0x4c3b70`, `strncmp` + a `/`-or-end check) — failure emits **400**
   via `0x4c3b14` (`li a1,400` at `0x4c443e`);
2. resolves the version (`0x4c3c2c`): it wants `v<N>` in the remaining path and a matching
   `version=` in the `Accept` header, and `api-version` optionally — failure emits **400**
   (`0x4c4444`);
3. calls the handler object's vtable slot 4 (`0x47aa98`) then slot 6 (`0x47a9b0`), then
   re-enters the matcher (`0x4c41ac`) with whatever path is left; if the handler consumed
   nothing, `0x4c43f8` emits **400** again.

The handler object's vtable is `0x66a794` (slot 2 = next-segment `0x47a9c5`, slot 4 =
prepare `0x47aa98`, slot 6 = handle `0x47a9b0`). Slot 4 calls
`getBareProtocolVersionString` (`0x5f7a78`), which maps only **1 → "v1"** and **2 → "v2"**
(table at `0x6ac2ac`) and returns `""` for anything else — so the `lechmere.N` subprotocol
does not select the route version; the path does.

Wire evidence that this is all pre-upgrade:

- the 400 is returned in ~0.34 s with `Content-Type: text/plain`, `Content-Length: 0` and
  **no `Sec-WebSocket-Accept`**;
- a 200 KB request body does not change the timing (0.345 s vs 0.348 s), so the body is
  never read;
- `/api/v7` is the only versioned route that exists (`/api/v1`, `/api/v2`,
  `/api/v1/topology` all 404); `/api/v7`, `/api/v7/`, `/api/v7/topology`, `/api/topology`
  all 400;
- every `Sec-WebSocket-Protocol` value (`lechmere.0/1/2/7/10`) gives a byte-identical 400.

Consequence: the muse frame reader (`0x4a40b8`) cannot be what satisfies the gate. The
gate is reached only after the route layer stops 400ing, and the missing piece is the
*second* segment of the tree under `/api/v7` (the handler object carries 147 children at
`+4`/`+8`, i.e. the namespace/command table), not a header and not a frame.

## 6. `RAuthSplitToken` audit — clean on every malformed shape asked for

`RAuthSplitToken(out, value, n)` at `0x5f8e88` (caller `0x4c3d24` passes
`ap_request_get(req, "authorization")` and `n = 7168`):

```
5f8e9e  beqz a1, empty            ; NULL value
5f8ea0  lb v0,0(a1); bnez v0,parse ; empty value
5f8eb6  lb v0,0(s1); cmpi v0,32; btnez 5f8ec0; addiu s1,1; b 5f8eb6   ; skip spaces
5f8ec2  strncasecmp(s1, "bearer ", 7) -> type 1, a1 = s1+7
5f8ed4  strncasecmp(s1, "basic ", 6)  -> type 2, a1 = s1+6, else empty
5f8ee4  skip spaces from a1
5f8ef2  lbu a0,0(a2); li v0,33; neg; and; seb; beqz -> stop; addiu a2,1; b 5f8ef2
5f8f02  cmp a2,a1; bteqz empty
5f8f06  lw v0,80(sp)              ; n
5f8f08  subu a2,a1                ; token length
5f8f0a  sltu v0,a2; btnez empty   ; n < len -> empty
5f8f0e  std::string::assign
```

| shape | result |
|---|---|
| NULL / empty value | empty result, no deref |
| no scheme, or a scheme that is not `bearer `/`basic ` | empty result |
| `bearer` with no trailing space | `strncasecmp(...,7)` fails on the NUL, so it is not treated as a scheme |
| value exactly `bearer ` | `a1` lands on the NUL, the scan stops immediately, `cmp` equal → empty |
| embedded NUL | the scan stops at it; the token is truncated, never overrun |
| huge segment | bounded by `n` (7168) *before* the assign; over-long → empty |
| missing dots | irrelevant here — this function only strips the scheme, it never splits on `.` |

Both loops terminate on NUL (the space skip compares against 32, the token scan clears bit
5 and stops on `0x00` or `0x20`), the destination is a heap `std::string`, and the only
fixed number is a *rejection* bound. No copy/bounds bug.

## 7. RESOLVED: the gate is `X-Sonos-Api-Key` presence, and the JSON API is reachable pre-auth

Mechanical dump of the 147-entry child table at `0x66a7b4` (24-byte entries, count at
`+8`; entry = `{path, flags, reserved}`) gives the whole muse v1 surface, e.g.
`v1/players/{playerId}/info`, `.../settings/player`, `.../time/relative`,
`v1/households/{householdId}/devices/registrations`. Flags are a method bitmask in the low
byte: `0x01` GET, `0x02` POST, `0x04` PUT, `0x08` DELETE, `0x10` PATCH. So the second
segment under `/api` is `v1` and the third is `players|households|users|devices`, not a
namespace name — the earlier `/api/v7` shape was a dead end.

With the real path shape the 400 changes from an empty body to a JSON error, which names
the actual gate:

```
GET /api/v1/players/<UDN>/settings      -> 400 {"errorCode":"ERROR_API_KEY_VALIDATION_FAILED","reason":"Invalid api key"}
GET ... + X-Sonos-Api-Key: x            -> 400 {"errorCode":"ERROR_MISSING_PARAMETERS","reason":"Missing play…"}
GET ... with the real playerId + key    -> 401 {"errorCode":"ERROR_NOT_AUTHORIZED","wwwAuthenticate":"Bearer realm=\"service\"…"}
```

So: **`X-Sonos-Api-Key` is a presence check (any value passes)**; `Authorization: Bearer`
is not the key (`Bearer x` still gives API_KEY_VALIDATION_FAILED); and the per-route
Bearer requirement is a *separate*, later gate that only some routes apply.

Confirmed pre-auth 200s on `.86` with `X-Sonos-Api-Key: x` and no Bearer:

| path | status | body |
|---|---|---|
| `/api/v1/players/<UDN>/info` | **200** | 1765 B `discoveryInfo` (model, serial, capabilities, features) |
| `/api/v1/players/<UDN>/settings/player` | **200** | `playerSettings` (volumeMode, volumeScaling…) |
| `/api/v1/players/<UDN>/time/relative` | **200** | `relativeTimeStamp` |

401 `ERROR_NOT_AUTHORIZED` (Bearer needed): `.../settings`, `.../effectiveSettings`,
`.../update/status`, `.../networkTest/{id}`. 405 on `.../upnpDeviceProperties` (method).
`/api/v1/households` 404 (no such entry; it is `v1/households/{householdId}/households`).

Consequence for the bead: the `:1443` JSON binder is reachable pre-auth on a real route
set, so the copy/bounds hunt can now be driven against a live parser instead of guessed
paths. The `authorization` binding and `hasAuthToken` are on the *Bearer* path, not the
entry gate.
