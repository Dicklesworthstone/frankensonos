# ROBUSTNESS — how FrankenSonos always "just works"

The owner's core complaint about the official software: apps that lose
devices, flaky discovery, grouping that fights you. Every mechanism below
answers a failure mode that was **observed on the owner's own LAN** (or is a
known, named risk from the protocol work in `docs/PROTOCOL.md`). For each:
the failure, the mechanism that absorbs it, and where it lives.

Design rule: identity is the RINCON UUID; everything else — IP, coordinator
role, render params, firmware — is an attribute that can change underneath
us and is re-learned, never assumed.

## 1. Discovery never loses devices

| Failure | Mechanism | Lives in |
|---|---|---|
| SSDP returns only one household per scan (observed 2026-10-06) | Scan repeatedly; filter `X-RINCON-HOUSEHOLD` client-side; direct-seed known player IPs from the cached inventory | `a-ssdp-j64`, `b-doctor-lan-checks-4gi` |
| Multicast filtering/saturation on some networks | Direct-seed fallback is a first-class path, not an error path | `a-ssdp-j64` |
| A whole discovery channel dies silently | **mDNS (`_sonos._tcp.local`) as a second, independent channel** — the speakers' own daemon reconciles SSDP vs mDNS sightings (found in `anacapad` strings); we should too | `re-mdns-discovery` (new) |
| DHCP moves a player's IP | All state keyed by UUID; IP refreshed on every discovery/NOTIFY/description fetch; on connect failure, re-resolve UUID → new address → retry once | `b-self-healing-7ts` |

## 2. Every control call self-heals addressing

