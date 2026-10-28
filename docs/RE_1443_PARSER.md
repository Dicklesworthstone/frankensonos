# RE_1443_PARSER — lane log (:1443 request/JSON path, pre-auth)

Lane SapphireIvy (bead `frankensonos-jc7`). Target: a copy/bounds bug reachable **before** the
Bearer check in the :1443 API. Binary: `captures/firmware/s2_overlay/bin/anacapad` (86.10,
MIPS16; Ghidra project `captures/research/ghidra`). Read-only, no hardware.

## The gate this lane sits in front of

`0x4aa99c` is the API-key check: `memset(sp+24,0,32)`, `strlen(input)`, `ZZ0058(1, input, len,
sp+24)` (SHA-256), then `memcmp(sp+24, 0x676ea8, 32)` and `memcmp(sp+24, 0x676e88, 32)`, return
1 on either match, 0 otherwise. The two 32-byte constants are the ones DustyBison is inverting
(`frankensonos-c08`). Everything below runs before that check.

## Request-body path (read so far)

```
0x4ece50  body-read driver
  4ece64  lbu a0,32(this)            ; mode flag
  4ece72  vtable -28096(gp)          ; shouldRewriteBody()
  4ece86  vtable -28084(gp)          ; readFromNetwork(this, buf, len, timeout)
  4ece94  used = *(this+0x18) - *(this+0x14)     ; vector end - begin
  4ece9c  wanted = *(this+8)
  4ecea0  sltu used, wanted ; btnez -> error path when used < wanted
```

`MuseRESTStream::readFromNetwork` (`0x4ecd28`, decompiles cleanly):

```c
if (shouldRewriteBody() == 0)
    vtable[+8](this->sock, this->buf, param_1, param_2, param_3 * 1000);   // no resize
else {
    if (*(this+8) == 0) { resize(this+0x14, param_2 << 1); vtable[+8](..., param_1, param_2, 0); }
    else                { resize(this+0x14, *(this+8) << 1); readContentLengthBytes(this, param_3); }
    resize(this+0x14, ret);
}
```

`readContentLengthBytes` (`0x4ec1f0`) polls with `n * 1000` ms and loops `read()` into
`*(this+4) + *(this+20) + done` while decrementing the remaining count — the loop itself is
bounded by the caller's length.

## Candidate bug shape (not yet proven)

The `shouldRewriteBody() == 0` branch reads `param_2` bytes into `param_1` **without** resizing
anything, so the bound is whatever the caller guarantees. The open question is exactly where
`param_1`/`param_2` come from and whether `param_2` is the raw `Content-Length` while `param_1`
is a fixed buffer. The body-read driver `0x4ece50` is reached through a vtable, so the caller
has not been located yet.

## Next reads

1. Find the vtable slot that reaches `0x4ece50` and its caller; trace `param_1`/`param_2`.
2. Check whether `Content-Length` is capped before the read (search the header parse for a
   limit constant) and whether a negative/oversized value reaches `resize`.
3. Then the JSON body parser itself (the object/sajson-style consumer) for its own bounds.

## JSON reader: the length comes from `strlen`, not from Content-Length

The body is parsed by rapidjson behind a thin wrapper: `sonos::json::Parser::parse(parser, text,
strlen(text))` at `0x49f2b8` (call site `0x49f34c`). The length handed to the parser is therefore
a **NUL scan of the buffer**, not the `Content-Length` that was read.

The file-backed caller is safe: `FUN_0049f670` reads at most `0x7ff` into a 2048-byte stack
buffer (`sonosFileReadWhole`, `0x49f68c`), then writes `buf[n] = 0` at `0x49f6b4` before parsing.

The network path has no such store. `0x4ece50` reads the body through
`readFromNetwork` (`0x4ecd28`) and afterwards only compares `used = *(this+0x18) - *(this+0x14)`
against `wanted = *(this+8)` (`0x4ece94`-`0x4ecea2`); the tail of the function (`0x4eceb8`-
`0x4ecefe`) issues more vtable calls and never writes a terminator.

