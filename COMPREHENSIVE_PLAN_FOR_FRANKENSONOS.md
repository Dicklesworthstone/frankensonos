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
2. **A real "DJ"** — an agent that plays a pleasant, varied stream of whatever
   music the owner likes, learned from the owner's *own* Spotify account
   (liked tracks, saved albums, followed artists, playlists, top artists and
   tracks, recent listening) and overridden by preferences the owner sets by
   hand (genres, artists, eras or moods to favor or avoid, energy, explicit
   content), with sensible variety and anti-repeat, controllable by voice/agent
   with no phone in hand. The DJ is not tied to a genre: it spans whatever the
   owner's library spans. Classical music is one case it handles with care
   (multi-movement works play whole and in order, §12.1), not its premise.
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
2. **Direct control.** `fsonos play "Living Room" spotify:track:...`,
   `fsonos pause "Kitchen"`, volume/group/ungroup — all fast and correct,
   addressing the **coordinator** of the target group automatically.
3. **The DJ.** `fsonos dj start "Living Room"` begins a varied set of the
   music the owner likes on Spotify, shaped by the owner's own preference
   overrides; `dj skip` advances; the daemon keeps the queue fed and avoids
   recent repeats. An agent can do the same via MCP:
   `dj_start`, `dj_skip`, `play`, `set_volume`, `list_zones`.
4. **Always-on daemon.** `fsonos serve` runs a long-lived process that holds
   live state via GENA event subscriptions (no polling storms), exposes the
   HTTP API and the MCP server, and keeps the DJ running. Managed by launchd on
   the Mac mini that sits on the speaker LAN.
5. **Off-LAN via Tailscale.** An agent on any tailnet device reaches the daemon
   over Tailscale; the daemon is the only thing fronted — the speakers never
   leave the LAN.
6. **Diagnose and set up.** `fsonos doctor` says what is broken and how to fix
   it; `fsonos setup` takes a fresh install to first playback (§12.2).
7. **Agents read before they act.** Zone state, favorites, library search, and
   recent plays are tools too, and every error carries a code and a fix
   (§12.3).
8. **Guardrails.** Volume caps, quiet hours, an action log, and undo apply to
   every agent action on every surface (§12.5).

---

## 2. Environment (ground truth for this deployment)

- **Build/runtime host:** a Mac mini (macOS, arm64) that sits directly on the
  speaker LAN and is also on the tailnet, so the daemon reaches every player
  locally and is reachable by off-LAN agents over Tailscale. This is the
  machine the daemon runs on.
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

FrankenSonos is built on the owner's Rust libraries. Bead `FND-DEPS` wired
them and proved each with a real test; the exact lines live in the workspace
`Cargo.toml`:

| Library | Role in FrankenSonos | Source (wired + proven) |
|---|---|---|
| **asupersync** `0.5` | async runtime: TCP/UDP(+multicast), HTTP/1.1 client+server, TLS, timers, structured concurrency, cancellation (`Cx`) | crates.io `"0.5"` → `0.5.0`, feature `tls-webpki-roots` |
| **fsqlite** (frankensqlite) `0.4.9` | durable local store (device/library caches, play history, DJ + render params) | crates.io `=0.4.9`, feature `async-api` |
| **fastmcp** (fastmcp_rust) `0.10` | MCP server (stdio + streamable HTTP) | git `fastmcp_rust@03b5274…` (`fastmcp-rust` + `fastmcp-server`) |
| **fastapi** (fastapi_rust) `0.4` | HTTP control API | git `fastapi_rust@cb9d729…`, `default-features = false` |
| **rano** | *human-operated* LAN diagnostic only; NOT a build dep | n/a |

**Integration hazards (each found and resolved empirically in `FND-DEPS`):**

1. **One asupersync.** fastmcp pins `=0.5.0`; fastapi and fsqlite 0.4.9 require
   `0.5.0`; `asupersync = "0.5"` resolves the whole graph to that single
   version, so `Cx` is one type everywhere. Never path/git-dep asupersync (the
   local checkout is `0.6.0` and would fork the type universe).
