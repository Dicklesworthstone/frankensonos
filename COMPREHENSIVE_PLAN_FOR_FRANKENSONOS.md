# COMPREHENSIVE PLAN FOR FRANKENSONOS

> A memory-safe Rust end-run around Sonos's own software. Control the Sonos
> gear **you already own**, on **your own LAN**, reliably, from a CLI, an HTTP
> API, and an MCP server — so AI agents can act as a tasteful DJ for you and so
> the system stops being frustrating.

**Status:** planning + scaffold complete; implementation in progress via a
beads task graph and a multi-agent swarm. This document is the self-contained
source of truth. A fresh contributor (human or agent) should be able to read
this and start implementing without asking for clarification.

**Date:** 2026-10-06. **Toolchain:** Rust `nightly-2026-08-31`, edition 2024.
**Repo:** <https://github.com/Dicklesworthstone/frankensonos>.

---

## 0. Why this exists (the "why", at length)

The owner has spent a great deal of money on Sonos hardware across two
households and finds the official software unreliable and frustrating. The
legacy **S1** household (Play:5 Gen 1 units plus two Bridge ZB100s) is on the
final, now-unsupported S1 firmware line; the **S2** household (Play:1, Sonos
One Gen 1) is current. Streaming is almost entirely **Spotify Premium**. The
owner wants three things:

1. **Reliability and control** — a system that reliably discovers every
   speaker, shows true group topology, and plays what it is told, instead of
   phone apps that lose devices or refuse to start playback.
2. **A real "DJ"** — an agent that plays a pleasant, varied stream of classical
   music drawn from the owner's *own* Spotify library (saved albums, liked
   tracks), with sensible variety and anti-repeat, controllable by voice/agent
   with no phone in hand.
3. **Agent-native** — first-class control surfaces so personal AI agents (Meta
   Muse on this Mac, Grok, OpenAI "dots", Claude) can manage the house audio
   through a stable, documented API and MCP tools — including from off-LAN over
   Tailscale.

The official stack does all of this badly or not at all (notably, the Spotify
Web API *cannot* start playback on a Sonos; see §6). The mature open-source
controllers — **SoCo** (Python), **node-sonos** / **node-sonos-ts** (JS), and
**Home Assistant's Sonos integration** — prove that a local controller talking
the devices' own UPnP/SOAP protocols is reliable and complete. FrankenSonos is
that controller, rebuilt in memory-safe Rust on the owner's "franken" library
stack, with an agent-native surface and a DJ brain on top.

### Non-goals / explicit scope boundary (read `docs/SCOPE.md`)

This project is **interoperability with hardware the owner possesses**, the
same category as SoCo and Home Assistant. It is **not**:

- a tool to defeat authentication, DRM, or access controls on anything;
- a tool to extract, exfiltrate, or crack credentials or secrets;
- a tool that targets devices or accounts the owner does not control;
- (for the autonomous swarm) firmware reflashing or binary reverse-engineering.

Custom/alternative **firmware** for the owner's own devices is a *possible,
much-later, human-led research track* only. It is **out of scope for the
autonomous agent swarm** and is not required for any of the three goals above —
external UPnP control already achieves them. Reflashing risks bricking devices;
treat the externally-controlled daemon as the product. See `docs/SCOPE.md` for
the authoritative in-scope / out-of-scope list every contributor must follow.

---

## 1. What success looks like (user-visible workflows)

1. **Discover & inspect.** `fsonos discover` lists every player across both
   households with room, IP, model, generation, and group membership — every
   time, deterministically. `fsonos zones` prints the live topology.
2. **Direct control.** `fsonos play "Jeff's Office" spotify:track:...`,
   `fsonos pause "Kitchen"`, volume/group/ungroup — all fast and correct,
   addressing the **coordinator** of the target group automatically.
3. **The DJ.** `fsonos dj start "Jeff's Office"` begins a varied classical set
   from the owner's Spotify library; `dj skip` advances; the daemon keeps the
   queue fed and avoids recent repeats. An agent can do the same via MCP:
   `dj_start`, `dj_skip`, `play`, `set_volume`, `list_zones`.
4. **Always-on daemon.** `fsonos serve` runs a long-lived process that holds
   live state via GENA event subscriptions (no polling storms), exposes the
   HTTP API and the MCP server, and keeps the DJ running. Managed by launchd on
   the Mac mini that sits on the speaker LAN.
5. **Off-LAN via Tailscale.** An agent on any tailnet device reaches the daemon
   over Tailscale; the daemon is the only thing fronted — the speakers never
   leave the LAN.

---

