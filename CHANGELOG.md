# Changelog

All notable changes to FrankenSonos are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project will
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) from its
first release.

**Scope and method.** This log is reconstructed from the real git history
(`git log`), the beads task graph in `.beads/`, and the committed source. There
are no tags or GitHub Releases yet: the project is pre-`0.1.0`, built by a
multi-agent swarm, so everything sits under [Unreleased](#unreleased).
Representative commits are linked for each capability so you can jump straight
to the evidence. A capability listed here has landed and passed the gates
(`cargo clippy --all-targets -D warnings` and the tests, run on the `rch`
fleet). How each part was verified is spelled out in
[Status and verification](#status-and-verification); no shipped binary, and no
live-hardware behavior beyond the owner-run checks named there, is claimed.

## Timeline

| Wave | When (2026) | What landed |
|---|---|---|
| 1. Foundation | Oct 6, evening | The workspace, the plan and scope, the franken dependency stack, protocol and core parsing, the Spotify client shapes |
| 2. Talking to the speakers | Oct 6, night | The real LAN transport, control actions, the household survey, the store, Spotify sign-in, the doctor engine |
| 3. Live model and simulator | Oct 7, small hours | GENA eventing, continuous Spotify playback, the simulator and e2e harness, the event engine and self-healing calls, the action log, the DJ's whole works |
| 4. The daemon and its surfaces | Oct 7, day | `fsonos serve` with the HTTP API and MCP; scenes, schedules and sleep timers; the action log and undo on every surface; browser-safe listeners; OpenAPI; the live model |
| 5. Tailnet first | Oct 7, evening | Listening on the tailnet by default, Tailscale Serve setup (never Funnel), live events over SSE, a hardened long-running live model |
| 6. Every capability on every surface | Oct 8, small hours to midday | The DJ playing on the speakers, steering and feedback; scenes, sleep timers, schedules, announcements, aliases, move and party, and the house policy on CLI, HTTP and MCP; `fsonos setup` |
| 7. Instant CLI and web remote | Oct 8, afternoon | CLI commands through a running daemon, the daemon's web remote with album art |
| 8. A DJ for every genre | Oct 8, evening | The DJ plays the owner's whole library in any genre, with classical works still whole, and steers by artist |

There is no released version yet. `0.1.0` comes with real-hardware CI and
prebuilt binaries (see [Status and verification](#status-and-verification)).

## [Unreleased]

Pre-release work toward `0.1.0`: control the owner's Sonos S1 and S2 players
from the `fsonos` CLI, an HTTP API, an MCP server and a web page, from any
device on the owner's tailnet, with a DJ playing from the owner's own Spotify
library, in whatever genres it spans.

### Added

#### Foundation

- **The workspace.** One Rust 2024 Cargo workspace of nine crates
  (`fsonos-types`, `-proto`, `-core`, `-spotify`, `-api`, `-mcp`, `-cli`,
  `-tailscale`, `-sim`), `#![forbid(unsafe_code)]` everywhere, a pinned
  `nightly-2026-08-31` toolchain and a committed `Cargo.lock`. With the
  comprehensive plan, `AGENTS.md`, the scope boundary (`docs/SCOPE.md`) and the
  beads task graph.
  [`0ecb761`](https://github.com/Dicklesworthstone/frankensonos/commit/0ecb761)
- **The franken dependency stack (`FND-DEPS`).** `asupersync` (one 0.5.0 for
  the whole graph), `fsqlite` (the store), `fastmcp` (MCP) and `fastapi` (HTTP),
  each proven by a real round-trip test. No Tokio, no reqwest, no bundled
  SQLite.
  [`f7c5807`](https://github.com/Dicklesworthstone/frankensonos/commit/f7c5807),
  [`aa8ca26`](https://github.com/Dicklesworthstone/frankensonos/commit/aa8ca26)

#### Speaking the speakers' protocols (`fsonos-proto`)

- SSDP discovery; UPnP/SOAP envelopes and faults behind a `Transport` seam;
  device descriptions, ZoneGroupTopology and ContentDirectory parsing;
  DIDL-Lite and `x-sonos-spotify:` URIs from per-household parameters; a real
  LAN transport over asupersync; `RampToVolume` with the three ramp types; the
  complete GENA event catalog; live-verified grouping verbs; and an mDNS/DNS-SD
  discovery parser — now also a query builder and a live second discovery
  channel: the survey asks `_sonos._tcp.local` (multicast-QU, the pattern
  both generations answer) next to SSDP, mDNS findings fill addresses SSDP
  missed and refine household hints from the S2 TXT `hhid=`, and the survey
  survives an SSDP failure on the mDNS channel alone. Each is proven by
  golden tests, many of them on scrubbed live captures.
  [`635d1cf`](https://github.com/Dicklesworthstone/frankensonos/commit/635d1cf),
  [`bacf067`](https://github.com/Dicklesworthstone/frankensonos/commit/bacf067),
  [`739a8ee`](https://github.com/Dicklesworthstone/frankensonos/commit/739a8ee),
  [`9d97549`](https://github.com/Dicklesworthstone/frankensonos/commit/9d97549),
  [`1455e06`](https://github.com/Dicklesworthstone/frankensonos/commit/1455e06),
  [`208f7db`](https://github.com/Dicklesworthstone/frankensonos/commit/208f7db),
  [`34b8615`](https://github.com/Dicklesworthstone/frankensonos/commit/34b8615)
- **Binary GETs for album art.** `Transport::http_get_bytes` returns a body and
  its content type, routed like every other request (so a routes file confines
  it) and capped at 4 MiB.
  [`c8d64c9`](https://github.com/Dicklesworthstone/frankensonos/commit/c8d64c9)

#### The house model (`fsonos-core`)

- **Two households, one model.** S1/S2 classification and coordinator
  resolution; topology folded into rooms (stereo pairs and home theaters
  included); room resolution with aliases, `all`/`everywhere`/`here`, unique
  prefixes and suggestions; and control addressed to the right coordinator.
  [`1c0d93e`](https://github.com/Dicklesworthstone/frankensonos/commit/1c0d93e),
  [`b51455d`](https://github.com/Dicklesworthstone/frankensonos/commit/b51455d),
  [`31dfe37`](https://github.com/Dicklesworthstone/frankensonos/commit/31dfe37)
- **What a house can do.** Grouping, zone snapshots, device-speed volume
  fades, playing Sonos favorites, moving playback between rooms (and copying
  across households), party mode, and announcements and chimes that put the
  music back.
  [`23c524d`](https://github.com/Dicklesworthstone/frankensonos/commit/23c524d),
  [`005051a`](https://github.com/Dicklesworthstone/frankensonos/commit/005051a),
  [`667fc7f`](https://github.com/Dicklesworthstone/frankensonos/commit/667fc7f),
  [`ca89c80`](https://github.com/Dicklesworthstone/frankensonos/commit/ca89c80),
  [`b645698`](https://github.com/Dicklesworthstone/frankensonos/commit/b645698),
  [`63b563f`](https://github.com/Dicklesworthstone/frankensonos/commit/63b563f)
- **Memory and safety.** A durable `fsqlite` store with append-only
  migrations; an action log in which every mutation records who asked, the
  policy's decision and the before-state, with the newest action undoable
  (DJ steering included); scenes that save, diff and apply a whole house
  state; schedules that survive DST and never fire twice; sleep timers that
  fade; and a house policy covering volume caps, step limits, quiet hours and
  per-tool allowlists.
  [`b056658`](https://github.com/Dicklesworthstone/frankensonos/commit/b056658),
  [`e29ebab`](https://github.com/Dicklesworthstone/frankensonos/commit/e29ebab),
  [`22cf13a`](https://github.com/Dicklesworthstone/frankensonos/commit/22cf13a),
  [`b2a8883`](https://github.com/Dicklesworthstone/frankensonos/commit/b2a8883),
  [`fe45ea1`](https://github.com/Dicklesworthstone/frankensonos/commit/fe45ea1),
  [`ba0eefb`](https://github.com/Dicklesworthstone/frankensonos/commit/ba0eefb),
  [`cdb5308`](https://github.com/Dicklesworthstone/frankensonos/commit/cdb5308)
- **A live, self-healing model.**
  - The event engine keeps every GENA subscription alive.
  - A reconcile loop surveys with jittered backoff and tracks each player's health.
  - Playback state is folded from NOTIFY events.
  - A moved player, a new coordinator or a rebooted one is followed and
    resubscribed.
  - A powered-off speaker is reported as unreachable, not unknown.
  - Commands are retried once when the house changed under them.
  - The model survives a sleeping host and stops within 3 s.
  [`7530edf`](https://github.com/Dicklesworthstone/frankensonos/commit/7530edf),
  [`147df63`](https://github.com/Dicklesworthstone/frankensonos/commit/147df63),
  [`6cf149d`](https://github.com/Dicklesworthstone/frankensonos/commit/6cf149d),
  [`6ee2d71`](https://github.com/Dicklesworthstone/frankensonos/commit/6ee2d71),
  [`2b61e38`](https://github.com/Dicklesworthstone/frankensonos/commit/2b61e38),
  [`19cdb7c`](https://github.com/Dicklesworthstone/frankensonos/commit/19cdb7c),
  [`7139367`](https://github.com/Dicklesworthstone/frankensonos/commit/7139367),
  [`8ce7640`](https://github.com/Dicklesworthstone/frankensonos/commit/8ce7640),
  [`e5e2ad3`](https://github.com/Dicklesworthstone/frankensonos/commit/e5e2ad3)

#### Spotify library and the DJ (`fsonos-spotify`)

- **Reading the owner's library.** A read-only Spotify Web API client using
  OAuth Authorization Code with PKCE (S256) and the `user-library-read` scope
  only. The library is cached in the store and searchable together with the
  household's favorites.
  [`8b09d50`](https://github.com/Dicklesworthstone/frankensonos/commit/8b09d50),
  [`f337c33`](https://github.com/Dicklesworthstone/frankensonos/commit/f337c33),
  [`860caa9`](https://github.com/Dicklesworthstone/frankensonos/commit/860caa9)
- **A DJ that thinks in whole works.**
  - Movements are grouped and ordered, and liked single movements are completed
    from album track lists.
  - Picks aim for pleasant variety and follow time-of-day mood programs.
  - Explicit and implicit feedback teaches it: likes, dislikes, early skips and
    full listens.
  - Sessions, steering and programs round-trip through the store and
    `moods.toml`.
  [`7e641c3`](https://github.com/Dicklesworthstone/frankensonos/commit/7e641c3),
  [`fa4d700`](https://github.com/Dicklesworthstone/frankensonos/commit/fa4d700),
  [`2cac89a`](https://github.com/Dicklesworthstone/frankensonos/commit/2cac89a),
  [`bf5b459`](https://github.com/Dicklesworthstone/frankensonos/commit/bf5b459),
  [`823d223`](https://github.com/Dicklesworthstone/frankensonos/commit/823d223),
  [`76d92cc`](https://github.com/Dicklesworthstone/frankensonos/commit/76d92cc),
  [`8604422`](https://github.com/Dicklesworthstone/frankensonos/commit/8604422)
- **A DJ for every genre.** The pool is the owner's whole library, not only
  its classical music. A song plays on its own, credited to its artist, and a
  classical work still plays whole; picks spread across artists, steering
  takes artists (`fsonos dj steer --artist`, `--not-artist`, MCP `dj_steer`),
  and the surfaces describe the DJ the same way for every genre.
  [`98543a8`](https://github.com/Dicklesworthstone/frankensonos/commit/98543a8),
  [`e41e929`](https://github.com/Dicklesworthstone/frankensonos/commit/e41e929)
- **Spotify on Sonos.** The `x-sonos-spotify:` render template, with
  per-household parameters learned from the household's own favorites, ends
  the long-standing UPnP 800 — and when a household's parameters drift
  (Spotify relinked, service updated), the render self-heals: on a UPnP 800
  the parameters are relearned from the favorites and the render retried
  exactly once; a second refusal reports `RENDER_PARAMS_STALE` with the
  re-link remedy instead of looping. Playback continues through the coordinator's
  [`739a8ee`](https://github.com/Dicklesworthstone/frankensonos/commit/739a8ee),
  [`f7f4cd2`](https://github.com/Dicklesworthstone/frankensonos/commit/f7f4cd2),
  [`a3d9802`](https://github.com/Dicklesworthstone/frankensonos/commit/a3d9802),
  [`8c4702e`](https://github.com/Dicklesworthstone/frankensonos/commit/8c4702e)

#### The daemon and its surfaces (`fsonos-api`, `fsonos-mcp`, `fsonos-cli`)

- **One choke point, three surfaces.**
  - A single `Surface` backs the CLI, the HTTP API and the MCP server, with
    stable error codes, hints and suggestions (`docs/ERRORS.md`).
  - Every operation id in `/openapi.json` names the MCP tool that does the same.
  - `fsonos serve` hosts the API and MCP on one live model, serves connections
    concurrently and streams the house's changes as server-sent events
    (`GET /events`, resumable with `Last-Event-ID`).
  [`c060cf9`](https://github.com/Dicklesworthstone/frankensonos/commit/c060cf9),
  [`8001776`](https://github.com/Dicklesworthstone/frankensonos/commit/8001776),
  [`3237755`](https://github.com/Dicklesworthstone/frankensonos/commit/3237755),
  [`a3beb2e`](https://github.com/Dicklesworthstone/frankensonos/commit/a3beb2e),
  [`772e955`](https://github.com/Dicklesworthstone/frankensonos/commit/772e955),
  [`9391198`](https://github.com/Dicklesworthstone/frankensonos/commit/9391198)
- **Every capability on every surface.** Each of these is available from the
  CLI, over HTTP and as an MCP tool, all through the same policy and action
  log:
  - favorites and zone state;
  - the action log and undo;
  - room aliases (`fsonos rooms alias`);
  - move and party;
  - the DJ's status, "why", steering and moods (`fsonos dj`), and its feedback;
  - scenes (`fsonos scene save|apply|list|show|rm`);
  - sleep timers and schedules, run by the daemon's scheduler;
  - say and chime;
  - the house policy (`fsonos policy show|check`);
  - for agents, library search, play history and `sonos://` resources.
  [`4b1a962`](https://github.com/Dicklesworthstone/frankensonos/commit/4b1a962),
  [`4063bdb`](https://github.com/Dicklesworthstone/frankensonos/commit/4063bdb),
  [`256e6b0`](https://github.com/Dicklesworthstone/frankensonos/commit/256e6b0),
  [`541f101`](https://github.com/Dicklesworthstone/frankensonos/commit/541f101),
  [`9cf1ad7`](https://github.com/Dicklesworthstone/frankensonos/commit/9cf1ad7),
  [`72ff65d`](https://github.com/Dicklesworthstone/frankensonos/commit/72ff65d),
  [`ef981f5`](https://github.com/Dicklesworthstone/frankensonos/commit/ef981f5),
  [`483240c`](https://github.com/Dicklesworthstone/frankensonos/commit/483240c),
  [`cb48f3c`](https://github.com/Dicklesworthstone/frankensonos/commit/cb48f3c),
  [`1ad7a24`](https://github.com/Dicklesworthstone/frankensonos/commit/1ad7a24),
  [`98fc748`](https://github.com/Dicklesworthstone/frankensonos/commit/98fc748),
  [`018d5e3`](https://github.com/Dicklesworthstone/frankensonos/commit/018d5e3),
  [`847fbd4`](https://github.com/Dicklesworthstone/frankensonos/commit/847fbd4),
  [`f5e8b88`](https://github.com/Dicklesworthstone/frankensonos/commit/f5e8b88),
  [`d987c5d`](https://github.com/Dicklesworthstone/frankensonos/commit/d987c5d)
- **Playing what you mean.** `fsonos play <room> --search "<query>"` plays
  the best match from the library and favorites, with `--pick` to choose.
  [`abf7320`](https://github.com/Dicklesworthstone/frankensonos/commit/abf7320)
- **A guided first run.** `fsonos setup` walks through the Spotify sign-in and
  the LAN checks, plays softly for five seconds to prove a room
  (`--test-play`), and ends with the tailnet connect URLs. `fsonos doctor`
  checks the LAN, Spotify on Sonos in each household, and Tailscale, and
  every problem names its fix.
  [`816946b`](https://github.com/Dicklesworthstone/frankensonos/commit/816946b),
  [`9cb1128`](https://github.com/Dicklesworthstone/frankensonos/commit/9cb1128),
  [`15e4961`](https://github.com/Dicklesworthstone/frankensonos/commit/15e4961),
  [`25cae3c`](https://github.com/Dicklesworthstone/frankensonos/commit/25cae3c),
  [`715952a`](https://github.com/Dicklesworthstone/frankensonos/commit/715952a),
  [`bef99d9`](https://github.com/Dicklesworthstone/frankensonos/commit/bef99d9)
- **An instant CLI.** When a daemon is running, CLI commands go through it,
  using its warm state and its sleep-timer fades, and are identified as the
  CLI by a token in the data directory.
  [`d1af970`](https://github.com/Dicklesworthstone/frankensonos/commit/d1af970)
- **The web remote.** The daemon serves a page at `/`, reachable from any
  device on the tailnet. It shows:
  - rooms by household;
  - what each zone plays, with album art from the zone's own player
    (`GET /art`);
  - play/pause and next;
  - a volume slider per room;
  - DJ start and stop with a mood picker;
  - scene buttons;
  - a banner when doctor checks fail.

  It is kept live by `GET /events`, needs no build step and loads nothing from
  any CDN. It works by keyboard, at phone width and in dark mode.
  [`f82b7df`](https://github.com/Dicklesworthstone/frankensonos/commit/f82b7df)

#### Tailscale (central to the product)

- **Command your speakers from anywhere on your tailnet.**
  - FrankenSonos detects the host's tailnet and prints exact connect URLs.
  - The HTTP API listens on loopback plus the tailnet by default.
  - `fsonos tailscale setup|status|teardown` fronts the API and MCP with
    Tailscale Serve over HTTPS. It refuses Funnel.
  - Behind Serve, the caller's tailnet login is their identity in the policy
    and the log.
  [`0fe015b`](https://github.com/Dicklesworthstone/frankensonos/commit/0fe015b),
  [`8a9221a`](https://github.com/Dicklesworthstone/frankensonos/commit/8a9221a),
  [`874d29e`](https://github.com/Dicklesworthstone/frankensonos/commit/874d29e),
  [`990f252`](https://github.com/Dicklesworthstone/frankensonos/commit/990f252),
  [`3c99c85`](https://github.com/Dicklesworthstone/frankensonos/commit/3c99c85),
  [`b6b72a4`](https://github.com/Dicklesworthstone/frankensonos/commit/b6b72a4),
  [`3a0b8c4`](https://github.com/Dicklesworthstone/frankensonos/commit/3a0b8c4)

#### Simulator and test harness (`fsonos-sim`)

- **Try it without speakers.** Virtual Sonos players run over real localhost
  sockets, with:
  - stereo pairs, deterministic discovery, GENA eventing and fault injection;
  - time that plays a queue through, and clip fetches;
  - parity checks against real fixtures;
  - album art served the way players serve it.

  `fsonos sim` and a routes file confine a run to the simulator, which
  refuses anything it does not name. The e2e harness drives whole scenarios
  through `fsonos serve`, with JSON-lines step logs.
  [`4338b1e`](https://github.com/Dicklesworthstone/frankensonos/commit/4338b1e),
  [`740ff46`](https://github.com/Dicklesworthstone/frankensonos/commit/740ff46),
  [`5ad5a33`](https://github.com/Dicklesworthstone/frankensonos/commit/5ad5a33),
  [`76e03c2`](https://github.com/Dicklesworthstone/frankensonos/commit/76e03c2),
  [`5b57e62`](https://github.com/Dicklesworthstone/frankensonos/commit/5b57e62),
  [`dfb59cf`](https://github.com/Dicklesworthstone/frankensonos/commit/dfb59cf),
  [`6f2df23`](https://github.com/Dicklesworthstone/frankensonos/commit/6f2df23)

#### Documentation

- `docs/PROTOCOL.md`: the wire-protocol reference, verified against the
  owner's own S1 and S2 players. It covers the service matrix, the Spotify
  render template, queue and album-container playback, playlist URIs, GENA
  mechanics and the grouping verbs.
  [`a835a81`](https://github.com/Dicklesworthstone/frankensonos/commit/a835a81),
  [`3c274b1`](https://github.com/Dicklesworthstone/frankensonos/commit/3c274b1),
  [`8e933ef`](https://github.com/Dicklesworthstone/frankensonos/commit/8e933ef)
- `docs/ROBUSTNESS.md` maps every observed failure mode to the mechanism that
  absorbs it. `docs/DEPLOY.md` covers launchd, the tailnet and Serve.
  `docs/ERRORS.md` lists every error code.
  [`f096002`](https://github.com/Dicklesworthstone/frankensonos/commit/f096002),
  [`72823bf`](https://github.com/Dicklesworthstone/frankensonos/commit/72823bf)
- Owner-directed research notes are kept as prose under `docs/` (for example
  [`docs/FIRMWARE_PATHS.md`](docs/FIRMWARE_PATHS.md)). They are documentation
  only: FrankenSonos never writes to a device.

### Fixed

A selection; each commit message has the details.

- HTTP connections are served concurrently, so an open event stream never
  holds up other callers.
  [`d52e4f3`](https://github.com/Dicklesworthstone/frankensonos/commit/d52e4f3)
- IPv6 listeners answer their own address, and loopback listeners answer
  Tailscale Serve's `ts.net` Host.
  [`11ccf17`](https://github.com/Dicklesworthstone/frankensonos/commit/11ccf17),
  [`77a47a6`](https://github.com/Dicklesworthstone/frankensonos/commit/77a47a6)
- A daemon on an ephemeral port admits its own page's origin.
  [`f82b7df`](https://github.com/Dicklesworthstone/frankensonos/commit/f82b7df)
- A powered-off speaker is `PLAYER_UNREACHABLE`, not an unknown room.
  [`974127a`](https://github.com/Dicklesworthstone/frankensonos/commit/974127a)
- The queue feed follows inserts, removals and replaced queues, and a resync
  puts a work's movements back to back.
  [`77f17f2`](https://github.com/Dicklesworthstone/frankensonos/commit/77f17f2),
  [`03fea77`](https://github.com/Dicklesworthstone/frankensonos/commit/03fea77)
- Every DJ pick is steered by the zone's session or the time-of-day program,
  and a steering clash is `INVALID_ARGUMENT`.
  [`a3558c6`](https://github.com/Dicklesworthstone/frankensonos/commit/a3558c6),
  [`1eb9b37`](https://github.com/Dicklesworthstone/frankensonos/commit/1eb9b37)
- A shutdown that cannot wake its listener no longer stalls.
  [`652af30`](https://github.com/Dicklesworthstone/frankensonos/commit/652af30)

### Security and privacy

- **No site data in this public repository.** No real IPs, MAC addresses,
  serials, household ids, tokens, keys, firmware images or packet captures.
  Fixtures use a synthetic scheme enforced by a `fixtures_are_scrubbed` test,
  and a doc lint keeps site data out of the deploy docs.
  [`635d1cf`](https://github.com/Dicklesworthstone/frankensonos/commit/635d1cf),
  [`72823bf`](https://github.com/Dicklesworthstone/frankensonos/commit/72823bf)
- **Browser-safe listeners.**
  - A Host allow-list defeats DNS rebinding.
  - A request carrying an Origin must come from one of the daemon's own.
  - Writes must be JSON.
  - No CORS header is ever sent.
  - A bind guard refuses public or wildcard binds without
    `--allow-unsafe-bind`.
  [`32f2217`](https://github.com/Dicklesworthstone/frankensonos/commit/32f2217)
- **Serve, never Funnel.**
  - Setup refuses to run while Funnel is on.
  - A daemon admits exactly its own Serve origin, `https://<MagicDNS name>`.
    It learns that name from Tailscale's own Serve config and withdraws it
    while Funnel is on. It never trusts a wildcard or a name taken from a
    request.
  - The web remote's album art is fetched only from the zone's own player:
    the caller names a room, never a URL.
  [`b6b72a4`](https://github.com/Dicklesworthstone/frankensonos/commit/b6b72a4),
  [`f82b7df`](https://github.com/Dicklesworthstone/frankensonos/commit/f82b7df)

## Status and verification

- **Gates.** Every capability above landed after `cargo clippy --all-targets
  -D warnings` and its tests passed on the `rch` build fleet.
- **The simulator, end to end.** e2e scenarios drive the real `fsonos` binary
  and `fsonos serve` against `fsonos-sim`. They cover core control,
  self-healing, events, the DJ and its steering and feedback, scenes,
  schedules, announcements, the house policy and quiet hours, setup, the
  daemon client, and the web remote.
- **Real hardware, opt-in.** Owner-run, inaudible checks on the owner's own
  players confirm discovery, control, render parameters and the live model;
  a tailnet test covers serve and doctor. These are `#[ignore]`d or behind
  environment flags, so CI never needs the network.
  [`d88cf26`](https://github.com/Dicklesworthstone/frankensonos/commit/d88cf26),
  [`39c8fec`](https://github.com/Dicklesworthstone/frankensonos/commit/39c8fec)
- **Not yet.**
  - Real-hardware CI, prebuilt binaries and the `0.1.0` tag.
  - Spotify rendering on legacy S1 players is still being confirmed on real
    hardware.
  - MCP resource-update notifications wait on upstream support. GET /events,
    the HTTP stream, already delivers the same changes.
  - Interfaces may change until `0.1.0`.

## Notes for agents

- **Start here:** `COMPREHENSIVE_PLAN_FOR_FRANKENSONOS.md`, then `AGENTS.md`
  (doctrine) and `docs/SCOPE.md` (the binding scope boundary).
- **Task graph:** `br ready` lists claimable work. The lanes are A (proto and
  sim), B (core), C (Spotify and the DJ) and D (surfaces), plus the Tailscale
  epic and an owner-directed research lane.
- **Build:** `cargo` is the gate and runs on the `rch` fleet; keep the pure
  unit tests green. Live-LAN tests are opt-in (`FSONOS_SEEDS=…`, `#[ignore]`).

[Unreleased]: https://github.com/Dicklesworthstone/frankensonos/commits/main