2. **fsqlite comes from crates.io `0.4.9`, not git `2633b38`.** That rev is
   fsqlite `0.3.18` on asupersync `0.4.10` — am_baseline patches it in only for
   its embedded beads engine; its mailbox DB runs registry `0.4.9`, which shares
   asupersync `0.5.0`. No `bundled` feature exists (from-scratch engine).
3. **fsqlite engine futures overflow a default thread stack** (stack overflow
   in the first round-trip run). Either `Box::pin` the raw `!Send`
   `Connection` futures and drive them on a 32 MiB thread, or use
   `AsyncConnection` (`async-api`): a `Send` handle over fsqlite's own 32 MiB
   worker thread whose `*_sync` methods fit the synchronous `Store` trait.
4. **fastmcp `#[tool]` needs `fastmcp-server` as a direct dependency**: its
   expansion names `::fastmcp_server::promote_legacy_tool_content` by absolute
   path at this rev.
5. **fastapi `#[get]`/`#[post]` macros cannot be used**: they emit
   `#[allow(unsafe_code)]` for a Linux `link_section` route registry, which is
   incompatible with the workspace `forbid(unsafe_code)`. Register handlers with
   `App::builder().get(path, handler)`.
6. **asupersync's h1 server rejects every request by default**
   (`HostPolicy::RejectUnknown` → 421 Misdirected Request). Each listener — the
   GENA sink above all — must set `Http1Config::host_policy` to the Host values
   it serves (the address the players are given in the SUBSCRIBE callback).