**Candidate bug: `strlen()` over a body that is not NUL-terminated**, i.e. the length field is
consumed before any validation of the terminator. If the body buffer is exactly
`Content-Length` bytes and the following byte is not zero, `strlen` runs past the body and the
parser is handed a length longer than the allocation. To confirm: find the vtable slot that
reaches `0x4ece50`, read where the destination buffer is allocated and whether anything
terminates it, then send a body whose last byte is non-zero and whose allocation is tight.

## The string sink chain (named)

A JSON string value reaches a copy through this fixed chain:

```
rapidjson ReaderHandler::String (0x5f4f90)
  -> sonos::json::bindString (0x5f1428)          ; args: (iface, key, type, char *text, uint len)
  -> FUN_005f13c0 (0x5f13c0)                     ; builds a descriptor, calls iface->vtable[1]
  -> RJsonDataBinding vtable 0x6ab9c8 slot 1 = 0x5f16e4
  -> FUN_0051b9b4(dest, 0x240000)                ; the copy
```

`bindString`'s signature is `(RJsonDataBindingInterface&, unsigned int, unsigned char, char*,
unsigned int)` — the last argument is the JSON string length, i.e. **attacker-controlled and
unvalidated at this point**; the destination comes from the binding table, not from the packet.
The constant `0x240000` is a descriptor word (high byte 0x24), not a length.

`0x51b9b4` is the same body the token lane hit as the "real getter"; it is a jump target with no
prologue of its own and the linear MIPS16 decode from it is misaligned, so its internal bound
check (if any) is the remaining read. The next step is a correct entry point for `0x51b9b4` and
then the destination sizes in the binding tables that feed `bindString`.

## Sink status: binder slot reached, copy primitive still unnamed

Confirmed by the vtable dump: `RJsonDataBinding` vtable `0x6ab9c8`, slot 1 = `0x5f16e4`, and
`0x5f16e4` is a thunk `FUN_0051b9b4(param_1, 0x240000)`.

`0x51b9b4` is **not** a function: it is an unnamed 0x20-byte PIC stub (MIPS32, immediately after
`__cxa_guard_release` ends at `0x51b9b4`) of the form `lui t9,0x52; addiu t9,t9,-17988;
lui gp,0x1e; addiu gp,gp,16804; addu gp,gp,t9; lw t9,-32724(gp); b 0x51bb04` (the lazy
resolver). So the copy primitive is whatever the GOT slot holds, and that slot is **not**
resolvable from the file: the computed address `0x6f7b8c` has no relocation and lies past EOF
(NOBITS), and neither `.rel.plt` nor `.rel.dyn` covers it.

That is the honest stop for this read: the chain is JSON string -> `bindString` -> binder slot 1
-> stub -> GOT. Naming the primitive needs the GOT slot resolved (run-time or by a correct gp),
and only then can the destination sizes in the binding tables be compared against the
attacker-controlled length.

## The 1112-byte sink (PPC 57.23 build): constructor, singleton, and what it does

`FUN_1021fb54` (store at `0x1021fb60`) sets descriptor `+0xc = 0x14f8`, memsets the buffer at
`+8`, publishes it as the singleton `DAT_10545d78`, and installs two function pointers:
`FUN_1021f8f8` at descriptor `+0x10` and **`FUN_10219cf8` at `+0x14`** — the sink. It also seeds
the record (`*singleton = param_1`, `[1] = singleton`, `[2] = &DAT_1054dae8`, `[0x501] = 10`).

`FUN_10219cf8` is a **command dispatcher on `*param_2`** (ids `0x14`, `0x1e`, `0x4d`, …), writing
into the singleton. Its copies are all bounded: `strlcpy(..., 0x80)` at several sites and a fixed
`memcpy(singleton+0x140d, __s2, 0x21)` at `0x1021a1xx`. So the sink is not itself a length bug.

Reachability: the constructor is referenced only from a data table at `0x1047f92c` (init
registration) and the sink only from the constructor, so the caller is whoever owns the
descriptor — not yet located. That is the next read, and it decides whether any `:1400` request
reaches the dispatcher at all.

## Unblocked: the primitive is `sonos::json::bindStdString`

The "unresolvable GOT slot" was a red herring — the slot holds the lazy PLT0 address and the resolver
takes its index from the stack, so the *symbol* is recoverable from the ELF's own `.dynsym`, which is
intact. Decoding `0x5f16e4` as MIPS16 (the file is big-endian MIPS; `readelf` needs `-B` and the
section headers are stripped, so parse `PT_DYNAMIC` by hand) lands inside a named function:

```
_ZN5sonos4json13bindStdStringER25RJsonDataBindingInterfacejhPNSt7__cxx1112basic_stringIcSt11char_traitsIcESaIcEEEj
= sonos::json::bindStdString(RJsonDataBindingInterface&, unsigned, unsigned char, std::string*, unsigned)
```

So the destination is a `std::string` and the fifth argument is a length cap, not a fixed buffer. The
overflow, if any, is therefore in a caller that copies the bound string into fixed storage, or in the
cap being attacker-controlled. Next: the callers of `bindStdString` and what they do with the result.

**Correction to the mode.** `0x5f16e4` is MIPS16 and calls `jalx 0x51b9b4` with `a1 = 36`, so
`0x51b9b4` is MIPS32 and the earlier linear MIPS16 decode from it was misaligned by construction.
The stub's `lw t9,-32724(gp)` targets `0x6f7b8c`, which is in `.got` (not `.got.plt`: the PLT relocs
start at `0x6f6008`), and no `DT_REL`/`DT_JMPREL` entry covers it, so the pointer is installed at
runtime by an initializer. `bindStdString` itself is at `0x5f1621` (36 bytes) and
`bindStdStringArrayAndSize` at `0x5f15e9`.

## bindStdString / bindStdStringArrayAndSize: the "cap" is the attacker length truncated to u16

Both are thin wrappers over the generic binder and are referenced **only** from the type table at
`0x6f8ba8` (bindStdString) and `0x6f8bac` (bindStdStringArrayAndSize), i.e. their caller is
`FUN_005f13c0`'s type dispatch, not a per-field site:

- `bindStdString` `0x5f1620` → `FUN_005f13c0(5, iface, key, type, string*, len, …)`
- `bindStdStringArrayAndSize` `0x5f15e8` → `FUN_005f158c(iface, count, 0, out, cap)` then
  `FUN_005f13c0(5, iface, key, 0, string*, len, cap, …)`

Neither wrapper clamps anything. The length reaches `FUN_005f1890`, which stores it as a
**halfword**: `0x5f189a  sh a3,0x18(a0)`. `FUN_005f13c0`'s parameter is declared `undefined2` for
the same reason.

So the "cap" is not a cap: it is the attacker's JSON string length **narrowed to 16 bits**. A
string of 65 536 + n bytes is passed on as n. Whether that overflows depends only on the
destination size in the interface method (the unresolved `0x51b9b4` stub), so a fixed destination
smaller than the wrapped value is the bug shape.

**Verified independently.** `0x5f189a` is `sh a3,24(a0)` — a halfword store, so the u16 wrap is real and
not a Ghidra type artefact. The consumer reads the field back through the unresolved GOT pointer, so
whether the *copy* uses the truncated value or the original is still the open question; if it copies
the original into a buffer sized from the truncated field, any JSON string over 64 KiB is an overflow.

## Resolved: the copy uses the truncated u16, because the descriptor has no other length

Full store list of `FUN_005f1890` (the only builder of this descriptor), from `0x5f1890`:

```
0x5f1892 sw a2,0x4(a0)    0x5f1894 sw a2,0x8(a0)    0x5f189a sh a3,0x18(a0)   <- the length
0x5f18a4 sw s0,0x0(a0)    0x5f18a6 sw a2,0xc(a0)    0x5f18b0 sw a1,0x1c(a0)
0x5f18b2 sw a2,0x10(a0)   0x5f18aa/0x5f18ac sb v0,0x1a/0x1b(a0)
0x5f18b4 sb v1,0x20(a0)   0x5f18b8 sb v0,0x21(a0)
```

There is **no 32-bit length field**: the length exists only as the halfword at `+0x18`. Whatever
the interface method does, it can only read the truncated value, so a JSON string of `65536 + n`
bytes is copied as `n`.

That settles the :1443 question in the negative for "the original length is used": the bug is a
16-bit wrap of an attacker-controlled length, and it becomes an overflow exactly when the
destination is a fixed buffer smaller than the wrapped value. Readers of `+0x18` exist (e.g.
`0x48068a lhu a1,24(a2)`); the remaining work is matching one to a fixed destination.

## The port is reachable from the LAN (TLS)

`:1443` is open on both lines from an ordinary LAN host, and it speaks **HTTPS**: a plain `http://`
request is reset, `https://` answers. `/` and `/api` are 403/400 without credentials, but `/status`
answers 200 with a diagnostics index (`enetports`, `wireless`, `zp`, `VERSION`, `proc/ath_rincon/status`,
`ifconfig`, `showstp`), and `/api/<anything>` consistently answers **400** with an empty body.

