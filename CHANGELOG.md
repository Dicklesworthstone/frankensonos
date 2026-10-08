# Changelog

All notable changes to FrankenSonos are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) once it
cuts its first release.

**Scope & method.** This log is reconstructed from the project's real git
history (`git log`), the beads task graph in `.beads/`, and the committed source.
There are no tags or GitHub Releases yet — the project is pre-`0.1.0` and under
active development by a multi-agent build swarm, so everything lives under
[Unreleased](#unreleased). Representative commits are linked for each capability
so another agent can jump straight to the evidence. Capability claims reflect
landed, gated code (`cargo clippy -D warnings` + `cargo test --workspace` via the
`rch` fleet); no claim of a shipped binary or of live-hardware behavior is made
except where a commit is explicitly an owner-run, inaudible check on real players.

## [Unreleased]

Pre-release groundwork toward `0.1.0`. The `0.1.0` milestone is "see & control":
deterministic discovery, live topology, and direct play/pause/volume against a
real LAN — reachable from any device on your Tailscale tailnet — through the
`fsonos` CLI, HTTP API, and MCP server. Most of that surface area now exists in
tree and is green under the test suite; what remains is end-to-end assembly in
the daemon and verification against the owner's real households.

### Added

- **Project foundation.** A single Rust 2024 Cargo workspace of nine crates
  (`fsonos-types`, `-proto`, `-core`, `-spotify`, `-api`, `-mcp`, `-cli`,
  `-tailscale`, `-sim`), `#![forbid(unsafe_code)]` workspace-wide, a pinned
  `nightly-2026-08-31` toolchain, and a committed `Cargo.lock`. Includes the
  comprehensive plan, `AGENTS.md`, the in/out-of-scope boundary (`docs/SCOPE.md`),
  and the beads task graph.
  [`0ecb761`](https://github.com/Dicklesworthstone/frankensonos/commit/0ecb761)

- **Protocol layer (`fsonos-proto`).** SSDP `M-SEARCH` discovery; UPnP/SOAP
  envelope + fault parsing behind a `Transport` seam; `roxmltree` XML helper;
  device-description, ZoneGroupTopology and ContentDirectory (`Browse`) parsing;
  DIDL-Lite + `x-sonos-spotify:` URI construction from per-household params; a
  real LAN transport; `RampToVolume` with the three Sonos ramp types; and the
  complete GENA event catalog — AVTransport/RenderingControl/Queue `LastChange`
  plus ZoneGroupTopology/ContentDirectory/GroupRenderingControl plain-property
  events — proven against live-captured, scrubbed fixtures.
  [`635d1cf`](https://github.com/Dicklesworthstone/frankensonos/commit/635d1cf),
  [`739a8ee`](https://github.com/Dicklesworthstone/frankensonos/commit/739a8ee),
  [`9d97549`](https://github.com/Dicklesworthstone/frankensonos/commit/9d97549),
  [`60df345`](https://github.com/Dicklesworthstone/frankensonos/commit/60df345),
  [`1455e06`](https://github.com/Dicklesworthstone/frankensonos/commit/1455e06)

- **Core model (`fsonos-core`).** The two-household state model with coordinator
  resolution and S1/S2 classification; topology folded into rooms (stereo-pair /
  home-theater aware); room resolution and aliases (`all`/`everywhere`/`here`,
  fuzzy matching, suggestions); control orchestration (intent → coordinator →
  proto); group/ungroup; zone snapshot capture & restore; cancellable,
  device-speed volume fades; a house policy (volume caps, step limits, quiet
  hours, per-tool allowlists); and a durable `fsqlite`-backed `SqliteStore` with
  append-only migrations.
  [`1c0d93e`](https://github.com/Dicklesworthstone/frankensonos/commit/1c0d93e),
  [`b51455d`](https://github.com/Dicklesworthstone/frankensonos/commit/b51455d),
  [`31dfe37`](https://github.com/Dicklesworthstone/frankensonos/commit/31dfe37),
  [`cdb5308`](https://github.com/Dicklesworthstone/frankensonos/commit/cdb5308),
  [`b056658`](https://github.com/Dicklesworthstone/frankensonos/commit/b056658),
  [`23c524d`](https://github.com/Dicklesworthstone/frankensonos/commit/23c524d),
  [`005051a`](https://github.com/Dicklesworthstone/frankensonos/commit/005051a),
  [`667fc7f`](https://github.com/Dicklesworthstone/frankensonos/commit/667fc7f)

- **A live, self-healing model.** An event engine that keeps every GENA
  subscription alive; a reconcile loop that schedules surveys with jittered
  backoff and tracks per-player health; live playback state folded from NOTIFY
  events; and self-healing identity — a moved player, a new coordinator, or a
  rebooted player (BootSeq / 412) is followed, resubscribed, and logged. The
  `Live` handle runs the reconcile + event loop on its own thread and publishes
  snapshots so readers never wait on the network.
  [`7530edf`](https://github.com/Dicklesworthstone/frankensonos/commit/7530edf),
  [`147df63`](https://github.com/Dicklesworthstone/frankensonos/commit/147df63),
  [`836da3c`](https://github.com/Dicklesworthstone/frankensonos/commit/836da3c),
  [`6cf149d`](https://github.com/Dicklesworthstone/frankensonos/commit/6cf149d),
  [`82db0cd`](https://github.com/Dicklesworthstone/frankensonos/commit/82db0cd),
  [`6ee2d71`](https://github.com/Dicklesworthstone/frankensonos/commit/6ee2d71)

- **Action log, undo, scenes, schedules.** Every mutating request is logged
  (who asked, the policy's allow/clamp/deny decision, the before-state) and the
  newest action is undoable — including restoring a changed URI or splitting a
  regrouped room back out (migration 4). Named **scenes** save/diff/apply a whole
  house state (migration 5). A daemon **scheduler** parses schedules, computes
  next-run across DST, skips missed runs and never fires twice (migration 6), and
  **sleep timers** fade out, pause, and restore the volume.
  [`e29ebab`](https://github.com/Dicklesworthstone/frankensonos/commit/e29ebab),
  [`ad35754`](https://github.com/Dicklesworthstone/frankensonos/commit/ad35754),
  [`b2a8883`](https://github.com/Dicklesworthstone/frankensonos/commit/b2a8883),
  [`fe45ea1`](https://github.com/Dicklesworthstone/frankensonos/commit/fe45ea1),
  [`ba0eefb`](https://github.com/Dicklesworthstone/frankensonos/commit/ba0eefb)

- **Doctor, announcements, move & party.** A check engine (`fsonos doctor`,
  `GET /doctor`, MCP doctor tool) with LAN checks and per-household
  Spotify-on-Sonos checks (linkage / favorite / render-params, S1 and S2
  separately). Announcements and chimes that duck the music and put it back.
  Moving playback between rooms, copying across households, and whole-house
  party mode.
  [`556e749`](https://github.com/Dicklesworthstone/frankensonos/commit/556e749),
  [`715952a`](https://github.com/Dicklesworthstone/frankensonos/commit/715952a),
  [`93909c2`](https://github.com/Dicklesworthstone/frankensonos/commit/93909c2),
  [`63b563f`](https://github.com/Dicklesworthstone/frankensonos/commit/63b563f),
  [`b645698`](https://github.com/Dicklesworthstone/frankensonos/commit/b645698)

- **Spotify library + classical DJ (`fsonos-spotify`).** A read-only Spotify Web
  API client — OAuth Authorization Code + PKCE (S256), `user-library-read` only —
  with a library cache synced into the store and library search. A dependency-free
  DJ that models **whole works** (movements grouped and ordered, with
  completeness), picks for pleasant variety (anti-repeat, composer/work/album
  spread, prolific-composer damping, time-of-day energy), learns from explicit
  and implicit feedback with decaying weights, and follows time-of-day mood
  programs. A queue feed plays whole works continuously on a coordinator, fed by
  GENA playback events.
  [`7e641c3`](https://github.com/Dicklesworthstone/frankensonos/commit/7e641c3),
  [`8b09d50`](https://github.com/Dicklesworthstone/frankensonos/commit/8b09d50),
  [`fa4d700`](https://github.com/Dicklesworthstone/frankensonos/commit/fa4d700),
  [`2cac89a`](https://github.com/Dicklesworthstone/frankensonos/commit/2cac89a),
  [`f337c33`](https://github.com/Dicklesworthstone/frankensonos/commit/f337c33),
  [`823d223`](https://github.com/Dicklesworthstone/frankensonos/commit/823d223),
  [`a3d9802`](https://github.com/Dicklesworthstone/frankensonos/commit/a3d9802)

- **Spotify-on-Sonos rendering (the hard part).** The `x-sonos-spotify:` render
  template that resolves the long-standing UPnP 800 — the per-household
  `SA_RINCON<svc>` DIDL `desc` is load-bearing and learned from the household's
  own favorites — plus continuous playback through the coordinator's own queue.
  [`739a8ee`](https://github.com/Dicklesworthstone/frankensonos/commit/739a8ee),
  [`f40ff99`](https://github.com/Dicklesworthstone/frankensonos/commit/f40ff99),
  [`f7f4cd2`](https://github.com/Dicklesworthstone/frankensonos/commit/f7f4cd2)

- **Surfaces: CLI, HTTP API, MCP server, and the `serve` daemon.** A shared
  request/failure/source model and a single `Surface` choke point reused by all
  three surfaces, with stable error codes, hints and suggestions
  (`docs/ERRORS.md`). The `fsonos` CLI runs discover/zones/control subcommands
  against the sim or a real LAN with JSON-lines logs. The HTTP API (fastapi_rust)
  serves `GET /health /zones /zones/{room} /zones/{room}/state /favorites
  /doctor /actions` and `POST /play /pause /resume /next /previous /volume /mute
  /group /ungroup /play/favorite /undo /dj/{start,skip,stop}`, with
  `/openapi.json` and browser-safe listeners (Host allow-list, Origin check,
  JSON-only writes). The MCP server (fastmcp_rust) exposes the control tools over
  stdio and streamable HTTP. `fsonos serve` hosts the HTTP API and MCP on one
  shared Surface, prints a ready line with its bound addresses, stays up when
  discovery finds nothing, and exits cleanly on SIGINT/SIGTERM.
  [`c060cf9`](https://github.com/Dicklesworthstone/frankensonos/commit/c060cf9),
  [`8001776`](https://github.com/Dicklesworthstone/frankensonos/commit/8001776),
  [`695ee1b`](https://github.com/Dicklesworthstone/frankensonos/commit/695ee1b),
  [`d4e20fe`](https://github.com/Dicklesworthstone/frankensonos/commit/d4e20fe),
  [`4b1a962`](https://github.com/Dicklesworthstone/frankensonos/commit/4b1a962),
  [`32f2217`](https://github.com/Dicklesworthstone/frankensonos/commit/32f2217),
  [`a3beb2e`](https://github.com/Dicklesworthstone/frankensonos/commit/a3beb2e),
  [`3237755`](https://github.com/Dicklesworthstone/frankensonos/commit/3237755)

- **Tailscale (central to the product).** Detects this host's tailnet (status,
  addresses, MagicDNS name); builds the exact connect URL to hand owners and
  agents; and attributes each action to a tailnet identity (WhoIs) for the log
  and per-identity policy — the groundwork for commanding the speakers from
  anywhere on the tailnet.
  [`0fe015b`](https://github.com/Dicklesworthstone/frankensonos/commit/0fe015b),
  [`8a9221a`](https://github.com/Dicklesworthstone/frankensonos/commit/8a9221a),
  [`874d29e`](https://github.com/Dicklesworthstone/frankensonos/commit/874d29e)

- **Simulator + e2e harness (`fsonos-sim`).** Virtual Sonos players over real
  localhost sockets, with routing, stereo pairs, deterministic unicast-SSDP
  discovery, GENA eventing, GroupRenderingControl, and fault injection; `fsonos
  sim` lets you try FrankenSonos without speakers. The e2e harness drives real
  scenarios with a JSON-lines step-log convention.
  [`4338b1e`](https://github.com/Dicklesworthstone/frankensonos/commit/4338b1e),
  [`740ff46`](https://github.com/Dicklesworthstone/frankensonos/commit/740ff46),
  [`5ad5a33`](https://github.com/Dicklesworthstone/frankensonos/commit/5ad5a33),
  [`5b57e62`](https://github.com/Dicklesworthstone/frankensonos/commit/5b57e62),
  [`6f2df23`](https://github.com/Dicklesworthstone/frankensonos/commit/6f2df23)

- **Franken dependency integration (`FND-DEPS`).** The owner's Rust stack —
  `asupersync` (unified on one 0.5.0), `fsqlite` (store), `fastmcp` (MCP), and
  `fastapi` (HTTP) — wired via the proven version/git-rev recipe, each proven by a
  real round-trip test. No Tokio, no reqwest, no bundled SQLite.
  [`f7c5807`](https://github.com/Dicklesworthstone/frankensonos/commit/f7c5807),
  [`aa8ca26`](https://github.com/Dicklesworthstone/frankensonos/commit/aa8ca26)

- **Live wire-protocol reference (authorized RE lane).** `docs/PROTOCOL.md`: a
  reference verified against the owner's own S1 and S2 players — the service/URL
  matrix, the Spotify render template, continuous-playback and container-playback
  flows, playlist URIs (legacy + modern), GENA subscription mechanics, SMAPI
  documented against the live endpoint, and the `x-rincon-mp3radio:` bypass path.
  All captures stay local and git-ignored; the repo carries only scrubbed prose
  and synthetic fixtures. Owner-run, inaudible checks on the real households
  confirm discovery, control, render-params and the live model.
  [`a835a81`](https://github.com/Dicklesworthstone/frankensonos/commit/a835a81),
  [`b5e5b8b`](https://github.com/Dicklesworthstone/frankensonos/commit/b5e5b8b),
  [`6dd4627`](https://github.com/Dicklesworthstone/frankensonos/commit/6dd4627),
  [`3c274b1`](https://github.com/Dicklesworthstone/frankensonos/commit/3c274b1),
  [`d88cf26`](https://github.com/Dicklesworthstone/frankensonos/commit/d88cf26)

- **mDNS discovery channel (`fsonos-proto::mdns`).** A pure, bounds-checked
  DNS message parser (RFC 1035 compression, PTR/SRV/TXT/A) plus
  `SonosAdvert` extraction for both live-observed advertisement styles —
  S2's `RINCON_<uuid>@Room` with rich TXT (`hhid`, `bootseq`, `location`,
  `wss`) and S1's `Sonos-<MAC>` minimal TXT — proven against raw wire packets
  captured on the owner's LAN (scrubbed byte-equal to keep DNS compression
  offsets valid). Behavioral findings recorded for the socket layer: S2
  answers direct ephemeral-port probes unicast; S1 only answers
  responder-pattern queries; S2 echoes the question section.
- **Reliability architecture.** `docs/ROBUSTNESS.md` maps every failure mode
  observed on the owner's LAN to its absorbing mechanism (UUID-keyed
  identity, self-healing addressing, runtime-learned render params, GENA
  self-repair, safe mutations, supervised daemon), and `docs/PROTOCOL.md` §12
  records why native on-speaker Tailscale is rejected in favor of the
  daemon-fronted model (no owner code-execution channel on stock firmware;
  custom signed images mean brick risk and per-update re-injection).

### Security / privacy

- The repository is public and deliberately carries **no** site-specific data —
  no real IPs, MAC addresses, serial numbers, household ids, tokens, keys,
  firmware images, or packet captures. `.gitignore` excludes local inventories,
  captures, auth caches, the beads database, and per-machine agent state
  (`.claude/`); test fixtures use a deterministic synthetic scheme
  (`RINCON_000E58A0xxxx01400`, `100.101.102.103`, …) enforced by a
  `fixtures_are_scrubbed` test; example room names are generic.
- Browser-safe listeners (Host allow-list, Origin check, JSON-only writes) and a
  bind guard (loopback + tailnet CGNAT allowed; public/wildcard refused without
  `--allow-unsafe-bind`) keep the control surface safe to expose on the tailnet.

## Notes for agents

- **Start here:** `COMPREHENSIVE_PLAN_FOR_FRANKENSONOS.md`, then `AGENTS.md`
  (doctrine) and `docs/SCOPE.md` (the hard interoperability-only boundary).
- **Task graph:** `br ready` lists claimable work; lanes are A (proto + sim),
  B (core), C (spotify/DJ), D (surfaces), plus a Tailscale epic and an
  owner-authorized RE lane.
- **Build:** `cargo` is the gate (it offloads to the `rch` fleet); keep the pure
  unit tests green. Live-LAN behavior is opt-in behind a feature/env flag
  (`FSONOS_SEEDS=…`, `#[ignore]`) so CI never needs the network.

[Unreleased]: https://github.com/Dicklesworthstone/frankensonos/commits/main