7. **No `bundled`, no Tokio, no reqwest.** All networking is asupersync (tokio
   is in `Cargo.lock` only via asupersync's wasm32 target).

**Real API shapes** (verified against the local library sources — use these,
don't reinvent):

- Runtime: `asupersync::runtime::RuntimeBuilder::current_thread().with_reactor(asupersync::runtime::reactor::create_reactor()?).blocking_threads(0,16).build()?` then `rt.block_on(async { … })`. Inside, get the context with `asupersync::Cx::current()`.
- UDP multicast (SSDP): `asupersync::net::UdpSocket::bind(addr).await`, then `sock.join_multicast_v4(Ipv4Addr::new(239,255,255,250), Ipv4Addr::UNSPECIFIED)?`, `send_to`, `recv_from`.
- HTTP client (SOAP/Spotify): `asupersync::http::Client::default_for_runtime(cx)`, `.post(url).header(..).body(..).send(cx).await` → `resp.status: u16`, `resp.body: Vec<u8>`.
- HTTP server (GENA callback sink): `Http1Listener::bind_with_config(addr, handler, Http1ListenerConfig::default().http_config(Http1Config::default().host_policy(HostPolicy::allow_list(hosts)))).await?` (`HostPolicy` is `asupersync::http::h1::server::HostPolicy`), then `.run(&rt.handle()).await`; stop with `listener.shutdown_signal()`. Proof: `crates/fsonos-proto/tests/asupersync_http.rs`.
- fsqlite: `fsqlite::Connection::open(path).await?`, `.execute_with_params(sql, &[SqliteValue::…]).await?`, `.query(sql).await?` → `Vec<Row>`, `row.get(i) -> Option<&SqliteValue>`, `.close().await`; placeholders `?1,?2`. `Connection` is `!Send` — box its futures, run on a 32 MiB thread. Or `fsqlite::AsyncConnection::open_sync(path)?` with `execute_sync` / `execute_with_params_sync` / `query_sync` / `close_sync`. Proof: `crates/fsonos-core/tests/fsqlite_roundtrip.rs`.
- fastmcp: `#[tool(description="…")] async fn play(ctx:&McpContext, args…) -> McpResult<String>` (with `use fastmcp::prelude::*`); build with `fastmcp::auto::server_builder(name, ver).tool(Play).build()`; run `.run_stdio_with_cx(&cx).await` (never returns). Proof: `crates/fsonos-cli/tests/mcp_stdio.rs` (`fsonos mcp`, initialize → tools/list → tools/call `echo`).
- fastapi: `fn health(_: &RequestContext, _: &mut Request) -> Ready<Response>` returning `Response::json(&dto)`; `App::builder().get("/health", health).build()`; serve with `TcpServer::new(ServerConfig::new(addr)).serve_on_app(&cx, listener, Arc::new(app))` (or `fastapi::serve(app, addr)`). Proof: `crates/fsonos-api/tests/health.rs`.

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

- **Spotify Web API client**, read-only on the owner's **own** account:
  Authorization Code + **PKCE** (helper shape exists). Today: scope
  `user-library-read`; endpoints: saved albums, liked/saved tracks, album track
  lists, and track metadata. Planned for the taste model (§12.1): followed
  artists (`user-follow-read`), the owner's playlists
  (`playlist-read-private`), top artists and tracks (`user-top-read`), recently
  played tracks (`user-read-recently-played`), and artist genre tags where the
  Web API returns them. Tokens cached in the local git-ignored auth cache.
  HTTPS over the asupersync client. **The Web API never starts playback** (that
  is Sonos via SMAPI; see §6) — it only *reads taste*.
- **DJ engine** (pure, testable; anti-repeat selector exists). Build a
  candidate pool from the owner's own taste signals, whatever genres they span,
  apply the owner's manual preference overrides and any active steering, then
  pick for pleasant variety: spread across artists, albums and genres (and
  across composers, periods and works when the music is classical), respect
  energy/time-of-day, avoid recent repeats (via `Store` history). Feed the
  coordinator's queue ahead of track end (driven by GENA transport events from
  Lane B). *Current state:* the shipped pool is classical-only (it keeps tracks
  whose metadata scores as classical and drops explicit tracks); generalizing
  it is planned work, tracked in §12.1.
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

`fsqlite` tables (SQL, created by the append-only migration list in
`fsonos-core/src/store/sqlite.rs`; applied versions are recorded in
`schema_migrations(version INTEGER PK, name TEXT, applied_at INT)`):

- `players(id TEXT PK, household TEXT, room TEXT, ip TEXT, model TEXT, generation TEXT, last_seen INT)`
- `zone_groups(id INTEGER PK, household TEXT, coordinator TEXT, member TEXT, updated INT)` (edge list; `id` keeps group order; not `groups`, an SQL keyword)
- `spotify_library(source_uri TEXT PK, title TEXT, artist TEXT, album TEXT, duration_secs INT, is_classical INT, added INT)` — `is_classical` is the current implementation's DJ-candidacy flag (only rows judged classical enter the pool); it is classical-oriented, and replacing it with genre-agnostic candidacy is planned work (§12.1, below)
- `play_history(id INTEGER PK, zone TEXT, source_uri TEXT, played_at INT)`
- `render_params(household TEXT PK, sid INT, flags INT, sn INT, cdudn TEXT, item_id_prefix TEXT, learned_at INT)`
- `auth(service TEXT PK, refresh_token TEXT, expires INT)` — local only; the DB
  file lives under the git-ignored data dir.

Added by §12 (each table is created by the migration of the bead that needs it):

- `spotify_library` gains `album_uri TEXT, album_artists TEXT, origin TEXT, disc_number INT, track_number INT` (migration 2) and `work_key TEXT` (migration 3) (whole-work DJ, §12.1).
- `dj_sessions(coordinator TEXT PK, mood TEXT, constraints TEXT, expires INT)` (migration 3; steering survives restarts; keyed by coordinator UUID because zone names change with grouping; `mood`/`constraints` nullable, expired rows kept until deleted).
- `album_tracks(album_uri TEXT, disc_number INT, track_number INT, source_uri TEXT, title TEXT, duration_secs INT, fetched_at INT, PK(album_uri, disc_number, track_number))` (migration 3; completes liked movements into whole works; not part of the library; replaced per album).
- `feedback(id INTEGER PK, at INT, work_key TEXT, composer_key TEXT, performer TEXT, signal INT)` (migration 3; indexed by each key with `at`, queried by key over a time window; §12.9).
- `actions(id INTEGER PK, at INT, client TEXT, surface TEXT, intent TEXT, decision TEXT, result TEXT, before_state TEXT)` (§12.5; denied requests are logged too).
- `scenes(name TEXT PK, spec TEXT, updated INT)` (§12.7).
- `schedules(id INTEGER PK, spec TEXT, action TEXT, enabled INT, last_fired INT)` (§12.10).

Planned, not yet migrated (the genre-agnostic DJ, §12.1): `spotify_library`
gains the track's genre tags, which taste sources it came from (liked, saved
album, followed artist, playlist, top item, recent play), and a taste weight,
and stops using `is_classical` as the candidacy test; `feedback` gains
`artist_key` and `album_key` beside the classical keys; the other taste reads
(followed artists, playlists, top items, recent plays) are cached the same way
the library is.