So the "local" REST surface is not local at all — it is a second, richer attack surface than `:1400`,
and it is the one that carries the `bindStdString` u16 truncation. The 400 is a parameter check, not
auth: the string `Invalid request parameters` is referenced once, from the data table at `0x1f8080`, so
the handler validates a shape before anything else. Next: recover that shape (the `namespace`/`interval`/
`delayMs` strings sit together at `0x282c88`, next to `MuseChaos`, which suggests a chaos-injection
control endpoint) and then drive the JSON binder with an over-long string.

## /api request shape (bb-p75)

`:1443` is TLS-only and `/api/*` answers a bare 400 until the shape is right. The shape is in the
string block at file offset `0x282c00` (vaddr `0x682c00`):

```
chaos: delaying request for %s#%s by %d ms | chaos: dropping request for %s#%s | MuseChaos
<html><body>set: namespace=%s, command=%s, interval=%d, delayMs=%d</body></html>
<html><body>Invalid request parameters</body></html>
namespace | interval | delayMs
```

So the fault-injection endpoint is **MuseChaos** and takes `namespace`, `command`, `interval`,
`delayMs`; success echoes them, failure is the "Invalid request parameters" body. The generic
command-header binding table (`FUN_006084a4`, `0x6084a4`) registers the field names in order —
`command, cmdId, namespace, groupId, sessionId, playerId, deviceId, userId, locationId,
serviceId, householdId, corrId, authorization, intendedTargets, upnpAnacapaPort,
upnpTimeoutMillis` — each via `FUN_005f53f4(obj, name, 5, index)`, i.e. type 5, which is the
`bindStdString` family whose length is the truncated u16.

