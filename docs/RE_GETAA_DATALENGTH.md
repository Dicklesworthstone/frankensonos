# RE_GETAA_DATALENGTH — the READ reply `DataLength` is trusted past the requested size

Lane FoggyBirch, bead `frankensonos-f4k`. Device `.86` (S2 86.10-81100, Play:1), live, unauthenticated
`GET /getaa?u=x-file-cifs://…`. No hardware interaction beyond HTTP; players left muted.

## Result

The SMB2 READ reply's **`DataLength`** (u32 at body offset 4) is the only field that breaks the client.
`DataOffset` (u8 at body offset 2) is separately required to be exactly `0x50`.

| reply mutation | getaa | effect |
|---|---|---|
| control (server's own `DataLength` = 4068, `DataOffset` = 0x50) | **200, 4068 B JPEG** | — |
| `DataLength` = 0x4000 (exactly the client's requested read size) | **200, 4068 B** | accepted |
| `DataLength` = 0x4001 … 0xffffffff | **never 200** | 404, or a **9.4 s hang** with three full retries |
| `DataLength` = 0, 1 | 404 | payload shorter than the file |
| `DataOffset` = anything but 0x50 (0x00, 0x01, 0x40, 0x4f, 0x51, 0x60, 0x7f, 0x80, 0xff) | 404 | PDU rejected before the payload is used |

The hang is not a crash: `/status/zp` answers 200 throughout and after, and the daemon needs no recovery.
It is a per-request worker pin — one unauthenticated GET costs ~9.4 s of a worker thread and three SMB
round trips. Eight concurrent requests each held a worker for 9.4 s.

## Why (static)

`libsmb2.so.1`, shipped build, `libsmb2.so.4.0.0` (string at file `0x301c8`), sha256
`ba823cc9…c0805a`. The S2 copy is MIPS and not in the extracted overlay; this is the same 4.0.0 source
(PPC build from the S1 image), and file offsets equal vaddrs in `.text`.

`smb2_process_read_fixed` at file **`0x1f854`**:

- `0x1f8dc` `lhz 9,26(1)` / `0x1f8e0` `cmplwi 9,17` — the fixed body's `StructureSize` is checked
  (`> 17` rejected). This is the only length check in the function.
- `0x1f8f8`–`0x1f904` — `DataOffset` (u8, body+2) and `DataLength` (u32, body+4) are read into the
  per-PDU read struct; `DataRemaining` (u32, body+8) is read at `0x1f908` and never used.
- `0x1f918` `lwz 9,4(29)` / `0x1f920` `cmpwi 9,0` — `DataLength == 0` returns success with no payload.
- `0x1f928` `lbz 6,0(29)` / `0x1f930` `cmplwi 6,80` / `0x1f934` `bf 0x1f998` — `DataOffset != 0x50`
  returns `-1` (the caller retries the whole connection).
- `0x1f92c` `mr 3,9` — **`DataLength` is returned as the payload length with no comparison against the
  requested count.**

The caller sizes the receive buffer from the request, not the reply: `smb2_cmd_read_async` at
`0x1f70c`–`0x1f720` loads the requested count (`lwz 6,4(31)`) and the caller's buffer (`lwz 5,16(31)`)
and passes them to `smb2_add_iovector` (`0x10140`), which only records the pair and accumulates the
total at `+4`. The socket read is a bare `readv` on that iovec (`0x23484`–`0x23490`). Nothing reallocates
when the reply advertises more than was asked for, so the surplus bytes stay in the socket and the next
parse starts mid-payload.

## Reproducer

```bash
# terminal 1 — pad the READ payload to 0x8000 and advertise it
~/sonos-root-local/venv/bin/python ~/sonos-root-local/recon/smbserv-final.py 0x50 0x8000 2611

# terminal 2
curl -m 15 -o /dev/null -w '%{http_code} %{time_total}\n' \
  'http://192.168.4.86:1400/getaa?u=x-file-cifs%3A%2F%2F192.168.4.170%3A2611%2FSHARE%2Freal.jpg&w=200&h=200'
# -> 404 after ~9.4 s, three PATCH lines in the server log (three retries); /status/zp stays 200
```

Matrix runner: `~/sonos-root-local/recon/getaa_matrix.py <ip> [OFF:LEN …]` (fresh port and server per
case, liveness before/after, saves under `recon/getaa-saves/`).

## Limits

- The 404-vs-hang split inside `0x4001…0x7fff` is not deterministic; both outcomes are the same defect
  and neither ever returns the image.
- `DataRemaining` is parsed and dropped, so it cannot be used to signal the surplus.
- The S2 art handler that allocates the 0x4000 buffer is in the encrypted base rootfs, not in the
  overlay; the request size is fixed at 0x4000 on the wire (server log `READ len=16384`).

## The `u=` filename is only an existence probe; content is a fixed candidate list

Server-side SMB log for one request (`u=…/SHARE/real.jpg`):

```
smb2Create: real.jpg      <- opened and closed, never read
smb2Create: folder.jpg    <- first candidate
smb2QueryInfo / smb2Read / smb2Close folder.jpg
```

With `folder.jpg` absent the client tries, in order, `folder.jpg`, `Folder.jpg`, `folder.gif`,
`Folder.gif`, and reads the first that exists (`folder.gif` alone returns 200). With `folder.jpg`
absent and only `real.jpg` present the request is a 404: the URL's file component is never read.
A nonexistent name makes the client open `.` and give up.

Consequences:

- The bytes are a **pure proxy**: no decode, no validation, no resize. Empty file, 2-byte SOI,
  truncated JPEG, 4 KiB of random bytes, a PNG and a JPEG with `SOF0` dimensions `0xffff×0xffff`
  all return 200 with the exact byte count. `w=`/`h=` are ignored (566×424 stays 566×424 at
  `w=100&h=100`). The JPEG decode happens on the controller, not the player.
- Multi-chunk reads are fine: 16 384, 16 385, 32 768 and 200 000-byte files all return intact
  (the client loops READ with the honest `DataLength`), so the defect above is specific to a
  reply that advertises more than it was asked for.

## S1 cross-check (57.23, `.202`)

The S1 daemon also carries `/getaa` and `x-file-cifs`, but against the same impacket server it opens
the TCP connection and closes it before NEGOTIATE (server log: `Incoming connection` then
`Closing down connection`, no SMB2 command), so every S1 request is a 404. The S1 client is a
different dialect/negotiation path; the matrix above is S2-only and the offsets are the S2-line
libsmb2 4.0.0.

## CVE-2026-110712 on the live path: present, not observable

`/getaa` makes the client issue QUERY_INFO with FileAllInformation, so the `FileNameLength`
over-read (`smb2_decode_file_all_info`) is on this path. Patching the reply's
`NameInformation.FileNameLength` (impacket struct field, confirmed on the wire) to `0x1000`,
`0x10000`, `0x100000`, `0x1000000`, `0x7fffffff` and `0xffffffff` against a 120-byte
`OutputBufferLength` gives **200 every time**, `/status/zp` 200 after each.

The walk stops at the first UTF-16 NUL in the heap, so the over-read stays inside mapped heap and
the device has no ASan to show it. The host-side PoC
(`captures/research/smb-fuzz/poc_queryinfo_oob.c`) remains the proof of the defect; `/getaa` is the
live reachability proof, not a crash oracle. CVE-2026-110713 (FileFsVolumeInformation) and
-110714 (security descriptor) are not reachable here: the client asks FileAllInformation only.

Harness: `~/sonos-root-local/recon/smbserv_qinfo.py <port> filename_len <value>`.

## Every reply field the client consumes, with wire offsets

`recon/smbserv_fuzz.py` mutates one named reply field per case (fresh port and server, warm-up request
first because a cold first connection 404s, liveness before/0.6 s/2.0 s after). Offsets are absolute in
the SMB2 PDU, 64-byte header included.

| op | field | type | wire offset | values tried | result |
|---|---|---|---|---|---|
| NEGOTIATE | `SecurityBufferOffset` | u16 | **120** | 127, 136 | 404; 129 tolerated |
| SESSION_SETUP | `SecurityBufferOffset` | u16 | **68** | 71, 73, 80 | 404 |
| SESSION_SETUP | `SecurityBufferLength` | u32 | **70** | 0, 1, 0x4000 | 404 |
| QUERY_INFO | `OutputBufferLength` | u32 | **68** | 0, 1, 0x10000, 0xffffffff | 404 |
| QUERY_INFO | `OutputBufferOffset` | u16 | **72** | 71, 73, 80 | 404 |
| READ | `DataLength` | u32 | **68** | > 0x4000 | 404 or **9.4 s hang** |
| READ | `DataOffset` | u8 | **66** | ≠ 0x50 | 404 |
| CREATE | `CreateContextsOffset`/`Length` | u32 | 144 / 148 | — | not exposed by this impacket build; contexts unused |
| IOCTL | `InputOffset`/`InputCount` | u32 | 88 / 92 | — | not issued by `/getaa` |

Only READ `DataLength` produces anything worse than a clean 404. The reason is visible in the parsers:
`smb2_process_query_info_fixed` checks `offset+length` against the received length (`0x1efb0`), the
length against `max_transact_size` (`0x1efc8`) and `offset >= 72` (`0x1efd0`);
`smb2_process_session_setup_fixed` checks `offset+length <= received` (`0x1fce8`) and `offset >= 72`
(`0x1fcfc`); `smb2_process_ioctl_variable` checks the count against the available bytes (`0x1d6ec`).
`smb2_process_read_fixed` has no length check at all — only `DataOffset == 0x50` (`0x1f930`).

Harness fixes needed to get here: `smb2Negotiate` returns `(None, [pkt], …)` and the packet carries the
response under `Data`, not `Buffer`; the original `res[0][0]` path raises for NEGOTIATE.

## The FileNameLength over-read: faulting instruction and a 2-byte margin

Call site: `smb2_decode_file_all_info` reads the u16 at payload+96 (`0x20fa0`–`0x20fb0`), then calls
`utf16_to_utf8(iov_base + 100, FileNameLength / 2)` at `0x20fc0` (PLT `0x40210`). Nothing compares
`FileNameLength` with `iov_len`.

Faulting instruction: `lhbrx r10, 0, r9` at **`0x270e4`** in the sizing pass, and the same load at
**`0x27164`** in the conversion pass. Both sit in a loop whose only bound is `p < base + FileNameLength`
(`0x270dc`, `0x2715c`), so the first load past the iovec is at `base + iov_len`. The surrogate branches
read `p + 2` with no bound at all (`0x271e8`, `0x27238`).

Steering: the reply's `OutputBufferLength` is set to the real serialized payload length
`100 + 2*UNITS`, the name is `'A' * UNITS` (no NUL halfwords), and `FileNameLength = 2*UNITS + 2`.
That makes the walk read exactly **one UTF-16 unit (2 bytes) past the buffer** — the minimum margin —
and the next bytes are the following heap chunk. At `UNITS = 1998` the payload ends at 4096, the
caller's buffer size, so the 2-byte read lands on the chunk after it.

Observed (fresh port and server per case, warm-up, liveness before/after):

| UNITS | payload end | 2-byte read lands | result |
|---|---|---|---|
| 1996–1999 | 4092–4098 | inside / just past the caller's buffer | 200 |
| 2000 | 4100 | past the buffer | 5/10 200, 5/10 no answer (worker pinned) |
| 2000–2005 (first scan) | 4100–4110 | past the buffer | curl 52 (empty reply, worker aborted) or 28 (hang) |

`/status/zp` answers 200 before, after and 1.2 s later in every case: the worker dies or stalls, the
daemon does not. The outcome depends on what the neighbouring chunk holds — a NUL halfword stops the
walk (200), anything else keeps it walking (hang), and an unmapped page aborts the connection (52).

Harness: `~/sonos-root-local/recon/smbserv_qinfo2.py <port> <units> <delta>`, sweep
`recon/getaa_oob.py <ip> <units>:<delta> …`.