| Failure | Mechanism | Lives in |
|---|---|---|
| Coordinator moved (reboot, pair role swap) | Coordinator resolved per-command from *fresh* topology; on UPnP 800 after a grouping verb, refresh ZoneGroupTopology and retry once **only if the coordinator actually moved** — 800 also means "bad Spotify desc", never blindly reclassified | `b-self-healing-7ts` |
| Player rebooted mid-conversation | `X-RINCON-BOOTSEQ` change or a failed GENA renew (412) → resubscribe + full state re-fetch | `b-self-healing-7ts` |
| Pause refused (UPnP 701): the group isn't playing, or a stream can't pause | Read the transport state back: already paused or stopped is done (a set's pause goes on), still playing stops instead; the fade-out and sleep paths share it, so their volumes are always put back | core `control::pause_at` |
| A scene, an announcement or an undo sets a volume while a fade runs there (a sleep timer's, a schedule's, one asked for) | The fade ends where it is first, so it does not overwrite the new level a step later; a sleep timer fading there ends too, as for any volume change, and an announcement ends the fades only once its turn has come. A sleep timer an announcement interrupted sleeps again once the music is back, fading over the usual time from where it came back | api `Surface::volumes_changing`, `Surface::announcing` |
| Calls that take long, many at once (announcements that play, fades asked for, DJ starts that ask Spotify, the doctor probing the LAN) | Each runs on a thread of its own; the listener's workers only move bytes, so other requests on the API and MCP listeners are answered meanwhile. At most 64 run at once: past that a call is refused busy (503 `NOT_READY`) rather than queued, and the HTTP API holds at most 256 connections; `/health` is always answered on the spot, so the CLI still finds a busy daemon | api `off_thread`, `http::Op::entry`; mcp `with_backend_off_thread`; cli `daemon::listener_runtime` |
| The music moves, or a group with a sleep timer joins another | The timer follows the music: a move re-keys it (and the player's own timer) to the coordinator the music plays under now. A room whose group joined another since has only its own music stopped: it fades, leaves that group, and gets its volume back; the group it joined plays on | core `SleepTimers::rekey`, `sleep::run`; api `sleep_follows` |
| The daemon stops mid-fade (a scheduled fade-out or ramp, `fade_and_pause`, `fade_volume`, a scene fade) | Every fade finishes at once instead of stopping part-way: a fade-out pauses its group and puts the volumes back, a ramp reaches its level. A sleep fade stops and puts the volumes back; the player's own timer pauses the group, and the timer is taken up again at the next start | core `Fader::finish_all`, api `Scheduler::shut_down` |
| A player drops off mid-fade (a sleep timer, a fade-out before pausing) | Every room's volume goes back, best effort, whatever step failed, so no group is left silent for its next play; a failed sleep timer is dropped, not left fading, and the player's own timer (set past the end) still pauses the group | core `sleep::run`, `fade::fade_out_and_pause` |
| Non-idempotent op retried into a duplicate | `AddURIToQueue` and friends are never blind-retried; state is checked first | `b-self-healing-7ts` |

## 3. Rendering never rots

| Failure | Mechanism | Lives in |
|---|---|---|
| Spotify relinked → `sid`/`flags`/`sn` drift | Render params are **learned from the household's own favorites at runtime**, never hardcoded (verified live on both households) | `a-didl-params-ffn`, PROTOCOL.md §4 |
| Learned params go stale anyway → UPnP 800 | On render 800: re-learn from favorites, retry once, then report `RENDER_PARAMS_STALE` with a remedy. The DJ learns them before every enqueue, so an 800 from a coordinator that still leads its group reports `RENDER_PARAMS_STALE` at once | `re-render-800-retry` (new) |
| Wrong `desc` (the original UPnP-800 killer) | Template pins `SA_RINCON<svc>` from the learned descriptor; doctor checks linkage per household | `b-doctor-render-checks-aok` |
| Single track ends → transport STOPPED | The DJ uses the queue flow (append → point → seek → play → next), never bare single-shot `SetAVTransportURI` for continuous listening | PROTOCOL.md §4, `c-dj-*` |
| The DJ's top-up fails while its last work plays (a refused render, a network blip) → the queue runs out and stops back on track 1 | The feed knows that stop: back on track 1, the queue's length unchanged, the last track having played. It tops up past the end and plays from the first new work; an owner's Stop or a pause is left alone | `dj-dry-queue-restart-c1dg` |

## 4. State stays live without polling storms

| Failure | Mechanism | Lives in |
|---|---|---|
| Polling costs/lag | GENA subscriptions: initial NOTIFY is full state; deltas after | `a-gena-gz5` |
| Subscription silently dies (player reboot, timeout) | Renew at ≤85% of granted timeout; renew failure (412) → resubscribe + re-fetch; one ZGT subscription anywhere tracks the whole household | `b-self-healing-7ts`, `a-gena-gz5` |
| Parser breaks on a shape Sonos changes | Every state-bearing service's event shape is pinned by a live-captured golden fixture (11 fixtures, both generations) | `re-fixtures-eji` |
| Sonos ships new firmware under us | `AvailableSoftwareUpdate` on ZGT events (only the offered version is read) → doctor `lan.updates` warns which rooms run older software; S1 is frozen forever (stable target) | doctor `lan.updates`, `re-firmware-kds` |
| Sonos's own updater is fragile | **Verified live on both generations (2026-10-10): an update stream interrupted at the wrong record boundary crashes the player outright** — S1 bridge (57.23, 20+ fires incl. Sonos's own shipping descriptor records; anacapad restart or watchdog reboot) and S2 One (97.1, descriptor-class inputs; anacapad restart). FrankenSonos never auto-updates; it warns, and the owner decides | `re-firmware-kds` |

## 5. Mutations are safe by construction

| Failure | Mechanism | Lives in |
|---|---|---|
| Destructive-ish op (ungroup, queue clear) regrets | Zone snapshot before mutation + persisted action log + undo | `b-snapshot-dpa`, `b-action-log-undo-jti` |
| "Did it actually work?" | Every SOAP answer is checked: a UPnP fault is a coded failure (with its `upnp_code`), never a silent success; a player that moved, or a coordinator that changed under a command, is found again and the command retried once (noted `HEALED`); scene apply and undo report each step that failed | api `heal`, `b-self-healing-7ts`, scenes, undo |
| Whole-container cleanup would be 491 fragile calls | `RemoveTrackRangeFromQueue` removes a range in one atomic call (verified live, both generations); undoing an add to the queue sends it, once it has read the tracks back where they were added, and so does the DJ, taking its long-played works off a queue grown past `trim_queue_at`; nothing trims a container yet | PROTOCOL.md §4 |
| Volume/group abuse by a buggy client | House policy: volume caps, step limits, quiet hours, client allowlists | `b-policy-23m` |

## 6. The daemon is supervised and honest

| Failure | Mechanism | Lives in |
|---|---|---|
| Daemon crash | launchd `KeepAlive` (the interim Python bridge already runs this way, dual-household, params-learned) | DEPLOY.md |
| Exposed control surface | Bind guard: loopback or tailnet only, never a public interface; Tailscale fronts the daemon, speakers stay LAN-only (and stay stock — see PROTOCOL.md §12) | README, `ts-*` epic |
| Something's wrong and you can't tell | `fsonos doctor` / `GET /doctor` / MCP doctor: store, SSDP, seeds, players, households, GENA round-trip, Spotify linkage, Tailscale. A caller the daemon cannot identify gets the last report while it is under 30 s old, so it cannot make the daemon probe the LAN in a loop | `b-doctor-*`, `d-doctor-surface-15q`, `doctor-lan-rate-phc` |
| Opaque failures | Stable error codes + hints + suggestions on CLI/HTTP/MCP; no silent fallbacks | `d-error-codes-tc4` |

## 7. The host and the network fail too (fleet-verified 2026-10-11/12)

| Failure | What was observed on the owner's fleet | Mechanism |
|---|---|---|
| Speaker VLAN outage | The whole speaker VLAN went dark for ~40 minutes (gateway unreachable, all devices silent, both households); the daemon answered `NOT_READY` naming the survey failure and recovered on its own when the VLAN returned | A discovery failure is a reported state, not a crash; the live model keeps resurveying (`no player answered discovery` + hint) |
| Two daemons on one host | The GENA callback port (8097) is fixed per host: a second daemon logs `Address already in use` and retries (~78 s), and its model looks stale until it binds and resubscribes | One daemon per host; when running a test daemon, stop the first — the port is baked into every subscription URL |
| Multicast egress on a multi-homed host | `fsonos discover` found nobody while the speakers answered HTTP: the kernel routes multicast by one route (en0) while the players lived on the en1 VLAN | Every LAN interface now gets its own query socket (`IP_MULTICAST_IF`, commit `6e83b3c`); an interface-enumeration failure falls back to the old wildcard socket |
| SSE clients through all of the above | A 15 s heartbeat keeps a quiet stream distinguishable from a dead connection, and reconnect replay (`Last-Event-ID` / `?since=N`) never skips an id the client has not seen | The event bus keeps a 1024-event ring; `since` means "I have seen up to and including this id" |

## What is deliberately NOT done

- **On-speaker Tailscale / custom firmware**: would *reduce* reliability
  (brick risk, per-update re-injection) for a benefit the daemon already
  provides. Full analysis: `docs/PROTOCOL.md` §12.
- **Polling-based state**: strictly worse than evented state for this
  workload; GENA + resubscribe covers the failure cases.
- **Retrying everything**: blind retries of non-idempotent verbs cause
  duplicates; only identity/addressing reads and the specific, classified
  cases above retry, exactly once.