Next: drive `bindStdString` with a >64 KiB string on one of those fields and watch for the
16-bit wrap (the destination sizes are the unresolved part).

## /api/chaos: route confirmed, parameter transport still open

The route name table at `0x6764a8` (packed strings) holds `chaos`, `setrecoveryurl`,
`legacy_anacapad` and `forceupdate` (with `/var/run/forceupdate`); `/api/` is a prefix applied to
each registered name by `FUN_004e440c` (`0x4e440c`, `PTR_s__api__004e4498`). The chaos handler
class is `RMuseChaosEntry` (`getChaosAction` at `0x4e6fa0`).

Live on `.86`, every shape tried still answers **400 with an empty body**:
`GET /api/chaos?namespace=x&command=y&interval=1&delayMs=0`,
`GET /api/chaos` bare, `POST /api/chaos` with a JSON body of the same four fields, on `:1443`
(TLS) and `:1400`. So the route is right and the handler rejects before the chaos action runs —
the parameter transport (query vs body vs header) or a required extra field is still unknown.
`getChaosAction` itself only decides delay/drop from the entry state; the parse is upstream.

## The `/api` route is a muse websocket, not a REST endpoint

`/api` and `/websocket/api` are both matched routes (a nonsense path 404s, these 400), and the firmware
carries the handshake it wants:

