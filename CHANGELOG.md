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
so another agent can jump straight to the evidence. Nothing here describes a
shipped binary; see the per-item status notes.

## [Unreleased]

Pre-release groundwork toward `0.1.0`. The milestone target for `0.1.0` is
"see & control": deterministic discovery, live topology, and direct
play/pause/volume against a real LAN from the `fsonos` CLI (plan milestone M2).

### Added

- **Project foundation.** A single Rust 2024 Cargo workspace of seven crates
  (`fsonos-types`, `-proto`, `-core`, `-spotify`, `-api`, `-mcp`, `-cli`) that
  compiles green with pure-logic modules and unit tests, `#![forbid(unsafe_code)]`
  workspace-wide, a pinned `nightly-2026-08-31` toolchain, and a committed
  `Cargo.lock`. Includes the comprehensive plan, `AGENTS.md`, the in/out-of-scope
  boundary (`docs/SCOPE.md`), and the beads task graph (one `FND-DEPS` prerequisite
  plus four lane epics with dependency edges).
  [`0ecb761`](https://github.com/Dicklesworthstone/frankensonos/commit/0ecb761)
- **Protocol layer (`fsonos-proto`).** SSDP `M-SEARCH` discovery for
  `ZonePlayer:1`; UPnP/SOAP envelope + `SOAPACTION` construction and response/UPnP-fault
  parsing with a `Transport`-trait dispatch seam; a `roxmltree`-based XML helper;
  UPnP device-description parsing (UDN, room, model, `swGen` S1/S2 line, services);
  ZoneGroupTopology and ContentDirectory (`Browse`) parsers; and DIDL-Lite parsing
  plus `x-sonos-spotify:` URI construction whose per-household `sid`/`flags`/`sn`
  and `SA_RINCON…` descriptor are taken as parameters to be learned from a
  household's own favorites, never hard-coded.
- **Core model (`fsonos-core`).** The two-household state model with
  coordinator resolution, S1/S2 classification (authoritative `swGen` with an
  S1-only-hardware fallback, bridges excluded as non-renderers), topology folding
  into rooms (stereo-pair / home-theater aware), room resolution from what a
  person or agent types (case- and curly-apostrophe-insensitive, household- and
  player-id qualified), and a durable `Store` trait with an in-memory
  implementation.
- **Spotify library + classical DJ (`fsonos-spotify`).** A read-only Spotify Web
  API client surface — OAuth Authorization Code + PKCE (S256), `user-library-read`
  only, token requests, callback parsing with CSRF `state`, and Spotify-link
  canonicalization — plus a dependency-free, seedable DJ engine that picks for
  pleasant variety (anti-repeat with time + play cooldowns, composer/work/album
  spread, prolific-composer damping, and a time-of-day energy target).
  [`7e641c3`](https://github.com/Dicklesworthstone/frankensonos/commit/7e641c3)
- **Surfaces (`fsonos-api`, `fsonos-mcp`, `fsonos-cli`).** Shared request DTOs
  and validation reused by both the HTTP API and the MCP tools; a single
  `Failure` shape rendered as FastAPI-style `{"detail": …}` or MCP tool-error
  text; `source_uri` canonicalization (Spotify share links → `spotify:<kind>:<id>`);
  a bind guard for the unauthenticated listeners; and the `fsonos` CLI skeleton.
- **Deployment guide.** `docs/DEPLOY.md` with a launchd plist template and a
  Tailscale deployment model (the daemon is fronted on the tailnet; speakers stay
  on the LAN; tailnet ACLs restrict access), including the `FSONOS_*` environment
  contract.
  [`33c559b`](https://github.com/Dicklesworthstone/frankensonos/commit/33c559b),
  [`b16dd5e`](https://github.com/Dicklesworthstone/frankensonos/commit/b16dd5e)
- **Franken dependency integration (`FND-DEPS`).** Wired the owner's Rust stack —
  `asupersync` (async runtime, unified on the 0.5 line), `fsqlite` (local store),
  `fastmcp` (MCP server), and `fastapi` (HTTP API) — into the workspace via the
  proven version/git-rev recipe, with FND-DEPS proof tests for an fsqlite
  round-trip, an asupersync HTTP client↔server loopback, an MCP-over-stdio echo,
  and a fastapi `GET /health`.
- **Golden fixtures.** Scrubbed, synthetic S1/S2 device-description,
  ZoneGroupTopology, favorites/queue, and SOAP-fault response bodies, with golden
  tests that replay them through a recording in-memory `Transport`.
- **Live wire-protocol verification (RE lane).** `docs/PROTOCOL.md`: an
  11-section reference verified against the owner's real S1 and S2 players —
  the service/URL matrix (incl. the `schemas-sonos-com` Queue and
  `schemas-tencent-com` QPlay namespaces), the Spotify render template that
  resolves the UPnP 800 (the `SA_RINCON<svc>` DIDL `desc` is load-bearing;
  `parentID` is not), the queue-based continuous-playback flow, GENA
  subscription mechanics, the `AvailableSoftwareUpdate` firmware-URL oracle,
  and the `.upd` firmware container format (record-typed; S2 payload is a
  plain tar with `bin/anacapad`, S1 payloads encrypted and deliberately not
  pursued). SMAPI envelope/auth documented against the live endpoint, and the
  `x-rincon-mp3radio:` bypass path verified. All captures stay local and
  git-ignored; the repo carries only scrubbed prose and synthetic fixtures.
- **GENA NOTIFY parsing (`fsonos-proto::gena`).** `parse_propertyset` +
  `parse_last_change` with channel-aware accessors (`Master` volume/mute),
  transport-state and track-metadata conveniences, and tolerance for values
  arriving as element text (community-documented quirk) — proven against the
  live-captured NOTIFY corpus from both households (golden tests).

### Security / privacy

- The repository is public and deliberately carries **no** site-specific data —
  no real IPs, MAC addresses, serial numbers, household ids, tokens, captures, or
  personal room names. `.gitignore` excludes local inventories, captures, auth
  caches, and the beads database; test fixtures use synthetic identifiers; and
  example room names are generic.

## Notes for agents

- **Start here:** `COMPREHENSIVE_PLAN_FOR_FRANKENSONOS.md` (architecture, lanes,
  the dependency recipe, the Spotify-on-Sonos reality, milestones), then
  `AGENTS.md` (doctrine) and `docs/SCOPE.md` (the hard interoperability-only
  boundary).
- **Task graph:** `br ready` lists claimable work; the lane epics are A (proto),
  B (core), C (spotify/DJ), D (surfaces), gated behind `FND-DEPS`.
- **Build:** `cargo` is the gate (it offloads to the `rch` fleet); keep the pure
  unit tests green. Live-LAN behavior is opt-in behind a feature/env flag.

[Unreleased]: https://github.com/Dicklesworthstone/frankensonos/commits/main
