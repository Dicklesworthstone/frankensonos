# Changelog

All notable changes to FrankenSonos are recorded here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project will
follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) from its
first release.

**Scope and method.** This log is reconstructed from the real git history
(`git log`), the beads task graph in `.beads/`, and the committed source. There
are no tags or GitHub Releases before this one: the project was pre-`0.1.0`,
built by a multi-agent swarm, so everything sits under [0.1.0](#010---2026-10-24).
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
| 9. Albums, playlists and fades | Oct 9, evening | Spotify albums and playlists play on every surface, and library search finds saved albums and playlists by name; `fsonos sleep` fades through a running daemon |
| 10. The whole house, live | Oct 10–12 | Direct tailnet callers named by Tailscale WhoIs; house-wide verbs (`fsonos pause everywhere`); the queue, tone, mute and history on every surface; fades on demand and hand-written scenes; `fsonos events`; a DJ that survives restarts, trims its queue and hears more of your listening; more doctor checks; discovery on every interface and other fixes found on real players |
| 11. Promises kept | Oct 12 | Seek; `fsonos zones` and `fsonos search`; an `events` tool for agents; the CLI from another tailnet machine; a scene that plays a Spotify link; a history of what the house played, not only what fsonos started; the house policy on every way music starts; long calls that never stall the daemon; sleep timers, fades and the DJ that hold up through moves, regroups, restarts and refusals |

There is no released version yet. `0.1.0` comes with real-hardware CI and
prebuilt binaries (see [Status and verification](#status-and-verification)).

## [0.1.0] - 2026-10-24

Everything in this section landed for the first release: control the owner's
Sonos S1 and S2 players from the `fsonos` CLI, an HTTP API, an MCP server and
a web page, from any device on the owner's tailnet, with a DJ playing from the owner's own Spotify
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
- **Albums and playlists play.** `fsonos play`, HTTP and MCP take a Spotify
  album or playlist (`spotify:album:…`, `spotify:playlist:…` or an
  open.spotify.com link). The group's queue is replaced with it, and it
  plays from its first track. The household's account and item-id prefix
  are learned from its own favorites, and on a UPnP 800 they are relearned
  once, as for tracks. Artists, episodes and shows still answer
  `NOT_IMPLEMENTED`.
  [`8f2b7b8`](https://github.com/Dicklesworthstone/frankensonos/commit/8f2b7b8)
- **Search finds albums and playlists.** Library search returns them
  (`fsonos play --search`, `search_library`, `GET /library/search`):
  - each saved album as a result of its own, which plays whole from its
    first track;
  - the playlists in the owner's list by name, their own and the ones they
    follow. The taste read keeps the list, cached in `spotify_playlists`
    (migration 10), with `playlist-read-private` granted.

  In a tie, tracks and favorites come first, then albums, then playlists,
  so a query that names a track still plays the track.
  [`d4bffdb`](https://github.com/Dicklesworthstone/frankensonos/commit/d4bffdb),
  [`13682d8`](https://github.com/Dicklesworthstone/frankensonos/commit/13682d8)
- **A DJ that hears more of your listening.** What you played on Spotify
  lately counts as heard; where Spotify gives no genre, one is guessed from
  the playlist, album and track names; variety balances broad genre families;
  and feedback nudges a work's genres too.
  [`17cd108`](https://github.com/Dicklesworthstone/frankensonos/commit/17cd108),
  [`f422c94`](https://github.com/Dicklesworthstone/frankensonos/commit/f422c94),
  [`95356c0`](https://github.com/Dicklesworthstone/frankensonos/commit/95356c0),
  [`acb8d51`](https://github.com/Dicklesworthstone/frankensonos/commit/acb8d51)
- **Steering can leave things out.** `fsonos dj steer` takes
  `--not-decade 80s` and `--not-period baroque` (`exclude_decades`,
  `exclude_periods`).
  [`e9b1099`](https://github.com/Dicklesworthstone/frankensonos/commit/e9b1099)
- **A DJ left on for days.** It survives a daemon restart, and once the queue
  passes 400 tracks it takes its long-played works off (keeping the last 50
  behind the one playing), never the owner's own items.
  `fsonos dj sync --status` says when the library was last read.
  [`be091a4`](https://github.com/Dicklesworthstone/frankensonos/commit/be091a4),
  [`516db31`](https://github.com/Dicklesworthstone/frankensonos/commit/516db31),
  [`ba88a9b`](https://github.com/Dicklesworthstone/frankensonos/commit/ba88a9b)

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
- **The whole house in one command.** A room can be a set: `all`,
  `everywhere` or an alias naming several rooms. `fsonos pause everywhere`,
  `fsonos volume downstairs 20` and `fsonos ungroup everywhere` reach each
  group once, step past a group that fails (noting it as `SKIPPED`), and are
  logged and undone as one action. `policy.toml` is reloaded when it changes,
  keeping the last good version after a malformed edit.
  [`889f3a0`](https://github.com/Dicklesworthstone/frankensonos/commit/889f3a0),
  [`ebbd728`](https://github.com/Dicklesworthstone/frankensonos/commit/ebbd728),
  [`730647f`](https://github.com/Dicklesworthstone/frankensonos/commit/730647f)
- **More of each room, on every surface.**
  - the queue (`fsonos queue`, `GET /zones/{room}/queue`, `get_queue`), and
    adding a Spotify link to it, next or at the end
    (`fsonos play <room> <link> --next`), which `fsonos undo` takes back off;
  - a room's tone: bass, treble and loudness (`fsonos eq`);
  - whether a room is muted, in its zone state;
  - what played (`fsonos history`), every sleep timer (`fsonos sleep`), the
    room `here` means (`fsonos rooms default`), each player's group in
    discovery, and whether quiet hours are in effect now;
  - for agents, `get_scene`, `delete_scene` and `list_players`.
  [`52d7bf7`](https://github.com/Dicklesworthstone/frankensonos/commit/52d7bf7),
  [`93594e3`](https://github.com/Dicklesworthstone/frankensonos/commit/93594e3),
  [`24e0f73`](https://github.com/Dicklesworthstone/frankensonos/commit/24e0f73),
  [`46ccb78`](https://github.com/Dicklesworthstone/frankensonos/commit/46ccb78),
  [`cc2992e`](https://github.com/Dicklesworthstone/frankensonos/commit/cc2992e),
  [`446c9f5`](https://github.com/Dicklesworthstone/frankensonos/commit/446c9f5),
  [`e03ae85`](https://github.com/Dicklesworthstone/frankensonos/commit/e03ae85),
  [`83def78`](https://github.com/Dicklesworthstone/frankensonos/commit/83def78),
  [`eb01101`](https://github.com/Dicklesworthstone/frankensonos/commit/eb01101)
- **Fades on demand.** `fsonos volume <room> 20 --fade 10s` ramps and
  `fsonos pause <room> --fade 30s` fades out first (`fade_volume`,
  `fade_and_pause`). `fsonos scene apply --fade 20s` fades a scene's rooms to
  their levels together, a room that starts playing fading in from silence,
  and a scheduled scene fades by the policy's default. Any volume change ends
  a fade in flight.
  [`987c34b`](https://github.com/Dicklesworthstone/frankensonos/commit/987c34b),
  [`de30916`](https://github.com/Dicklesworthstone/frankensonos/commit/de30916),
  [`187ece7`](https://github.com/Dicklesworthstone/frankensonos/commit/187ece7)
- **Scenes written out.** `fsonos scene save dinner --from dinner.toml`
  (`PUT /scenes/{name}` with a body, `save_scene` with `scene`) saves a scene
  written by hand: its groups, volumes and mutes, and per group a favorite, a
  stream or the DJ in a mood, checked against the house first.
  [`f7e056f`](https://github.com/Dicklesworthstone/frankensonos/commit/f7e056f)
- **Following the house live.** `fsonos events` prints the house's changes as
  they happen (with `--json`, an event per line; `--since <id>` replays what
  was missed). `GET /events` carries the DJ's picks as `dj.status`, and the
  web remote refreshes on them; it also gained previous, favorites, a room's
  mute, pause everywhere, and the DJ's mood, skip, like and dislike.
  [`34eee82`](https://github.com/Dicklesworthstone/frankensonos/commit/34eee82),
  [`69eb866`](https://github.com/Dicklesworthstone/frankensonos/commit/69eb866),
  [`cc82fe6`](https://github.com/Dicklesworthstone/frankensonos/commit/cc82fe6),
  [`36f262e`](https://github.com/Dicklesworthstone/frankensonos/commit/36f262e),
  [`7e0f95e`](https://github.com/Dicklesworthstone/frankensonos/commit/7e0f95e)
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
- **The doctor checks more.** It says when Sonos offers newer software than
  a player runs (FrankenSonos never installs it), whether Tailscale Serve
  fronts the daemon, whether the Spotify sign-in still works, and which room
  aliases no longer name a room.
  [`fcf3f53`](https://github.com/Dicklesworthstone/frankensonos/commit/fcf3f53),
  [`ac54a7e`](https://github.com/Dicklesworthstone/frankensonos/commit/ac54a7e),
  [`ab10f0c`](https://github.com/Dicklesworthstone/frankensonos/commit/ab10f0c),
  [`f99eb9a`](https://github.com/Dicklesworthstone/frankensonos/commit/f99eb9a)
- **An instant CLI.** When a daemon is running, CLI commands go through it,
  using its warm state, and are identified as the CLI by a token in the data
  directory. `fsonos sleep` goes through it too, so the group fades out with
  the speaker's own timer as a backstop; `--direct`, or no daemon, sets the
  speaker's timer alone.
  [`d1af970`](https://github.com/Dicklesworthstone/frankensonos/commit/d1af970),
  [`032dcbb`](https://github.com/Dicklesworthstone/frankensonos/commit/032dcbb)
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
- **Seek.** `fsonos seek Kitchen 1:30` (or `+30`, `-15`) moves within the
  track a room's group is on (`POST /seek`, the `seek` tool).
  [`49a594a`](https://github.com/Dicklesworthstone/frankensonos/commit/49a594a)
- **More reads on the CLI.** `fsonos zones <room>` prints the zone a room
  plays in, and `fsonos search <words>` lists the library's best matches (and
  a room's favorites, with `--zone`), as HTTP and MCP already had them.
  [`8a12050`](https://github.com/Dicklesworthstone/frankensonos/commit/8a12050)
- **An agent can keep up with the house.** The `events` tool waits for the
  house's next changes (`GET /events?wait=<secs>` answers once, as JSON).
  [`34614f8`](https://github.com/Dicklesworthstone/frankensonos/commit/34614f8)
- **The CLI from another tailnet machine.** `--daemon-url
  https://<mac>.<tailnet>.ts.net` sends the CLI to the daemon behind Tailscale
  Serve. Scenes, schedules and `policy show` reach the daemon too, and a
  command's notes (a clamped volume, a room a set skipped) print through it.
  [`4133bcd`](https://github.com/Dicklesworthstone/frankensonos/commit/4133bcd),
  [`408952c`](https://github.com/Dicklesworthstone/frankensonos/commit/408952c),
  [`db3189d`](https://github.com/Dicklesworthstone/frankensonos/commit/db3189d)
- **A ramp at the speakers' own pace.** `fade_volume` with no fade has each
  speaker ramp itself (`RampToVolume`), each bounded as an immediate change
  would be.
  [`43178a9`](https://github.com/Dicklesworthstone/frankensonos/commit/43178a9)
- **What the house played.** The history records what the live model
  hears, so music started in the Sonos app counts as well, once (music that
  moves to another room, or plays on across a daemon restart, is the same
  play), and it names what played: `fsonos history` shows "Title — Artist".
  A grouped room's zone state carries its group's volume.
  [`b4c716b`](https://github.com/Dicklesworthstone/frankensonos/commit/b4c716b),
  [`98d994b`](https://github.com/Dicklesworthstone/frankensonos/commit/98d994b),
  [`ccf5639`](https://github.com/Dicklesworthstone/frankensonos/commit/ccf5639),
  [`e4cf10e`](https://github.com/Dicklesworthstone/frankensonos/commit/e4cf10e)
- **A scene can play a Spotify link.** A scene group's `uri` source may be
  a Spotify track, album or playlist, played through the household's own
  account.
  [`6e18b31`](https://github.com/Dicklesworthstone/frankensonos/commit/6e18b31)

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
- **Direct tailnet callers are named.** A caller reaching the tailnet
  listener directly is identified by Tailscale WhoIs on its real socket peer,
  so its own policy applies instead of read-only; tailnet ACLs remain the
  boundary.
  [`f8d262b`](https://github.com/Dicklesworthstone/frankensonos/commit/f8d262b)

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
  - The S2 devmode statement is fully mapped from the daemon and its crypto
    library: magic, model, a version window (both bounds inclusive), device-id
    `memcmp`, payload and ring. Everything before the ring is hashed, the id
    included, so a public statement cannot be patched to another serial. The
    payload length is a byte, so `FUN_004f38a0`'s 256-byte frame is exact, and
    `POST /mtmhhsetup`'s full form shape, validation, error codes and
    persistence are decoded, with a Unicorn harness proving the map
    ([`docs/RE_DEVMODE.md`](docs/RE_DEVMODE.md)).
  - The updater's HTTP response parser (`wget` as root, pre-signature) is
    mapped against malformed frames, and two pre-auth request-data surfaces
    (`/status/ZPLocalSettingsFile/<name>`, `/mtmhhsetup`) are length-fuzzed
    ([`docs/UPDATE_HTTP_CLIENT.md`](docs/UPDATE_HTTP_CLIENT.md)).

### Fixed

A selection; each commit message has the details.

- Cached-only startup opens the event listener even when every remembered
  player is offline, so recovery works without a daemon restart.
- HTTP connections are served concurrently, so an open event stream never
  holds up other callers.
  [`d52e4f3`](https://github.com/Dicklesworthstone/frankensonos/commit/d52e4f3)
- Discovery's SSDP `M-SEARCH` and mDNS query leave every LAN interface, not
  only the one the kernel's multicast route picks, so a multi-homed host
  (speaker VLAN + main LAN + tailnet) finds players on all of them.
  [`6e83b3c`](https://github.com/Dicklesworthstone/frankensonos/commit/6e83b3c)
- mDNS accepts complete UDP responses, bounds hostile allocations and DNS name
  expansion, confines RDATA to its declared length, handles non-ASCII instance
  names without panicking, and applies alive/goodbye records in wire order so
  departed players and stale addresses disappear until a fresh advertisement.
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
- A player leaves its group by joining itself, the method real S1 and S2
  players accept.
  [`ff96a6d`](https://github.com/Dicklesworthstone/frankensonos/commit/ff96a6d)
- A lost GENA event is noticed by its sequence number, and the state it
  carried is read back.
  [`8352918`](https://github.com/Dicklesworthstone/frankensonos/commit/8352918),
  [`fb86361`](https://github.com/Dicklesworthstone/frankensonos/commit/fb86361)
- A refused pause is read back: an already paused group counts as done, and
  a live stream that cannot pause stops.
  [`9033e70`](https://github.com/Dicklesworthstone/frankensonos/commit/9033e70)
- A move confirms the hand-over, and copies when the target refuses it.
  [`f872086`](https://github.com/Dicklesworthstone/frankensonos/commit/f872086)
- A scene records the DJ where it plays, not where it is steered, and a
  manual volume change survives a sleep fade.
  [`f1543e3`](https://github.com/Dicklesworthstone/frankensonos/commit/f1543e3),
  [`7a2b435`](https://github.com/Dicklesworthstone/frankensonos/commit/7a2b435)
- A simulator's commands never reach the real house's daemon or store.
  [`6f69b55`](https://github.com/Dicklesworthstone/frankensonos/commit/6f69b55)
- A refused resume or skip says why and what to do, and the stdio MCP server
  hands announcements, scene saves and `sonos://dj` to a running daemon.
  [`2f1083e`](https://github.com/Dicklesworthstone/frankensonos/commit/2f1083e),
  [`7d8984c`](https://github.com/Dicklesworthstone/frankensonos/commit/7d8984c)
- One daemon per data directory: a second `fsonos serve` refuses to start
  while the first still answers, and a taken event port says so.
  [`202aca7`](https://github.com/Dicklesworthstone/frankensonos/commit/202aca7)
- A long call (an announcement, a fade, a DJ start waiting on Spotify, the
  doctor probing the LAN) no longer stalls the daemon's other requests: each
  runs on a thread of its own, and a DJ command or an undo holds the store
  only for each read or write. The DJ still waits for an undo to put things
  back.
  [`ff48be0`](https://github.com/Dicklesworthstone/frankensonos/commit/ff48be0),
  [`d499366`](https://github.com/Dicklesworthstone/frankensonos/commit/d499366),
  [`558bcba`](https://github.com/Dicklesworthstone/frankensonos/commit/558bcba),
  [`f2216e8`](https://github.com/Dicklesworthstone/frankensonos/commit/f2216e8),
  [`179c8ba`](https://github.com/Dicklesworthstone/frankensonos/commit/179c8ba)
- Sleep timers follow the music through a move or a regroup. After a
  restart the daemon takes up only its own timers, once it knows the house.
  A player that drops off mid-fade gets its volume back, and a cancel, an
  extend or a volume change just as a fade begins ends it.
  [`f98bb10`](https://github.com/Dicklesworthstone/frankensonos/commit/f98bb10),
  [`3507a62`](https://github.com/Dicklesworthstone/frankensonos/commit/3507a62),
  [`a2afb6e`](https://github.com/Dicklesworthstone/frankensonos/commit/a2afb6e),
  [`dd08a84`](https://github.com/Dicklesworthstone/frankensonos/commit/dd08a84),
  [`1f664d9`](https://github.com/Dicklesworthstone/frankensonos/commit/1f664d9),
  [`4ff10be`](https://github.com/Dicklesworthstone/frankensonos/commit/4ff10be)
- A stopping daemon finishes its fades instead of stranding them. A ramp
  fades every room in one fade, and a scene, an announcement or an undo ends a
  fade before it sets a volume.
  [`6a05e31`](https://github.com/Dicklesworthstone/frankensonos/commit/6a05e31),
  [`4615d18`](https://github.com/Dicklesworthstone/frankensonos/commit/4615d18),
  [`a01c70d`](https://github.com/Dicklesworthstone/frankensonos/commit/a01c70d)
- Found on real players: routes confine both halves of discovery, and one
  bad interface or player no longer breaks discovery or a leave. A leave is
  read back from a second player too (asked once, if it is unreachable), and
  an announcement's restore retries a transient 402 once.
  [`eb2a674`](https://github.com/Dicklesworthstone/frankensonos/commit/eb2a674),
  [`8b693d0`](https://github.com/Dicklesworthstone/frankensonos/commit/8b693d0),
  [`11d8ef9`](https://github.com/Dicklesworthstone/frankensonos/commit/11d8ef9),
  [`41f7602`](https://github.com/Dicklesworthstone/frankensonos/commit/41f7602),
  [`22d2b9e`](https://github.com/Dicklesworthstone/frankensonos/commit/22d2b9e),
  [`191bf05`](https://github.com/Dicklesworthstone/frankensonos/commit/191bf05)
- The DJ plays on once it can top up a queue that ran dry, never claims
  the owner's later copy of a work it played, and keeps topping up when a
  trim is refused. A scheduled or scene mood lasts until the time-of-day
  program changes, and a render the player refuses answers
  `RENDER_PARAMS_STALE`.
  [`872ab91`](https://github.com/Dicklesworthstone/frankensonos/commit/872ab91),
  [`8ad75aa`](https://github.com/Dicklesworthstone/frankensonos/commit/8ad75aa),
  [`4802399`](https://github.com/Dicklesworthstone/frankensonos/commit/4802399),
  [`e9e7738`](https://github.com/Dicklesworthstone/frankensonos/commit/e9e7738),
  [`835ce42`](https://github.com/Dicklesworthstone/frankensonos/commit/835ce42)
- A party no room could join fails, a group command refused by its old and
  new coordinator answers `NOT_COORDINATOR`, a scene group that keeps what it
  has leaves its playback alone, and a one-shot schedule fires once.
  [`e5b5d75`](https://github.com/Dicklesworthstone/frankensonos/commit/e5b5d75),
  [`f8704d2`](https://github.com/Dicklesworthstone/frankensonos/commit/f8704d2),
  [`9cebe26`](https://github.com/Dicklesworthstone/frankensonos/commit/9cebe26),
  [`ed7a35a`](https://github.com/Dicklesworthstone/frankensonos/commit/ed7a35a)
- The daemon: one per data directory, by a lock. Its doctor runs the
  store, LAN and alias checks, `FSONOS_TAILSCALE_SERVE=1` starts it, and the
  launchd job carries the Spotify client id the DJ needs. Over stdio, moves
  and fades go to it, and the web remote's mute follows its event stream.
  [`ec921ad`](https://github.com/Dicklesworthstone/frankensonos/commit/ec921ad),
  [`a7b4f85`](https://github.com/Dicklesworthstone/frankensonos/commit/a7b4f85),
  [`d9a0679`](https://github.com/Dicklesworthstone/frankensonos/commit/d9a0679),
  [`7b9d714`](https://github.com/Dicklesworthstone/frankensonos/commit/7b9d714),
  [`118b13c`](https://github.com/Dicklesworthstone/frankensonos/commit/118b13c),
  [`af5b714`](https://github.com/Dicklesworthstone/frankensonos/commit/af5b714)

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
- **The house policy holds however music starts.** Music an agent starts
  (a play, a resume, a DJ start, a join, a move, a party, or a scene or
  schedule it made) is held to the caps and quiet hours. A scene never sets a
  volume its writer may not. Undo goes through the policy, and only the CLI
  undoes others' actions. A fade on demand is authorized as the tool it is.
  [`86e9492`](https://github.com/Dicklesworthstone/frankensonos/commit/86e9492),
  [`f7945e3`](https://github.com/Dicklesworthstone/frankensonos/commit/f7945e3),
  [`a48454f`](https://github.com/Dicklesworthstone/frankensonos/commit/a48454f),
  [`231ab00`](https://github.com/Dicklesworthstone/frankensonos/commit/231ab00)
- **No borrowed identities.** The MCP listener admits only its own Host
  names, closing DNS rebinding there too. A process on the daemon's host is
  local, not the owner, on the tailnet, and a tailnet or Serve name never
  takes one of the daemon's own clients' keys. An unidentified caller can't
  make the doctor probe the LAN in a loop.
  [`37add3c`](https://github.com/Dicklesworthstone/frankensonos/commit/37add3c),
  [`f3803ed`](https://github.com/Dicklesworthstone/frankensonos/commit/f3803ed),
  [`b919d2a`](https://github.com/Dicklesworthstone/frankensonos/commit/b919d2a),
  [`268b764`](https://github.com/Dicklesworthstone/frankensonos/commit/268b764)

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
- **S1 Spotify rendering, on the S1 hardware.** The per-household render
  parameters were learned from the owner's own favorites and playback was
  verified on a real S1 player; the recorded parameters are in
  [`docs/PROTOCOL.md`](docs/PROTOCOL.md) §4
  ([`889f565`](https://github.com/Dicklesworthstone/frankensonos/commit/889f565)).
- **MCP resource updates.** The server publishes `notifications/resources/updated`
  for `sonos://zones`, `sonos://zones/{room}` and `sonos://dj`, with tests
  ([`014c1c5`](https://github.com/Dicklesworthstone/frankensonos/commit/014c1c5));
  GET /events and the HTTP stream remain the ordered alternative.
- **Not yet.**
  - Real-hardware CI and the `0.1.0` tag. Published release binaries start with
    `0.1.0`; per-push CI artifacts expire with the run.
  - Interfaces may change until `0.1.0`.

## Notes for agents

- **Start here:** `COMPREHENSIVE_PLAN_FOR_FRANKENSONOS.md`, then `AGENTS.md`
  (doctrine) and `docs/SCOPE.md` (the binding scope boundary).
- **Task graph:** `br ready` lists claimable work. The lanes are A (proto and
  sim), B (core), C (Spotify and the DJ) and D (surfaces), plus the Tailscale
  epic and an owner-directed research lane.
- **Build:** `cargo` is the gate and runs on the `rch` fleet; keep the pure
  unit tests green. Live-LAN tests are opt-in (`FSONOS_SEEDS=…`, `#[ignore]`).

[0.1.0]: https://github.com/Dicklesworthstone/frankensonos/releases/tag/v0.1.0