```
Sec-WebSocket-Protocol: lechmere.%u
lechmere.%hhu%n
```

`lechmere` is the muse websocket subprotocol. Sending `Sec-WebSocket-Protocol: lechmere.1` (and 2..10),
with the upgrade headers, on either route, still answers 400 with an empty body — so the remaining
condition is something else in the handshake (the api-key header alone does not satisfy it either).
The JSON binder sits behind that handshake, which is why nothing JSON-shaped ever reached it.

Tried and rejected (all 400): subprotocol versions 0-16 and 20/32, role-shaped subprotocols
(`lechmere.controller|player|admin|app`), a literal `lechmere:` / `X-Lechmere:` header, the
`X-Sonos-Api-Key` + `X-Sonos-Corr-Id` pair, a `Host: lechmere.<id>.ws.sonos.com`, and query variants.
The handler does work before rejecting (the 400 arrives after a delay, not immediately), so it is reading
something and failing — `failed to read lechmere header: %d` is the message to chase, from
`RMuseController::prepareWebsocketHeader` / `captureWebsocketCommandToken`.

## The websocket handshake gate (bb-p75)

`RMuseController::prepareWebsocketHeader` (`0x4e8d7c`) is a three-step chain, each step must
return 0: `updateHeaderTargets(this,1)` → `captureWebsocketCommandToken(this)` →
`activateNamespaceFromHeader(this,1,…)`.

`captureWebsocketCommandToken` (`0x4e7d44`) opens with the hard gate:

```
if (*(int *)(this + 0x2868) != 0x34 || this[0x24f4] == 0) goto fail;
```

`0x34` (52) is a state constant written by `li v0,52; sw v0,10344(s0)` at `0x4e854e`/`0x4e855a`.
Past it the connection type is tested with `isConnectionType(conn,0,2)` — type **2 is an error**
(state `0x4e`), types 1 and 0 proceed to `RAuthSplitToken(this+0x1bb, 0x2000)` on the
`Authorization` header.

The wire vocabulary in the same string block (`0x674000`-ish): `lechmere.%hhu%n`,
`x-sonos-method: `, `x-sonos-uri: `, `SOAPACTION: `, `Bad protocol version: %c%c`,
`Bad message type: %c%c; %d`, `Bad extended header length: %c%c`,
`could not recv extended header; expected %u read %u`, `Sec-WebSocket-Protocol:|%02u`,
`protocol version negotiation failed due to unknown version (%s)`. So after the upgrade the
server reads a **binary muse frame** (message type + extended-header length + protocol version),
not text.

Live on `.86` (now reporting `86.10-81100`): a full standard upgrade (`Connection: Upgrade`,
`Upgrade: websocket`, `Sec-WebSocket-Version: 13`, `Sec-WebSocket-Key`, `Sec-WebSocket-Protocol:
lechmere.1`) still answers **400** on both `/api` and `/websocket/api`, with and without
`x-sonos-method`/`x-sonos-uri`. The gate that fails is therefore before or inside
`updateHeaderTargets`, not the protocol header.

### The frame format is in the binary too

Next to the handshake strings:

```
X-Sonos-Udn
Sec-WebSocket-Protocol: lechmere.%u\r\n%s: %s\r\n
x-sonos-method: \0 x-sonos-uri: \0 SOAPACTION: \0
Bad protocol version: %c%c
Bad message type: %c%c; %d
Bad extended header length: %c%c
could not recv extended header; expected %u read %u
```

So a frame is a two-character protocol version, a two-character message type, an extended-header
length and then `x-sonos-method` / `x-sonos-uri` / `SOAPACTION` headers — a binary envelope, not JSON.
The handshake itself sends the subprotocol plus one extra `<name>: <value>` header, and `X-Sonos-Udn`
is the obvious candidate.

