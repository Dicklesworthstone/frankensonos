# URI handling matrix (SetAVTransportURI / AddURIToQueue)

20 fixed URI shapes fired at the AVTransport control, one case at a time, with a
liveness gate before each. Server: `python3 -m http.server 8898` on
`192.168.4.160` serving `captures/firmware` (log `/tmp/uriweb.log`); volume 0.
Each case is `AddURIToQueue` (empty metadata), then — when a position comes
back — `SetAVTransportURI` to `x-rincon-queue:<own-udn>#pos` and `Play`, then
state + whether the server saw a GET. Runner: `captures/research/uri-fuzz/run_queue_cases.py`.

Codes are UPnP codes (the HTTP status is always 500 with them).

## .202 (S1 57.23, 2026-10-24)

`.202` arrived in a degraded transport state: `GetMediaInfo` reported
`x-rincon:RINCON_000E585C3DF801400`, a coordinator that is not in its own
zone-group state (`BecomeCoordinatorOfStandaloneGroup` answers 1023 while
`PLAYING`). Under that state:

- `AddURIToQueue` with empty metadata: one run answered **200** for
  `x-rincon-mp3radio://http://192.168.4.160:8898/x.mp3` with position 1; the
  same call, once the queue transport had been selected (500 but accepted) and
  played, produced a real `GET /x.mp3 200` in the server log. That is the only
  observed end-to-end playback.
- Repeating the matrix in the same state gave **800** for every shape on Add
  (radio, `%00`, `%2f`, `../`, `file:///VERSION`, `x-sonos-http:`), and
  `SetAVTransportURI` with a DIDL consistently **402** while empty metadata
  gave 500/1023 or no response at all. Six-shape confirmation: all 800.
- The daemon stayed up throughout (liveness 200 before every case); the
  failures are transport-state, not crashes.

## .81 (S2 86.10, 2026-10-24) — same build as .86

`AddURIToQueue` (empty metadata), everything else identical:

| shape | len | Add |
|---|---|---|
| `x-file-cifs://nas/shared/x.mp3` | 30 | **200** |
| `x-file-cifs://…` 4 KB path | 4040 | 804 |
| `x-file-cifs://user:<1800>@nas/…` | 3836 | 804 |
| `x-rincon-mp3radio://http://…/x.mp3` | 51 | **200** |
| `…/a%00b.mp3` | 55 | **200** |
| `…/a%2fb.mp3` | 55 | 804 |
| `…/sub/../x.mp3` | 58 | **200** |
| `…/a b.mp3` | 53 | **200** |
| `file:///jffs/system/Configure.dev` | 33 | 804 |
| `file:///jffs/localsettings.txt` | 30 | 804 |
| `file:///jffs/VERSION` | 20 | 804 |
| `file:///jffs/settings/../../../etc/passwd` | 41 | 804 |
| `file:///tmp/device_unlocked_flag` | 32 | 804 |
| `x-sonos-http://…/x.mp3` | 39 | **200** |
| `x-sonos-http://…/a%00.mp3` | 42 | **200** |
| `x-sonos-http://…` 4 KB path | 3639 | 804 |
| `x-rincon-mp3radio://…` 4 KB path | 3651 | 804 |
| `…/stream.mp3?` + 100 params | 836 | 402 |
| `x-sonos-http:…` + 100 params | 829 | 402 |
| `x-rincon-mp3radio://http://…:8898/` (empty path) | 46 | **200** |

- **804 tracks URI length**, not scheme: the same `x-file-cifs` and radio shapes
  are 200 at 30/51 bytes and 804 at 3.6–4 KB (a 3 KB boundary is the next read).
- **All five `file:///` shapes are 804** at trivial lengths, so a local file is
  not a queue-acceptable media source on S2.
- **402 for 100 query parameters** on both schemes (params are parsed, then
  refused; not a length effect — 836 < the 3 KB limit).
- `SetAVTransportURI` to `x-rincon-queue:<own-udn>#pos` answered **1023** for
  every accepted add and nothing fetched: `.81` follows a coordinator on the
  `192.168.6.x` segment (`RINCON_5CAAFD21D47001400`, `192.168.6.112:1400`) that
  is **not reachable from the bench host**, and queue playback needs it.

## .86 (S2 86.10) — blocked by topology

`.86` is in the same household and follows the same unreachable coordinator.
Here `AddURIToQueue` answered **800** for every shape (radio, with and without
metadata) and every `SetAVTransportURI`, including the queue form, 800/blank.
`MusicServices` on `.86` lists Spotify as `Id=12`; adding a DIDL with
`SA_RINCON52231_X_#Svc52231-0-Token` and/or `?sid=12` changed the answer to
**402**, not to acceptance. `.81` (same build, same group, own transport blank)
accepts adds, so the 800 is the transport state, not the firmware.

## Open

- Get `192.168.6.112:1400` reachable (or make a unit standalone) and re-run:
  the Set half of every accepted case is unmeasured until then.
- Bound 804 precisely (≈3 KB) and whether `%2f` is decoded before the length
  check.
- `file:///` being 804 everywhere means the `Configure.dev` boot file still
  cannot be a playback source even if it exists.
