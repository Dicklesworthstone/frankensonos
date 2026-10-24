# The update fetcher's HTTP response parser (S1 57.23)

Owner-directed RE. The player runs `wget -qC <url> -T 300 -O - -` as **root**
for the image URL, before any package parsing or signature check. The response
is therefore an unauthenticated, root-context parser surface and independent of
the file bytes. This is the matrix for malformed response frames; every case
serves the same stock 5,606,749-byte `57.23-74170-1-16.upd`.

Harness: `scripts/update_http_fuzz.py` (serve mode = mutated frame; run mode =
trigger `BeginSoftwareUpdate` with a direct `/update.upd` URL, record how far
the client got, then poll `:1400` for recovery). Bytes are counted at the
server before the client closes; `5,606,872` is header + body (5,606,749).

## Metrics

| mode | requests | response bytes written | abort point | daemon down (s) | recovery |
|---|---|---|---|---|---|
| control (CL, 200) | 1 | 5,606,872 | complete | 10.2 | up < 90 |
| chunked size 0 | 1 | 131 | after the `0\r\n\r\n` | 2.3 | 8.9 |
| chunked size 1 | 1 | 5,609,628 | complete | 10.9 | — |
| chunked size `FFFFFFFF` | 1 | 5,606,885 | complete | 2.2 | — |
| chunked size `-1` | 1 | 126 | status/headers | 0.0 | 3.6 |
| chunked size `ZZZ` | 2 | 126 | status/headers | 0.0 | 10.0 |
| 302 chain, 1 hop | 2 | 5,606,872 | complete | 16.1 | — |
| 302 chain, 10 hops | 5 | 375 (5 × 75) | after 4 redirects | 0.0 | 3.6 |
| 302 chain, 100 hops | 5 | 375 (5 × 75) | after 4 redirects | 0.0 | 4.2 |
| Location 4 KiB | 2 | 4,169 + 5,606,872 | complete (path truncated to 4,084 B) | 0.0 | 4.0 |
| `500` status | 1 | 142 | after the headers | 6.2 | 7.3 |
| header line 4 KiB | 1 | 5,610,978 | complete | 0.0 | — |
| header line 64 KiB | 1 | 5,672,418 | complete | 0.1 | — |
| 1 MiB status line | 1 | 131,085 | mid-status-line (~128 KiB) | 36.5 | 37.7 |

`—` = the unit was still recovering when the 60 s poll expired; the abort and
byte figures do not depend on it. A few triggers needed a re-fire while the
daemon was in one of its restart windows; the server request count shows it.

## Reading

- **Chunked framing is only half-implemented.** A `0` size terminates the body
  even when real bytes follow (131 bytes delivered, rest ignored), and both
  `-1` and a non-hex size are rejected at the status/headers stage. `1` is read
  correctly, and a wildly oversized size (`FFFFFFFF`) is accepted and the body
  streams normally — the size is not sanity-checked against a remaining length
  or a buffer.
- **Redirect handling stops after four redirects (five requests)**, not at 10
  or 100; no error body is read. One hop is followed and the body completes.
- **A 4 KiB `Location` is accepted; wget truncates it.** The client requested a
  4,084-byte path (plus `/` = 4,085), 12 bytes shorter than the header value,
  then looped until the server was taught to serve the truncated path. So the
  redirect URL buffer is ~4,096 bytes and a longer Location is silently cut
  mid-path.
- **Header size is generous but bounded.** A 64 KiB header line is fine; a
  1 MiB status line is aborted at 131,085 bytes (~128 KiB), which is the total
  response-header budget. The abort leaves the daemon to restart (down 36.5 s).
- **A `500` is not retried**: the client reads only the headers and gives up.
- **No crash and no wedge from any frame.** Every case recovered; the down
  times are the updater's own daemon restart after an attempt (2–37 s), not a
  device reboot, and `/status/zp` still reports the same version. The existing
  one-attempt-per-boot cooldown still applies between cases.

## Pre-auth surfaces that join request data

Two cheap surfaces were fuzzed in the same batch (harness
`scripts/preatuh_fuzz.py`, `/info` liveness after every case):

- `GET /status/ZPLocalSettingsFile/<name>` — the name is joined to the
  settings root. Names of 256/1024/2048/4096 bytes, plain and with a `../`
  prefix, all answer `200` with the empty-document baseline (111 bytes), and
  the daemon stays up. At an 8,220-byte request line the connection is reset,
  and the same at 65,564; so the join never sees more than ~4 KiB and the
  request-line limit (~4–8 KiB) is reached first. **No reachable join
  overflow; no 414, just a reset.**
- `POST /mtmhhsetup` — **this one is a gate record, not a parser fuzz.** On
  both locked 86.10 units every case answers `403`
  (the route is in the devmode/locked table class, flag `0x100`), including
  `hhid` 33/64/1024/65536, `name` 32/128/1024, `icon` 129/1024/65536 and the
  all-three combinations; `/info` stays up. A 196,669-byte body (all-64k)
  makes the *client* see a broken pipe because the daemon closes without
  reading the body. The handler's own 4,095-byte content-length cap and field
  checks only run once the unit is unlocked. No mtmhhsetup case reached
  `FUN_004f3b44`; the field caps are therefore untested live, not confirmed.