A real client (`websockets` 17.3) reproduces the 400 on both `/api` and `/websocket/api`, with and
without `X-Sonos-Udn` and with `lechmere.1`/`.2`, so the missing condition is something else in
`prepareWebsocketHeader` — worth reading properly rather than guessing further.

## Corrected decode of the gate (true entry 0x4e7d44)

The symbol address is odd (`0x4e7d45`); the true entry is `0x4e7d44`, and the first four
instructions are PIC setup, so a decode from the symbol is off by one. Correct bytes:

```
4e7d44  li v0,33          4e7d48  la v1,4efb60     4e7d4c  sll v0,16      4e7d50  addu v0,v1
4e7d52  save 136,ra,s0-s1 4e7d56  move gp,v0        4e7d58  sw v0,16(sp)   4e7d5a  move s1,a0
4e7d5c  lw v0,10344(a0)   4e7d60  cmpi v0,52        4e7d62  btnez 4e7e76   <- fail
4e7d66  lbu v0,9460(a0)   4e7d6a  beqz v0,4e7e76    <- fail
```

So the exact condition is: **state `+0x2868` must equal 52 and the byte at `+0x24f4` must be
non-zero**, both checked before anything else.

`+0x24f4` is not attacker-supplied: it is written in `RMuseController::updateHeaderTargets`
(`0x4e8c60  sb v0,9460(s1)`) from the **return value of a header lookup** whose destination is
`this+0x2518`, and it is cleared wholesale at `0x47aeee` (a run of `sb v0` over `+0x24f0…`).
`updateHeaderTargets` then re-checks `state == 52` and `*(this+0x24d8) < 33` at
`0x4e8c64`-`0x4e8c74`.

So the 400 is `updateHeaderTargets` refusing before the protocol header is ever looked at: the
missing piece is whatever makes that lookup succeed, not `lechmere`.
### Correction (2026-10-28, pane omp_2): the gate is `state == 52`; `hasAuthToken` only gates token capture

`updateHeaderTargets` returns 16 when the state is not 52 (`0x4e8d34`-`0x4e8d3e`), and
`captureWebsocketCommandToken` jumps to `0x4e7e76` on either a bad state or a zero
`+0x24f4` — where a state of 52 returns **0**. So `hasAuthToken() == 0` is not a refusal,
it only skips the token capture. The refusal is the state.

The state is produced by `RMuseHeaderHandler::checkIfValid` (`0x4e8318`): `out[0] = 52`,
then `out[0] = 72` if any of the bindings at `+0xc4/+0xe8/+0x10c/+0x130/+0x154/+0x19c` is
present. The handler's binding names are a literal table at `0x600c24` and bindings are 36
bytes apart from a base of 16, so those six are `householdId`, `sessionId`, `playerId`,
`deviceId`, `userId`, `locationId` — they must be **absent**. The table also puts
`authorization` at `+0x1e4` (index 13), which is exactly the field `hasAuthToken` tests;
its value is split by `RAuthSplitToken` (`0x5f8e88`) on `bearer `/`basic `.

The names are lowercase and match the muse frame header vocabulary, not HTTP header casing.
Read-only wire checks on `.86` (86.10-81100) all answer 400 with an empty body: no extra
header, `Authorization: Bearer x`, `authorization: Bearer x`, the whole outgoing header set,
`namespace: topology|zoneGroupTopology|household` + `apiVersion: 1`, and a pipelined 6-byte
frame as a GET body. `Host: lechmere.1.ws.sonos.com` also 400s but from the *static* handler
(`text/html` + CSP). So the block the bindings read is the frame, and the next read is the
frame validator at `0x4a40b8` (8-byte prefix read, then version/type checks returning
8/9/10). Details: `recon/jc7/findings2.md`.

## The muse frame reader needs a plain-TCP muse channel, not the WS JSON channel (2026-10-28, orchestrator synthesis)

