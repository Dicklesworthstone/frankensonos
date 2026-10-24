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

## Offline: the device's own wget, no reboots

The live matrix above costs a boot per case. The same binary runs offline:
`/usr/bin/wget` in the 1-16 rootfs is **the busybox binary** (md5
`bbf0ad32…` = `/bin/busybox`, v1.24.2), so the parser under test can be run
under `qemu-ppc` on an x86_64 guest at ~0.05 s/case.

**Exact argv** (from the `execl` at `0x10009060` in `upgrade`; path
`/usr/bin/wget`, argv0 `wget`, report file `0x…87e0`, URL from the caller on
the stack, `-` shared from `0x…7b88`):

```
/usr/bin/wget  wget  -qC /var/run/upgrade_report.txt  -T 300  -O -  <url>
```

`-C` takes the report file, not the URL — an easy guess to get wrong.
`/var/run` is a symlink to `/ramdisk/var`; that directory must exist in the
chroot. `-T` may be shortened to 5 for the harness.

Setup: extract the 1-16 rootfs (squashfs 3, `unsquashfs` 4.7 manages),
`chroot` into it with a static `qemu-ppc` copied in as `/qemu-ppc`, and run
`qemu-ppc -cpu e300c3`. `e300c3` is the model that works: `e500*` has no FPU
in QEMU (the uClibc loader faults on `stfd`), and the SPE-only models get that
far but crash; under `chroot` the host `/etc/ld.so.cache` is invisible so the
loader probes paths itself. Harness: `scripts/wget_offline_matrix.py`.

### Results (client stdout bytes / server bytes written / reset / rc)

| mode | out | sent | reset | rc | stderr |
|---|---|---|---|---|---|
| control | 5,606,749 | 5,606,872 | – | 0 | |
| chunk 0 | 0 | 5,606,880 | – | 0 | body read to EOF, none delivered |
| chunk 1 | 5,606,749 | 5,606,819 | – | 0 | |
| chunk `FFFFFFFF` | 5,606,749 | 5,606,885 | – | 1 | `error getting response` after the body |
| chunk `-1` | 8,255 | 126 | reset | 0 | **short success** |
| chunk `ZZZ` | 0 | 126 | – | 0 | |
| size leading zeros | 5,606,749 | 5,606,821 | – | 0 | accepted |
| size all zeros | 0 | 131 | – | 0 | |
| size `0x1` | 5,606,749 | 5,606,820 | – | 0 | `0x` accepted |
| size `+1` | 5,606,749 | 5,606,821 | – | 0 | `+` accepted |
| size `1 ` / `1\t` before CRLF | 5,606,749 | 5,606,822 | – | 0 | whitespace accepted |
| size 9-digit `100000000` | 5,606,749 | 5,606,827 | – | 1 | error after body |
| size `0xFFFFFFFFF` | 5,606,749 | 5,606,828 | – | 1 | error after body |
| size `1;x=y` | 5,606,749 | 5,606,825 | – | 0 | extension ignored |
| size uppercase `A` | 8,265 | 2,803,164 | reset | 0 | **short success, 2.8 MB consumed** |
| size `\r\n` split | 5,606,749 | 5,606,829 | – | 0 | survives 250 ms split |
| missing CRLF after data | 1 | 141 | – | 0 | stops after 1 byte |
| trailer 64 KiB after `0` | 0 | 196 | – | 0 | trailers not parsed |
| trailer 1 line after `0` | 0 | 146 | – | 0 | |
| TE + CL 10 | 0 | 86 | – | 1 | `error getting response` at headers |
| TE + CL 99999999 | 0 | 96 | – | 1 | same |
| duplicate CL 100/200 | 200 | 5,606,918 | – | 0 | **last CL wins** |
| CL `0xFFFFFFFF` | 0 | 78 | – | 1 | `content-length 0xFFFFFFFF is garbage` |
| CL 0 | 0 | 59 | – | 0 | |
| 100-continue then 200 | 5,606,749 | 5,606,912 | – | 0 | accepted |
| 204 with body | 5,606,749 | 5,606,780 | – | 0 | body read |
| 304 with body | 0 | 60 | – | 1 | `server returned error: HTTP/1.1 304` |
| body + second response | 5,606,749 | 5,606,803 | – | 0 | second response ignored |
| 302 ×1 | 5,606,749 | – | – | 0 | |
| 302 ×10 / ×100 | 0 | – | – | 1 | `too many redirections` |
| 4 KiB Location | 5,606,749 | – | – | 0 | |
| 500 | 0 | 64 | – | 1 | `server returned error: HTTP/1.1 500` |
| header 4 KiB / 64 KiB | 5,606,749 | – | – | 0 | accepted |
| 1 MiB status line | 0 | 131,085 | – | 1 | error text is the tail of the reason |

`A` is a *desync*, not a clean 10-byte chunk: the mode sends one data byte
against the ten it claims, so the decoder consumes the frame tail and then
parses data as a size. The `-1` case is the same shape with a size that never
counts down. The 9-digit / `FFFFFFFF` rows are the opposite: the chunk claims
more than the whole body, the client delivers every real byte, then hits EOF
and reports an error.

No frame crashes the client and none hangs (max 0.34 s). The live results agreed
where they overlap, with one difference: the live `chunk 0` server saw a reset
after 131 bytes because wget's stdout is a pipe to `upgrade`, which closes;
offline stdout is a file, so it drains. Both are "0 bytes delivered" at the
client.

Two behaviours are worth a device run if anything is retried: the **short
successes** (`-1`, uppercase hex → ~8.2 KiB delivered, megabytes consumed, exit
0) and the **post-body error** on oversized sizes, where the deliverable body is
complete but the decoder still reports failure.