Policy (`policy.toml`), aliases (`aliases.toml`), and DJ moods (`moods.toml`)
are hand-edited TOML in the data dir, not tables. The DJ's persistent
preference overrides (§12.1, planned) are too (`preferences.toml`).

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
- **DJ behavior:** seeded-RNG determinism; variety metrics over a long run,
  on a mixed-genre library (say pop, jazz, hip-hop and classical) as well as
  an all-classical one: the pool spans every genre the taste signals cover,
  manual overrides beat learned weights, and multi-movement works stay whole.
- **Virtual household (§12.4):** `fsonos-sim` serves S1 and S2 players over
  real localhost sockets, built from the scrubbed fixtures. The e2e suite in
  `tests/e2e/` runs the §1 workflows (all but the real-tailnet path) through the CLI, HTTP API, and MCP
  against it, logging each command, response, and sim-side SOAP exchange with
  timestamps. This is how the swarm verifies M2–M5 without the speaker LAN; the
  live-LAN lane stays the owner's final check.
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
  delivers a varied set drawn from the owner's own taste (Spotify likes and
  saves, plus the owner's manual preferences) that keeps going.
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

---

## 12. User-value expansion (idea-wizard round, 2026-10-06)

§5 delivers the original contract. This round asked what would make
FrankenSonos clearly better to live with every day, for the owner and for the
agents acting for them, without leaving `docs/SCOPE.md`. About thirty candidates
were weighed; the fifteen below survived, ranked by expected user value. Each is
tracked as beads labeled `idea-wizard`; bead slugs are in brackets. The beads
carry the full design, risks, and acceptance criteria.

### 12.1 A DJ built on the owner's own taste: whole works, reasons, steering

[`c-dj-taste`, `c-dj-works`, `c-dj-work-select`, `c-dj-work-expand`,
`c-dj-steer`, `d-dj-explain`]

The DJ plays what the owner likes, whatever that is. Its taste comes from the
owner's own Spotify account and from preferences the owner states by hand, not
from a genre the DJ was built around.

