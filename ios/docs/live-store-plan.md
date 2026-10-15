# Live store: the iPhone app talks to `fsonos serve`

Everything below was observed against `fsonos sim` + `fsonos serve` (release build, loopback), not read from docs.
Captured responses live in `ios/Tests/Fixtures/`.

```
SwiftUI views ── ZoneStore ──┬─ MockZoneStore   (screenshots, previews)
                             └─ LiveZoneStore ── DaemonClient ──HTTP──▶ fsonos serve
                                       ▲                          │
                                       └──── SSE /events ◀────────┘
```

## What the daemon gives us (observed)

| Need | Source | Shape |
|---|---|---|
| Rooms and groups | `GET /zones` | `[{coordinator_room, members[], transport_state, household}]` |
| One room's state | `GET /zones/{room}/state` | `{zone, transport_state, volume?, track?}`; `volume` is that room's own |
| Track | `track` in state | `{title?, creator?, album?, uri, duration_secs?, position_secs?, queue_position?}`; absent when stopped |
| Favorites (Browse, My Sonos) | `GET /favorites?zone=R` | `[{id, title, kind, description, art_uri?}]` |
| Search | `GET /library/search?q=` | hits, shape to be captured |
| Live changes | `GET /events` (SSE, `Last-Event-ID` or `?since=`) | `zone.state` deltas, `topology.changed`, `player.health`, `action.logged`, `events.reset`, `: heartbeat` every 15 s |
| Controls | `POST` JSON, `content-type: application/json` | `/pause /resume /next /previous {zone}`, `/volume {zone, volume or delta, group?}`, `/mute`, `/group {zone, to}`, `/ungroup {zone}`, `/play/favorite {zone, favorite}`, `/play`, `/dj/*` |
| Errors | 4xx/5xx JSON | `{detail, code, hint, suggestions[], retryable}` |

Gaps the app must live with in v1 (daemon work is a separate track):

| Gap | v1 behavior |
|---|---|
| Event `track` field is only a URI, not metadata | refetch `/zones/{room}/state` for that zone (coalesce 150 ms) |
| No album art URL on `TrackDto` | generated procedural art keyed on title; favorites use `art_uri` when present |
| Per-member volume needs one call per room | fetch on bootstrap and on `volume` events; no group-wide poll |
| No queue listing (only `queue_length`) | queue button shows "n tracks", no list |
| No Browse tree (Sonos Radio, services) | Browse lists favorites as one source |

## Failure matrix

| # | State or input | What the store does | How it can fail | What the user sees |
|---|---|---|---|---|
| 1 | Daemon unreachable at launch | state `.offline`, retry with backoff 1, 2, 4, 8, 15 s | wrong URL, daemon down, Tailscale off | banner "Can't reach the daemon at URL" with Retry; never falls back to mock data |
| 2 | SSE drops mid-stream | reconnect with `Last-Event-ID`, same backoff | proxy resets, phone sleeps | banner "Reconnecting", controls stay enabled |
| 3 | SSE open but silent for more than 40 s | treat as dead, reconnect | missed heartbeat | same as 2 |
| 4 | `events.reset` | full refetch of `/zones` and every room state | ring overflow after long sleep | brief "Refreshing" |
| 5 | `zone.state` for a room not in the model | refetch `/zones` first, then that room | race with a topology change | none |
| 6 | Command returns 4xx | revert optimistic change, show `detail` and `hint`; offer `suggestions` for room typos | stale room name, bad volume | toast |
| 7 | Command times out or 5xx | revert, refetch affected zone, toast "Didn't go through" | daemon busy, speaker offline | toast |
| 8 | Volume slider drag | optimistic, debounce 100 ms, last write wins; ignore echoed `volume` events for that room while dragging; reconcile on release | echo fights the thumb | smooth slider |
| 9 | Group or ungroup | optimistic list change; `topology.changed` can arrive before the POST response, so apply it idempotently by refetch | double apply, flicker | rooms regroup once |
| 10 | Track change event | refetch state for that zone only | N+1 storm on a party-mode change | title updates within 1 s |
| 11 | `duration_secs` is 0 or absent (streams) | hide scrubber, show "Live" | divide by zero, empty bar | no broken scrubber |
| 12 | Playing position | tick locally from `position_secs` plus elapsed; resync on each state fetch | drift | steady counter |
| 13 | Room names with spaces, accents, `+` | percent-encode path segments | 404 or wrong room | works |
| 14 | Two households (S1 and S2) | rooms list shows both; names are unique per household in `/rooms` | clash on same name | grouped by household only if names collide |
| 15 | Unknown event kind or unknown JSON field | ignore | decoder throws and kills the stream | none |
| 16 | `player.health` offline | dim that room row, disable its controls | commands hang | "Offline" subtitle |
| 17 | App backgrounded | cancel SSE, on foreground refetch everything then reconnect | stale UI | fresh on return |
| 18 | Daemon URL setting | stored in UserDefaults, editable in Settings, default `http://127.0.0.1:8099`; HTTP allowed for loopback and `*.ts.net` | ATS blocks HTTP | works, Settings shows the host |
| 19 | No daemon URL chosen yet, or the user wants another | browse `_fsonos._tcp` over Bonjour, resolve each service with NetService (IPv4 address, else the `.local` name); first launch picks the first one found, Settings lists them all | Bonjour type missing from `NSBonjourServices`, a host that publishes only IPv6, multicast blocked off the LAN | the daemon is found with no typing; away from home the URL field still works |

## Test plan (red first)

All E2E, run against a real `fsonos sim` and `fsonos serve` on random loopback ports, driven by `ios/tools/e2e-live`
(starts both, waits for `/health`, runs the Swift checks, tears down, prints a PASS line per matrix row).
Swift side runs as a macOS SwiftPM test target that compiles `Model/` and `Net/` only (no SwiftUI), because iPhone
simulators do not boot on the dev Mac.

1. Rows 1 and 2: start with the daemon down, assert `.offline`; start it, assert `.live`; kill and restart serve, assert reconnect and resume.
2. Rows 6 to 10: drive `play/favorite`, `volume`, `group`, `pause` through the store; assert model state matches `GET /zones` after each, and that a bad room name reverts.
3. Rows 11 to 13, 15: decoder tests on the captured fixtures plus mutated copies (missing track, unknown field, unknown event).
4. A mutation check: break the revert path and the reconnect path once each and confirm the matching test goes red.

## Order of work

1. Fixtures, `ios/tools/e2e-live`, red tests.
2. `Net/DaemonClient.swift` (HTTP, SSE line parser, backoff) and `Net/DTOs.swift`.
3. `LiveZoneStore` implementing the existing `ZoneStore`, with deterministic UUIDs per coordinator room so the views do not change.
4. Settings: daemon URL field and connection status row; banner view.
5. Wire the app to choose the store from a launch flag (`-mock`) and default to live.