## 2. Environment (ground truth for this deployment)

- **Build/runtime host:** `mac-mini-max`, macOS 26.2, arm64. It is on the
  speaker LAN directly (`en1` = `192.168.4.165/22`, covering
  `192.168.4.0`–`192.168.7.255`), and on the tailnet (`100.68.51.94`). This is
  the machine the daemon runs on.
- **Builds** offload to the `rch` fleet (remote cargo). `cargo` is the gate;
  `rch` is just where it runs. The pinned nightly builds cleanly there.
- Household/device specifics (IPs, MACs, serials) are **site data** and live in
  a local, git-ignored inventory file — never in the repo. The daemon learns
  them at runtime via discovery; a direct-seed fallback list may be kept
  locally (`local/seeds.toml`) for networks where SSDP is flaky.

---

## 3. Architecture

A single Cargo workspace, one binary (`fsonos`) that is both CLI and daemon,
and a library split so the four implementation lanes own disjoint crates:

```
crates/
  fsonos-types    Pure domain vocabulary (Player, ZoneGroup, Track, ...). No I/O.
  fsonos-proto    LANE A: SSDP discovery, UPnP/SOAP control, GENA eventing, DIDL.
  fsonos-core     LANE B: inventory, topology, grouping, control orchestration,
                          durable store (fsqlite). The daemon's brain.
  fsonos-spotify  LANE C: Spotify Web API (library reads) + the DJ engine.
  fsonos-api      LANE D: HTTP control API (fastapi_rust), Tailscale-fronted.
  fsonos-mcp      LANE D: MCP server (fastmcp_rust) — agent tools.
  fsonos-cli      LANE D: `fsonos` binary — CLI + `serve` daemon wiring.
```

**Design principles** (mirrored in `AGENTS.md`):

- **Pure core, I/O at the edges.** Protocol encoding/decoding (SOAP envelopes,
  DIDL, SSDP messages, GENA headers) and DJ selection are pure functions,
  unit-tested without a network. I/O sits behind traits (`Transport`, `Store`)
  so the franken async stack is wired in one place.
- **Coordinator-addressed control.** Group-wide commands (play/pause/volume for
  a group) target the group's coordinator; the core resolves it.
- **Event-driven state, not polling.** The daemon SUBSCRIBEs to GENA events for
  topology/transport/volume and renews before timeout. Polling is a fallback.
- **Two households, cleanly.** S1 and S2 differ (music-service account
  linkage, Queue service, SonosNet vs WiFi). The model carries a `Generation`
  and the control paths branch where the protocols genuinely differ.
- **`#![forbid(unsafe_code)]`** at every crate root. This workload has no reason
  for unsafe.

---

## 4. The franken stack & the dependency recipe (highest integration risk)

FrankenSonos is built on the owner's Rust libraries. None are on crates.io for
their patched versions; the **known-good** wiring (proven by the `am_baseline`
Agent-Mail project, which combines the same stack with a committed lockfile) is:

| Library | Role in FrankenSonos | Source (proven) |
|---|---|---|
| **asupersync** `0.5` | async runtime: TCP/UDP(+multicast), HTTP/1.1 client+server, TLS, timers, structured concurrency, cancellation (`Cx`) | crates.io registry `"0.5"`, feature `tls-webpki-roots` |
| **fsqlite** (frankensqlite) | durable local store (device/library caches, play history, DJ + render params) | git `frankensqlite@2633b38…` |
| **fastmcp** (fastmcp_rust) `0.10` | MCP server (stdio + streamable HTTP) | git `fastmcp_rust@03b5274…` |
| **fastapi** (fastapi_rust) `0.4` | HTTP control API | git `fastapi_rust@cb9d729…` (HEAD) |
| **rano** | *human-operated* LAN diagnostic only; NOT a build dep | n/a |

**Integration hazards (resolve empirically in bead `FND-DEPS`, do not guess):**

1. **asupersync must unify on one `0.5.x`** across our crates + fastmcp +
   fastapi. fastmcp pins `=0.5.0`; fastapi uses `0.5.0`. Depend on
   `asupersync = "0.5"` so the graph resolves to the single registry version.
   Our local checkout is `0.6.0` — do **not** path/git-dep asupersync, or you
   fork the type universe and `Cx` won't match across crates.
2. **fsqlite rides its own (older) asupersync line internally.** That is fine
   *only* if we use fsqlite's **non-`Cx` `Connection` API** (`Connection::open`,
   `execute_with_params`, `query`, `prepare` — all `async`, driven by our
   runtime's `block_on`). Prove a real open→create→insert→query round-trip in
   `FND-DEPS` before any lane relies on it. If the two runtimes fight over the
   reactor, fall back to running the store on a dedicated thread with its own
   mini-runtime and a channel. `fsqlite` has **no `bundled` feature** (it is a
   from-scratch engine, not a C-SQLite wrapper).
