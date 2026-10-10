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
| Non-idempotent op retried into a duplicate | `AddURIToQueue` and friends are never blind-retried; state is checked first | `b-self-healing-7ts` |

## 3. Rendering never rots

| Failure | Mechanism | Lives in |
|---|---|---|
| Spotify relinked → `sid`/`flags`/`sn` drift | Render params are **learned from the household's own favorites at runtime**, never hardcoded (verified live on both households) | `a-didl-params-ffn`, PROTOCOL.md §4 |
| Learned params go stale anyway → UPnP 800 | On render 800: re-learn from favorites, retry once, then report `RENDER_PARAMS_STALE` with a remedy | `re-render-800-retry` (new) |
| Wrong `desc` (the original UPnP-800 killer) | Template pins `SA_RINCON<svc>` from the learned descriptor; doctor checks linkage per household | `b-doctor-render-checks-aok` |
| Single track ends → transport STOPPED | The DJ uses the queue flow (append → point → seek → play → next), never bare single-shot `SetAVTransportURI` for continuous listening | PROTOCOL.md §4, `c-dj-*` |

## 4. State stays live without polling storms

| Failure | Mechanism | Lives in |
|---|---|---|
| Polling costs/lag | GENA subscriptions: initial NOTIFY is full state; deltas after | `a-gena-gz5` |
| Subscription silently dies (player reboot, timeout) | Renew at ≤85% of granted timeout; renew failure (412) → resubscribe + re-fetch; one ZGT subscription anywhere tracks the whole household | `b-self-healing-7ts`, `a-gena-gz5` |
| Parser breaks on a shape Sonos changes | Every state-bearing service's event shape is pinned by a live-captured golden fixture (11 fixtures, both generations) | `re-fixtures-eji` |
| Sonos ships new firmware under us | `AvailableSoftwareUpdate` oracle on ZGT events → doctor warns; S1 is frozen forever (stable target) | `re-firmware-kds`, doctor |
| Sonos's own updater is fragile | **Verified on the owner's bridge (57.23, 2026-10-10): an update stream interrupted at the wrong record boundary crashes the player outright** (anacapad restart or watchdog reboot; 20+ live fires incl. Sonos's own shipping descriptor records). FrankenSonos never auto-updates; it warns, and the owner decides | `re-firmware-kds` |

## 5. Mutations are safe by construction

| Failure | Mechanism | Lives in |
|---|---|---|
| Destructive-ish op (ungroup, queue clear) regrets | Zone snapshot before mutation + persisted action log + undo | `b-snapshot-dpa`, `b-action-log-undo-jti` |
| "Did it actually work?" | Verify-after-mutate reads (e.g. GetTransportInfo after Play) on the orchestration path | `b-orchestration-jtz` |
| Whole-container cleanup is 491 calls of fragile | `RemoveTrackRangeFromQueue` — one atomic call (verified live, both generations) | PROTOCOL.md §4 |
| Volume/group abuse by a buggy client | House policy: volume caps, step limits, quiet hours, client allowlists | `b-policy-23m` |

## 6. The daemon is supervised and honest

| Failure | Mechanism | Lives in |
|---|---|---|
| Daemon crash | launchd `KeepAlive` (the interim Python bridge already runs this way, dual-household, params-learned) | DEPLOY.md |
| Exposed control surface | Bind guard: loopback or tailnet only, never a public interface; Tailscale fronts the daemon, speakers stay LAN-only (and stay stock — see PROTOCOL.md §12) | README, `ts-*` epic |
| Something's wrong and you can't tell | `fsonos doctor` / `GET /doctor` / MCP doctor: store, SSDP, seeds, players, households, GENA round-trip, Spotify linkage, Tailscale | `b-doctor-*`, `d-doctor-surface-15q` |
| Opaque failures | Stable error codes + hints + suggestions on CLI/HTTP/MCP; no silent fallbacks | `d-error-codes-tc4` |

## What is deliberately NOT done

- **On-speaker Tailscale / custom firmware**: would *reduce* reliability
  (brick risk, per-update re-injection) for a benefit the daemon already
  provides. Full analysis: `docs/PROTOCOL.md` §12.
- **Polling-based state**: strictly worse than evented state for this
  workload; GENA + resubscribe covers the failure cases.
- **Retrying everything**: blind retries of non-idempotent verbs cause
  duplicates; only identity/addressing reads and the specific, classified
  cases above retry, exactly once.
