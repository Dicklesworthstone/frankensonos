# RE_NOTIFY — POST/NOTIFY /notify body handling

Lane `frankensonos-re-notify-shapes-qgv`. Live, 2026-10-23, on `.86` (S2 86.10-81100, Play:1) and
`.202` (S1 57.23-74170, Play:5).

## Wire shape (this was wrong before)

`POST /notify` is **405** on both `:1400` and `:1443`. The verb is **NOTIFY**, and GENA headers are
required: `NOTIFY /notify` with `NT: upnp:event`, `NTS: upnp:propchange`, `SID: <any>`, `SEQ: <n>`
answers **200 with an empty body**. Without `NT` it is **400**. `PUT` is 405.

`200` is *accepted*, not *parsed*: a 1-byte body also gets 200.

## The body reader is a single read

The endpoint reads the body **once**, answers, and closes. It never waits for `Content-Length`:

- a 200 ms gap between the header block and the body is fine (single write);
- a 1-byte first write followed by the rest is answered 200 immediately and the rest is lost;
- writing past the first read raises `EPIPE` on the client, and on S1 the still-unread bytes turn the
  close into an **RST that destroys the response** (`ConnectionResetError`).

Consequence: fragmenting a body is only safe if the whole body fits in the first read.

### First-read ceiling, single write, 4 runs each (status)

| body bytes | 256 | 512 | 1024 | 1536 | 2048 | 3072 | 4096 |
|---|---|---|---|---|---|---|---|
| S2 `.86`  | 200×4 | 200×4 | 200×4 | 200×4 | 200×4 | 200×4 | 200×4 |
| S1 `.202` | 200×4 | 200×3 + RST | 200×4 | RST×4 | RST×4 | RST×3 + 200 | RST×3 + 200 |

S2 still answers 200 at 16 KB and 64 KB single-write (3/3 each) — its read is at least 64 KB or it
loops until the close. S1's ceiling sits between 1 KB and 1.5 KB.

### Fragment × gap matrix (fragmented writes, 10 s reply timeout)

`write=ok` with no reply = the RST ate it. Nothing killed the daemon in either run.

| frag | S2 0 ms | S2 5 ms | S2 50 ms | S1 0 ms | S1 5 ms | S1 50 ms |
|---|---|---|---|---|---|---|
| 1 | 200 | EPIPE | EPIPE | 200 | EPIPE | EPIPE |
| 7 | 200 | EPIPE | EPIPE | 200 | EPIPE | EPIPE |
| 64 | 200 | EPIPE | EPIPE | 200 | EPIPE | EPIPE |
| 256 | 200 | 200 | EPIPE | 200 | EPIPE | EPIPE |
| 1460 | 200 | 200 | 200 | 200 | 200 | EPIPE |
| 4096 | 200 | 200 | 200 | 200 | 200 | EPIPE |

So the failure is purely "more than one segment", not time: 50 ms of gap at 1460 bytes breaks S2,
while 0 ms at 256 bytes does not.

Reproduce: `captures/research/notify_frag_matrix.py <host>` (defaults `:1400`, plain, `NOTIFY`).

## Where the body goes (static, 57.23 PPC)

The endpoint blob is at `0x1043db74` (`/notify\0/xml/device_description.xml\0…`) inside the 57.23
`anacapad` (base `0x10000000`). Prior note stands: the LastChange parser sits behind
`g_pZone vtable+0x2c`, so the parser itself is reached through the zone object, not through the
`/notify` handler. Not re-walked in this lane.

## /getrs (S1 only; 404 on S2)

`/getrs` answers **200 with `Content-Length: 0`** and dummy content headers
(`X-RINCON-CONTENT-VERSION: 4294967295`, `X-RINCON-LAST-UPDATE-DEVICE: RINCON_FFFFFFFFFFFF99999`)
for every shape tried: bare, `?id=` over `-1, 0..10, 100, 1000, 10000, 999999`, POST body
`id=1`, and 40 further parameter names (`index idx i n s k name setting key val value var param q a
which item num no seq prop property sid did cmd …`). Nothing ever returns content, so the
parameter-name guess is still open. Static anchor for the next read: a 16-byte descriptor table at
`0x10001440` (`{u32 blob/offset, void *handler, u32 size, u32 tag}`; `tag 0x1200000b` entries point
into the `_savegpr_27…31` thunk area at `0x1043c9cc`+, `tag 0x12000000` entries point at runtime-filled
`.bss` name slots around `0x105460b0`). `/getrs` is not referenced by a literal pointer and is not
reached by a `lis`/`addi` pair in the whole 1.1 M-line disassembly, so it is looked up by hash, not
strcmp — which also explains why parameter-name brute force fails.