3. **No `bundled`, no Tokio, no reqwest.** All networking is asupersync.

The exact dependency lines (with revs) are kept, commented, in the workspace
`Cargo.toml`. `FND-DEPS` un-comments the needed subset per crate and proves
`cargo check --workspace` still passes.

**Real API shapes** (verified against the local library sources — use these,
don't reinvent):

- Runtime: `asupersync::runtime::RuntimeBuilder::current_thread().with_reactor(asupersync::runtime::reactor::create_reactor()?).blocking_threads(0,16).build()?` then `rt.block_on(async { … })`. Inside, get the context with `asupersync::Cx::current()`.
- UDP multicast (SSDP): `asupersync::net::UdpSocket::bind(addr).await`, then `sock.join_multicast_v4(Ipv4Addr::new(239,255,255,250), Ipv4Addr::UNSPECIFIED)?`, `send_to`, `recv_from`.
- HTTP client (SOAP/Spotify): `asupersync::http::Client::default_for_runtime(cx)`, `.post(url).header(..).body(..).send(cx).await` → `resp.status: u16`, `resp.body: Vec<u8>`.
- HTTP server (GENA callback sink): `asupersync::http::h1::Http1Listener::bind(addr, handler).await?` then `.run(&handle).await`.
- fsqlite: `fsqlite::Connection::open(path).await?`, `.execute_with_params(sql, &[SqliteValue::…]).await?`, `.query(sql).await?` → `Vec<Row>`, `row.get(i) -> Option<&SqliteValue>`; placeholders `?1,?2`. `Connection` is `!Send`.
- fastmcp: `#[fastmcp_rust::tool(description="…")] async fn play(ctx:&McpContext, args…) -> McpResult<String>`; build with `ServerBuilder::new(name,ver).tool(Play).build()`; run `.run_http(&cx, "0.0.0.0:8098").await?` or `.run_stdio_with_cx(&cx).await`.
- fastapi: `#[get("/zones")] async fn zones(cx:&RequestContext) -> Result<Json<Vec<ZoneDto>>, HttpError>`; `App::builder().route_entry(zones_route()).build()`; `serve(app, "0.0.0.0:8099").await`.

---

## 5. Lane specifications

Each lane owns disjoint crates (no file contention). `FND-DEPS` is the shared
prerequisite that wires the franken deps; until it lands, lanes build their
pure logic (encoding, selection, DTOs) which needs no franken deps.

### LANE A — `fsonos-proto` (interoperability protocol layer)

Owner goal: talk to the owner's players exactly as SoCo/node-sonos do.

- **SSDP discovery.** `M-SEARCH` for `urn:schemas-upnp-org:device:ZonePlayer:1`
  on `239.255.255.250:1900`; collect `LOCATION` URLs; fetch and parse each
  `…/xml/device_description.xml`. SSDP returns one household per scan — iterate
  and also support a local direct-seed list. (Message build + parse are done;
  wire the multicast socket.)
- **UPnP/SOAP control** against `:1400`: AVTransport (play/pause/next/seek,
  `SetAVTransportURI`, queue ops), RenderingControl (volume/mute/bass/treble),
  GroupRenderingControl (group volume), ZoneGroupTopology (group structure),
  ContentDirectory (browse favorites/queue), MusicServices + SystemProperties
  (service accounts), DeviceProperties, and (S2) Queue. Envelope + SOAPACTION
  builders are done; wire dispatch via `Transport` over the asupersync client,
  and write per-action request/response (un)marshaling.
- **GENA eventing.** SUBSCRIBE with a callback URL to an asupersync HTTP sink;
  parse NOTIFY `LastChange` docs into typed state deltas; renew before timeout;
  UNSUBSCRIBE on shutdown. (Header build + timeout parse are done.)
- **DIDL + URIs.** DIDL-Lite metadata and the `x-sonos-spotify:` URI. Critically
  the `sid`/`flags`/`sn` and the `SA_RINCON<type>…` descriptor and item-id
  prefix are **per-household** and are discovered from the household's own
  favorites via ContentDirectory — never hard-coded. (Builder takes them as
  params; add the "learn params from favorites" routine.)

Reference ground truth: svrooij **sonos-api-docs**, SoCo, node-sonos(-ts),
Home Assistant `sonos`. Port behavior, not code wholesale (respect licenses).

### LANE B — `fsonos-core` (the brain)

- **Inventory & classification.** Fetch/parse device descriptions, classify
  S1/S2, reconcile into `HouseholdState` (players + groups). (Model + a model
  classifier exist.)
- **Topology & grouping.** Maintain live group structure from ZoneGroupTopology
  events; implement group/ungroup/join/leave and stereo-pair awareness via
  AVTransport `SetAVTransportURI` to `x-rincon:<coordinator-uuid>`.
- **Control orchestration.** High-level intents → coordinator resolution →
  `fsonos-proto` calls. One place that knows "play X on room Y" means "find Y's
  coordinator, set its URI, Play".
- **Durable store (fsqlite).** `Store` trait exists with a `MemStore`; add
  `SqliteStore`: device cache, Spotify library cache, play history (for DJ
  anti-repeat), and the learned per-household Spotify render params. Schema +
  migrations; short WAL transactions; never hold a txn across network I/O.
- **Reconciliation loop.** Periodic + event-driven refresh of inventory/topology
  with backoff; health of each player.

### LANE C — `fsonos-spotify` (library + DJ)

- **Spotify Web API client**, read-only on the owner's **own** library:
  Authorization Code + **PKCE** (helper shape exists), scope
  `user-library-read`; endpoints: saved albums, liked/saved tracks, and track
  metadata. Tokens cached in the local git-ignored auth cache. HTTPS over the
  asupersync client. **The Web API never starts playback** (that is Sonos via
  SMAPI; see §6) — it only *reads taste*.
- **Classical DJ engine** (pure, testable; anti-repeat selector exists). Build
  a candidate pool of the owner's classical tracks (genre/metadata heuristics +
  saved-albums), then pick for pleasant variety: spread across
  composers/periods/works, respect energy/time-of-day, avoid recent repeats
  (via `Store` history). Feed the coordinator's queue ahead of track end
  (driven by GENA transport events from Lane B).
