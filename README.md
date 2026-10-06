# FrankenSonos

**Your Sonos, your way — a memory-safe Rust controller for the Sonos speakers
you already own, on your own network.**

FrankenSonos is a local controller and daemon that talks to Sonos players over
their own, long-documented local protocols (SSDP discovery, UPnP/SOAP on port
1400, GENA event subscriptions) — the same approach proven by
[SoCo](https://github.com/SoCo/SoCo),
[node-sonos](https://github.com/bencevans/node-sonos), and the
[Home Assistant Sonos integration](https://www.home-assistant.io/integrations/sonos/).
It gives you reliable discovery, grouping, and playback control, reads *your
own* Spotify library to act as a tasteful classical-music DJ, and exposes
everything through a CLI, an HTTP API, and an **MCP server** so your AI agents
can run the house — including from anywhere over [Tailscale](https://tailscale.com).

It is built to work past the frustrations of the official apps: it discovers
every speaker across both the legacy **S1** and current **S2** generations
deterministically, shows true group topology from live events instead of
guesswork, and plays what you tell it to.

## Why

Sonos hardware is expensive and long-lived, but the official software can be
unreliable and gets in the way of simple automation — and the Spotify Web API
famously cannot start playback on a Sonos at all. FrankenSonos does what the
mature open-source controllers do: it speaks the devices' own local control
protocols directly, so playback is driven locally and reliably, and layers an
agent-native surface and a DJ brain on top.

## What it does

- **Reliable discovery & topology.** Finds every player on your LAN (S1 + S2),
  with room, model, generation, and live group membership.
- **Direct control.** Play / pause / next / volume / group / ungroup, always
  addressed to the correct group coordinator.
- **Spotify DJ.** Reads your saved albums and liked tracks (your own account,
  read-only) and plays a varied classical stream with anti-repeat — no phone in
  hand. Sonos renders Spotify itself via its music-service integration; the
  Spotify Web API is used only to understand your taste.
- **Agent-native.** A JSON HTTP API and an MCP server (`list_zones`, `play`,
  `dj_start`, …) let Claude, Grok, Meta Muse, OpenAI agents, and others control
  the audio as tools.
- **Off-LAN over Tailscale.** The daemon is fronted on your tailnet; the
  speakers never leave the LAN.

## Architecture

A single Rust workspace (edition 2024, `#![forbid(unsafe_code)]`), built on the
`asupersync` async runtime with `fsqlite` for local state, `fastapi_rust` for
the HTTP API, and `fastmcp_rust` for the MCP server:

| Crate | Responsibility |
|---|---|
| `fsonos-types` | Shared domain vocabulary (players, groups, tracks) |
| `fsonos-proto` | SSDP, UPnP/SOAP, GENA events, DIDL-Lite / URIs |
| `fsonos-core` | Inventory, topology, grouping, orchestration, local store |
| `fsonos-spotify` | Spotify Web API library reads + the DJ engine |
| `fsonos-api` | HTTP control API |
| `fsonos-mcp` | MCP server (agent tools) |
| `fsonos-cli` | The `fsonos` binary — CLI and `serve` daemon |

See [`COMPREHENSIVE_PLAN_FOR_FRANKENSONOS.md`](COMPREHENSIVE_PLAN_FOR_FRANKENSONOS.md)
for the full design.

## Build

```bash
# Rust nightly is pinned via rust-toolchain.toml.
cargo build --release
cargo test --workspace
```

## Usage (as the CLI/daemon matures)

```bash
fsonos discover                               # list players on the LAN
fsonos zones                                  # show live group topology
fsonos play "Jeff's Office" spotify:track:... # render a track on a zone
fsonos dj start "Jeff's Office"               # start the classical DJ
fsonos serve --http 127.0.0.1:8099            # run the daemon (API + MCP + DJ)
```

## Scope & privacy

FrankenSonos controls **hardware you own, on your own network**, and reads
**your own** Spotify library with your own credentials. It does not touch device
firmware, reverse-engineer binaries, or handle anyone's secrets beyond your own
local OAuth cache. This repository is public and contains **no** site-specific
data (IPs, serials, tokens, room lists); all of that is learned at runtime and
stored locally. See [`docs/SCOPE.md`](docs/SCOPE.md).

## License

MIT License with an OpenAI/Anthropic rider — see [`LICENSE`](LICENSE).
(`LicenseRef-MIT-OpenAI-Anthropic-Rider`; not plain MIT.)