Synthesis of omp_2's findings2.md with `docs/RE_1443_PARSER.md` §"The websocket handshake gate":

- `prepareWebsocketHeader`'s chain (`updateHeaderTargets` → `captureWebsocketCommandToken` →
  `activateNamespaceFromHeader`) gates on **`state == 52`** (a `checkIfValid`-produced state,
  i.e. *no* explicit-target binding present) — the frame vocabulary the state machine consumes
  (`x-sonos-method:`, `x-sonos-uri:`, `SOAPACTION:`, lowercase binding names) is **muse-frame
  extended-header material, not HTTP headers**.
- The frame validator `0x4a40b8` reads an **8-byte ASCII-hex prefix** `VVTTLLLL`
  (version ≥ 7 via `%02x` + `sltiu 7`; type < 7; `%04x` extended-header length), then receives
  the extended header and parses `name: value` lines. Wire attempts that put the frame in a GET
  body all 400 — the reader is not fed from an HTTP body.
- The 8-byte read goes through a vtable slot (`lw v0,100(v0); jalr`) on the *stream* object
  (`this+0x10`, `RMuseController` ctor arg a1, `RReadWriteStream&`). The muse frame channel is
  the **websocket payload stream after the 101 upgrade** — the JSON `[header,payload]` frames the
  static-key channel speaks are one layer *above* it, and the raw muse framing is what a
  `lechmere.N`-subprotocol control connection carries.

Next concrete experiment (whoever picks this up): complete a `Sec-WebSocket-Protocol: lechmere.9`
upgrade on :1443 (the `lechmere.%hhu%n` scanner caps the version at `%hhu`; try 7..13), then send
`0a010000`-style 8-byte ASCII-hex prefixes **as websocket binary frames** (opcode 0x2, masked),
followed by `LLLL` bytes of extended header (`namespace: topology\n`) — read the failure codes
8/9/10 back from the connection (the validator's return codes) instead of the flat 400. The
handshake-then-frame path has never been tried; every prior attempt stuffed the frame into HTTP.

Harness: `captures/research/ws-hunt/ws.py` already does the static-key WSS dance — add a
subprotocol parameter and a raw-binary-frame sender, keep the JSON layer out.

### The 400 is synchronous and pre-upgrade (2026-10-28, pane omp_2)

The route layer is a tree walker, not a flat table. `/api` is a node whose handler
(`0x47ab54`) builds a route record and calls the dispatcher `0x4c43e0`, which emits **400**
at three points: prefix mismatch (`0x4c443e`), version resolve failure (`0x4c4444`), and a
handler that consumes none of the remaining path (`0x4c43f8`). The handler object's vtable
is `0x66a794` (slot 2 next-segment `0x47a9c5`, slot 4 prepare `0x47aa98`, slot 6 handle
`0x47a9b0`); slot 4 maps only protocol versions 1 and 2 (`0x5f7a78`, table `0x6ac2ac`), so
the *path* selects the version, not `lechmere.N`.

The 400 returns in constant time with `Content-Type: text/plain`, `Content-Length: 0` and
**no `Sec-WebSocket-Accept`**; a 200 KB body does not change the timing, so the body is
never read. `/api/v7` is the only versioned route that exists (`/api/v1`, `/api/v2` 404) and
every subprotocol value gives a byte-identical 400. **No upgrade completes before the 400,
so the muse frame reader (`0x4a40b8`) is strictly post-101** and cannot be what satisfies
the gate. The missing piece is the second tree segment under `/api/v7` (the handler object
carries 147 children at `+4`/`+8`).

`RAuthSplitToken` (`0x5f8e88`) is clean on NULL, empty, missing scheme, scheme without a
space, exactly `bearer `, embedded NUL and huge segments: both loops terminate on NUL, the
only fixed number is a rejection bound (`n = 7168`) checked before the assign, and the
destination is a heap `std::string`. Details: `recon/jc7/findings2.md` §5-6.