- **The "enqueue a Spotify track on Sonos" path** is the crux the prior attempt
  got stuck on: it requires the correct `x-sonos-spotify:` URI **and** the
  byte-right DIDL `desc`/item-id for *that household*, learned from its own
  favorites (Lane A/B). Lane C provides the track ids; Lane A/B render them.

### LANE D — `fsonos-api` + `fsonos-mcp` + `fsonos-cli` (surfaces)

- **HTTP API (fastapi_rust).** `GET /zones`, `GET /zones/{room}`,
  `POST /play`, `POST /pause|resume|next|previous`, `POST /volume`,
  `POST /group`, `POST /ungroup`, `POST /dj/{start|skip|stop}`,
  `GET /health`, `GET /openapi.json`. DTOs exist. Bind loopback + the Tailscale
  interface. Clear JSON errors (`{"detail":…}`).
- **MCP server (fastmcp_rust).** Tools: `list_zones`, `play`, `pause`,
  `resume`, `next`, `set_volume`, `group`, `ungroup`, `dj_start`, `dj_skip`,
  `dj_stop`. Served over stdio (for a local agent) and streamable HTTP (for
  tailnet agents). Room-name matching normalizes curly apostrophes (helper
  exists) and is case-insensitive.
- **CLI + `serve` daemon.** `fsonos` subcommands (scaffolded) become real; the
  `serve` command wires: discovery → GENA sink → store → HTTP API → MCP server
  → DJ, under one asupersync runtime, with graceful shutdown. A launchd plist
  template (in `docs/`) runs it at login and restarts on crash.
- **Tailscale.** Document the deployment: the daemon binds the tailnet
  interface (or uses Tailscale Serve) so tailnet agents reach it; speakers stay
  on the LAN. ACLs restrict who can reach the daemon. No Tailscale on speakers.

---

## 6. Spotify on Sonos — the hard truth (so no one re-learns it painfully)

- **Spotify Web API cannot start playback on a Sonos.** Spotify Connect targets
  Spotify-certified endpoints; the Web API's "transfer playback" won't drive a
  Sonos renderer. So the Web API is used **only to read the owner's library**.
- **Sonos renders Spotify itself via SMAPI** (the Sonos Music API integration).
  To enqueue a track you hand AVTransport an `x-sonos-spotify:` URI plus
  DIDL-Lite metadata whose `desc` names the Spotify service account
  (`SA_RINCON<service_type>_X_#Svc<service_type>-0-Token`) and whose item id has
  the household's expected prefix. These values are **per-household** (the prior
  attempt confirmed one household used `sid=12&flags=8224&sn=1`, service type
  3079). **Learn them from the household's own favorites**, don't hard-code.