- **Taste signals (read-only, the owner's own account).** Liked tracks and
  saved albums (`user-library-read`); followed artists (`user-follow-read`);
  the owner's playlists (`playlist-read-private`); top artists and tracks
  (`user-top-read`); recently played tracks (`user-read-recently-played`).
  Each source has a weight (a liked track counts for more than a track on a
  followed playlist; heavy recent rotation counts toward taste but also toward
  "heard lately"). Genre tags come from the artists where the Web API returns
  them, and from metadata heuristics where it does not. The pool spans every
  genre these signals cover. The extra scopes are requested at sign-in, and
  `docs/SCOPE.md` lists them when they land.
- **Manual preference overrides win.** Persistent, owner-wide preferences in
  `preferences.toml` (data dir, beside `moods.toml`), with matching CLI, HTTP
  and MCP verbs: genres, artists, eras and moods to favor or avoid; a default
  energy; whether explicit tracks are allowed; specific artists, albums or
  tracks to pin or ban. Precedence, strongest first: an active `dj steer` on
  the group (until it expires or is cleared), then the persistent preferences,
  then learned feedback (§12.9), then the raw account signals. A hard "avoid"
  is never outweighed by a learned or account weight.
- **Selection is genre-agnostic.** Variety and anti-repeat are keyed by artist,
  album, track and genre tag (and by composer, period and work when the music
  is classical). Time-of-day energy, `PickReason`s, steering and feedback use
  the same keys; energy comes from metadata that applies across genres, with
  tempo markings as one more input for classical movements.
- **Multi-movement works stay whole.** Classical tracks on Spotify are
  movements. A DJ that picks tracks independently plays a scherzo, then an
  aria, then the finale of a different symphony, which is the most common way
  algorithmic classical radio goes wrong. When the owner's library includes
  classical music, the DJ's unit for it is the work: every movement of one
  recording (same album, same `work_key`), in disc and track order. `Track`
  gains `disc_number`, `track_number`, and `album_uri`; `LibraryItem` gains the
  two numbers, which the Web API client already parses. Other music is picked
  track by track.
- A liked single movement is completed into its whole work by reading the
  album's track list (`GET /v1/albums/{id}/tracks`, read-only) and caching it.
- Works longer than `max_work_minutes` (default 75: most complete operas
  and Passions) are left out unless the mood allows them. A parsed work is never
  split; titles that don't parse fall back to single-track units.
- Each pick carries a `PickReason` (artist or composer not heard in N days,
  genre or period balance, time-of-day energy, mood match, preference match,
  feedback weight). `fsonos dj status` shows what is playing, why, and what
  comes next.
- Steering is structured constraints, not free text: include/exclude
  artists, genres and eras, and for classical music composers, periods, and
  form or instrumentation keywords (piano, organ, choral, opera), maximum
  length, and energy bias, with an optional expiry (`--for 2h`). Named moods
  (`focus`, `dinner`, `sunday-morning`, `bright`, `calm`) are presets in
  `moods.toml`. Steering applies to one group's session; the preferences above
  apply to every session. Agents turn language into these arguments through
  MCP `dj_steer`; the daemon never parses natural language.

**Current state (2026-10-08): the shipped DJ is classical-oriented.**
Generalizing it as described above is planned work (bead `c-dj-taste`). What
exists today:

- The pool is classical-only. `classical::CandidatePool::build` keeps a track
  when its metadata (composer credits, catalogue numbers, form and tempo
  words, key signatures, genres, label) clears a classical score, or when most
  of its album does, and drops explicit tracks; the library cache stores that
  verdict in `spotify_library.is_classical`, and only those rows become
  candidates. A library with no classical music gives the DJ nothing to play.
- Only liked tracks and saved albums are read (scope `user-library-read`, plus
  album track lists for whole works). Followed artists, playlists, top items
  and listening history are not read yet.
- Variety, energy (estimated from tempo markings), steering (`--composer`,
  `--not-composer`, `--period`, `--with`/`--without` form keywords) and
  feedback rows (keyed by `work_key`, `composer_key` and `performer`) all use
  classical metadata.
- The only overrides are `dj steer` (per group, for a while or until cleared)
  and `moods.toml`; there are no persistent owner-wide preferences yet.
- Whole works in order, the long-work policy, album expansion, `PickReason`s,
  moods and feedback learning are implemented and stay; the generalization
  extends them rather than replacing them.

### 12.2 `fsonos doctor` and `fsonos setup`

[`b-doctor-engine`, `b-doctor-lan-checks`, `b-doctor-render-checks`,
`d-doctor-surface`, `d-setup-wizard`]

The prerequisites that make Sonos control fail are invisible: macOS Local
Network permission (multicast fails silently under launchd), multicast filtered
by mesh Wi-Fi, a firewall that stops players reaching the GENA callback,
Spotify not linked in one household's app, no Spotify favorite in a household
(so render params can't be learned), an expired token, a bind address the guard
refuses. Today each of these shows up as "nothing happens".

- A check registry in `fsonos-core`: each check has an id and prerequisites and
  returns pass, warn, fail, or skip with detail, evidence, and a concrete
  remedy. Checks whose prerequisite failed are skipped, not failed.
- The checks: data dir and store; SSDP responders per household; seeds; each
  player's `:1400` description fetch and latency; S1/S2 classification; a GENA
  round-trip (subscribe to one player's RenderingControl, expect the initial
  NOTIFY within 5 s, unsubscribe); Spotify linkage and learned render params per
  household; token refresh; bind-guard verdicts; daemon health. Tailnet checks
  (installed, running, logged in, tailnet IP, MagicDNS, daemon reachable over
  the tailnet) come from `ts-doctor` in the Tailscale epic, registered in the
  same engine as `tailscale.*`.
- Doctor is read-only and never changes playback. Output is a table with
  remedies or `--json`, with exit codes 0/6/7 for pass/warnings/fail (1-5 are the CLI's error codes). It is also MCP
  `doctor` and `GET /doctor`.
- `fsonos setup` runs the same checks in order on first run and stops at each
  failure with the fix ("In the S1 app, add any Spotify track to My Sonos, then
  press Enter"), runs the PKCE login, and offers to write `seeds.toml` when
  multicast fails but players answer directly.

### 12.3 State and read tools for agents, and errors that say what to do

[`a-soap-reads`, `b-now-playing`, `b-favorites-search`, `d-agent-read-tools`,
`d-error-codes`]

The §5 MCP tool list is nearly all writes. "Turn it down a bit in the kitchen",
"what's playing?", and "play that Brahms from yesterday" all need a read first.

- proto: `GetPositionInfo`, `GetTransportInfo`, `GetMediaInfo`, `GetVolume`,
  `GetMute`, `GetGroupVolume`, with the current track's DIDL parsed into title,
  composer, album, art URL, and duration.
- core: per-zone playback state in `HouseholdState`, updated from GENA deltas,
  with position interpolated from a timestamp.
- Tools and routes: `get_zone_state`, `list_favorites`, `play_favorite`,
  `search_library`, `recent_plays`, and `set_volume` with a relative `delta`.
  MCP resources `sonos://zones`, `sonos://zones/{room}`, and `sonos://dj` with
  subscriptions (fastmcp's `ServerBuilder` has `resource`, `resource_template`,
  and `resource_subscriptions`).
- Errors gain a stable `code` (`UNKNOWN_ROOM`, `AMBIGUOUS_ROOM`,
  `PLAYER_UNREACHABLE`, `SPOTIFY_NOT_LINKED`, `RENDER_PARAMS_MISSING`,
  `POLICY_DENIED`, …), a `hint`, and `suggestions` such as the closest room
  names. The same codes appear in HTTP, MCP, and CLI exit codes.

### 12.4 A virtual household for hardware-free testing and demos

[`a-sim-core`, `a-sim-gena`, `a-sim-discovery`, `d-e2e-sim`,
`d-e2e-core-scenarios`, `d-sim-subcommand`]

The swarm builds on `rch` workers that are not on the speaker LAN, so every
M2–M5 criterion that says "against a real player" can only be checked by the
owner by hand. `crates/fsonos-sim` serves virtual S1 and S2 players over real
localhost sockets, which is what AGENTS.md asks for in place of mocks.

- Each virtual player has its own localhost port, a device description from the
  scrubbed fixtures, and state machines for AVTransport (URI, queue, transport
  state, position clock), RenderingControl, ZoneGroupTopology (including
  `x-rincon:` joins), and ContentDirectory favorites with synthetic Spotify
  items.
- GENA: SUBSCRIBE, renew, and UNSUBSCRIBE, and a NOTIFY with `LastChange` on
  every state change.
- Discovery goes through a generated seeds file, plus a unicast SSDP responder
  where loopback allows it. Nothing relies on multicast over loopback.
- Fault injection: latency, dropped NOTIFYs, UPnP faults, reboots (lost
  subscriptions), port changes (standing in for DHCP IP changes), coordinator
  re-election.
- `fsonos sim --scenario two-households` lets a new user or an agent developer
  try every command before pointing it at a real house.
- The same e2e scenarios run against the owner's real players with
  `FSONOS_E2E_LIVE=1`: volume capped at 15, every touched zone snapshotted and
  restored, logs under the git-ignored `target/`. The owner's final live check
  is one command, never part of CI.
- The sim implements only the community-documented control surface, from our
  own scrubbed fixtures. It is a test double for our own client.

### 12.5 Safe agent control: limits, fades, quiet hours, an action log, undo

[`b-snapshot`, `b-policy`, `b-volume-ramps`, `b-action-log-undo`,
`d-safety-surfaces`]

With several agents able to drive the house from anywhere, the likely failure is
an agent doing something careless at 2 a.m. at volume 80. People will only hand
the house to agents they can bound and reverse. The guardrails live in
`fsonos-core` orchestration so every surface gets them.

- `policy.toml`: per-room `max_volume`, a `max_step` per call, quiet hours with
  a lower cap, fade durations, and per-client tool allowlists. Over-limit volume
  is clamped and the response says so; a disallowed tool returns
  `POLICY_DENIED`.
- Client identity is a surface plus a principal: `cli` (a person at the
  terminal), `mcp-stdio` (a local agent), `loopback-http` (a local process), or
  a tailnet principal. The daemon binds the tailnet directly by default
  (`ts-autobind`), so a tailnet peer is identified by Tailscale WhoIs on its
  address (`ts-identity`), which can't be forged; this needs the peer address
  to reach the handlers, which the pinned fastapi and fastmcp don't pass yet.
  Behind Tailscale Serve, the `Tailscale-User-Login` header names the user, but
  Serve omits it for tagged devices, so Serve gets its own upstream port and a
  header-less request there is `unknown`. An unresolved identity never fails a
  request. Caps apply to every identity except `cli`; `unknown` gets read-only
  tools. Policy keys are logins, tags, or node names.
- Fades with a duration use stepped `SetVolume`. RenderingControl
  `RampToVolume` takes no duration and only its SLEEP_TIMER type fades from the
  current volume, so it serves only "fade at device speed".
- A zone snapshot (group membership, volumes, mute, transport URI and metadata,
  track and position, play state) is taken before every mutating intent, and
  each action is logged with client, surface, intent, result, and before-state.
  `fsonos log`, `fsonos undo`, MCP `recent_actions` and `undo_last`. Undo says
  what it could not restore (for example, a queue that was replaced since).

### 12.6 Favorites and library search as play sources

[`b-favorites-search`] A Sonos favorite carries a working URI and DIDL for its
own household, so playing one needs no render-param learning. That makes it the
earliest real playback on both S1 and S2, and the fallback §6 calls for when a
Spotify track won't enqueue. Library search is token-scored fuzzy matching over
the cached library and favorites (title, composer, performer, album). The
surfaces are in 12.3.

### 12.7 Scenes

[`b-scenes`, `d-scenes-surface`] Named, declarative house states: group layout,
per-room volume, and a source (a favorite, a Spotify URI, or the DJ with a
mood). `scene apply` diffs current state against the scene and issues only the
operations needed. One call replaces five to ten, and it matches how people
describe what they want ("dinner", "work").

### 12.8 An instant CLI through the daemon

[`d-cli-daemon-client`] The CLI uses a running daemon's warm state when one
answers (`zones` in under 100 ms) and otherwise falls back to direct LAN mode
with the cached inventory. Every command takes `--json`; shell completions
complete room names; `--daemon` and `--direct` force a mode.

### 12.9 A DJ that learns

[`c-dj-feedback`, `d-dj-feedback-surface`] `dj like` and `dj dislike`, early
skips (under 30 s) as a negative signal, and full listens as a weak positive,
keyed by track or work, artist and album (and by composer and performer for
classical works), nudging the genres they carry, and decaying over weeks.
Learned feedback ranks below the owner's manual preferences and above the raw
account signals (§12.1). Today the `feedback` rows carry only the classical
keys (`work_key`, `composer_key`, `performer`); the artist and album keys come
with the genre-agnostic DJ (`c-dj-taste`). Time-of-day programs set the
default mood when none is given.

### 12.10 Schedules and a sleep timer

[`b-scheduler`, `d-schedule-surface`] Daemon-side and persisted:
`fsonos sleep Bedroom 45m` fades out and pauses;
`schedule add "weekdays 07:30" dj start Kitchen --mood bright` or a scene.
Device alarms can't start the DJ or a scene and differ between S1 and S2, so the
daemon owns scheduling. A run missed by more than 10 minutes is skipped.

### 12.11 Move playback and whole-house mode

[`b-move-party`, `d-house-verbs`] `fsonos move Kitchen Bedroom` joins the
target to the source's group, waits for the topology event, then removes the
source, so the track and position carry over and the DJ session follows.
`party` groups every room in a household. S1 and S2 players cannot be grouped
together, and the command says so instead of failing quietly.

### 12.12 Announcements

[`b-announce`, `d-announce-surface`] Snapshot, set a policy-capped announcement
volume, play a clip, wait for STOPPED via GENA, restore. Speech comes from macOS
`say` (WAV); chimes are generated, not copied. Clips are served from the
GENA sink listener, which the speakers can already reach on the LAN; control
endpoints never move onto that listener.

### 12.13 Live events and a web remote

[`d-events-sse`, `d-web-remote`, `d-http-hardening`] `GET /events` streams zone-state deltas, DJ
picks, and action-log entries as server-sent events (fastapi_rust has SSE
support). The API is unauthenticated, so before any browser page uses it
(`d-http-hardening`) every listener checks Host and Origin and accepts writes
only as JSON. A single static page served by the daemon
(rooms, now playing, volume, play/pause/skip, DJ mood, scenes) is a phone remote
over the tailnet, with no app store and no account.

### 12.14 Self-healing identity

[`b-self-healing`] Everything is keyed by player UUID, never by IP. An
unreachable player is re-resolved through SSDP or seeds and the call retried
once; a command that fails with UPnP 800 refreshes topology and retries once,
but only if the coordinator actually moved (800 also means a wrong Spotify
DIDL); a reboot (lost subscription SID)
triggers resubscription. Each path is tested with sim faults.

### 12.15 Room aliases and smart targets

[`b-room-aliases`, `d-house-verbs`] `aliases.toml` (`kitchen` → "Kitchen",
`downstairs` → Kitchen + Living Room), "did you mean" suggestions on near misses (suggestions only; a typo never acts on a room), reserved
`all`/`everywhere` per household, and a per-client default room.

### Milestone placement

- **M2:** 12.3 reads and now-playing state, 12.4 sim and e2e harness, 12.6
  favorites (first real playback), 12.15, and the doctor engine with LAN checks.
- **M3:** doctor render checks and `fsonos setup`.
- **M4:** 12.1 and 12.9.
- **M5:** 12.3 surfaces, 12.5, 12.7, 12.8, 12.13.
- **M6:** 12.10, 12.11, 12.12, 12.14.

### Considered and dropped

- An MQTT / Home Assistant bridge: Home Assistant already integrates Sonos.
- Driving the players' own AlarmClock service: it can't start the DJ or a
  scene, and S1 and S2 differ.
- An always-on record-and-scrub mode in the daemon for generating fixtures:
  fixtures come from deliberate requests reviewed by hand (§8), and a recorder
  running in the background is too likely to commit site data.
- Natural-language parsing in the daemon: agents do this better. The daemon
  takes structured arguments.