- **S1 vs S2 differ** in music-service account plumbing. Spotify Premium must be
  linked in the respective app once; the daemon then reuses that linkage. Treat
  S1 and S2 render-param discovery independently.
- **Fallback sources** (if a given track resists SMAPI enqueuing): the owner's
  saved content via ContentDirectory favorites/playlists, or TuneIn/local — but
  the primary path is Spotify-via-SMAPI as above.

---

## 7. Data model & store schema (initial)

`fsqlite` tables (SQL, created by migrations in Lane B):

- `players(id TEXT PK, household TEXT, room TEXT, ip TEXT, model TEXT, generation TEXT, last_seen INT)`
- `groups(coordinator TEXT, member TEXT, household TEXT, updated INT)` (edge list)
- `spotify_library(source_uri TEXT PK, title TEXT, artist TEXT, album TEXT, is_classical INT, added INT)`
- `play_history(id INTEGER PK, zone TEXT, source_uri TEXT, played_at INT)`
- `render_params(household TEXT PK, sid INT, flags INT, sn INT, cdudn TEXT, item_id_prefix TEXT, learned_at INT)`
- `auth(service TEXT PK, refresh_token TEXT, expires INT)` — local only; the DB
  file lives under the git-ignored data dir.

Keep raw tokens and site data out of git; the store file is in the OS data dir.

---

## 8. Testing strategy

- **Pure unit tests** (today, no network): SSDP build/parse, SOAP envelope +
  SOAPACTION, GENA headers/timeout, DIDL/URI build, model classification,
  coordinator resolution, DJ anti-repeat, room normalization. Keep these green.
- **Protocol fixtures:** capture *real* response bodies from the owner's own
  players (device descriptions, ZoneGroupTopology, favorites, LastChange) into
  `crates/*/tests/fixtures/` **with site-specific identifiers scrubbed**, and
  assert the parsers against them (golden tests).
- **Integration (opt-in, local):** a `--features live-tests` lane that runs
  against the real LAN, gated behind an env flag so CI never needs the network.
- **DJ behavior:** seeded-RNG determinism; variety metrics over a long run.
- **No mocks for the franken stack** where a real localhost socket/db will do:
  bind a real asupersync HTTP server for GENA tests; open a real fsqlite
  `:memory:`/tempfile for store tests.

Gates after substantive Rust changes: `cargo fmt --check`,
`cargo check --workspace --all-targets`, `cargo clippy --workspace -- -D warnings`,
`cargo test --workspace` (via `rch`).

---

## 9. Milestones

- **M0 — Foundation (done):** green workspace skeleton, pure-logic stubs +
  tests, docs, beads graph, GitHub repo.
- **M1 — `FND-DEPS`:** franken deps wired; `cargo check` green with asupersync
  unified; fsqlite round-trip proven; a trivial asupersync HTTP GET + an MCP
  stdio echo tool run.
- **M2 — See & control:** discovery + topology + direct play/pause/volume work
  against the real LAN from the CLI (Lanes A+B).
- **M3 — Spotify render:** learn per-household render params from favorites and
  reliably enqueue a Spotify track on both S1 and S2 (the prior blocker).
- **M4 — DJ:** library read + DJ engine + queue feeding; `fsonos dj start`
  delivers a varied classical set that keeps going.
- **M5 — Surfaces & daemon:** HTTP API + MCP server + `serve` + launchd +
  Tailscale deployment; agents control the house end-to-end.
- **M6 — Reliability polish:** event resubscription robustness, reconnection,
  backoff, health, golden fixtures for both households.

---

## 10. Beads mapping & the swarm

The work decomposes into a beads graph (see `br ready`): a root doctrine bead,
`FND-DEPS` (blocks franken-dependent work), and the four lanes with per-lane
sub-beads whose pure-logic parts are unblocked immediately. A multi-agent swarm
(four Opus-class Claude Code agents, one per lane) implements under the
code-first / batch-verify doctrine in `AGENTS.md`: real code + real tests,
syntax/format gate, commit; the orchestrator runs the verifying `cargo` pass and
closes beads with cited evidence. Agents never take on out-of-scope work
(`docs/SCOPE.md`); the firmware/RE track is not in the swarm's backlog.

---

## 11. Review loop (how this plan improves)

Per the planning-workflow method: paste this plan into a strong reasoning model
("Carefully review this entire plan … git-diff style changes …"), integrate the
revisions here, and repeat ~4–5 rounds to steady-state before/during
implementation. Keep the plan the living source of truth; when code and plan
disagree, reconcile at the boundary and update this file.
