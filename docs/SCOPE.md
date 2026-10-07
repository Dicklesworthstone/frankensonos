# SCOPE — what FrankenSonos is and is not

This file is **authoritative** and binds every contributor, human or agent.
Read it before taking on any task. If a task seems to fall outside this scope,
stop and ask the owner rather than improvising.

## What this project is

FrankenSonos is a **local interoperability controller for Sonos hardware the
owner already possesses, on the owner's own LAN** — the same category of
software as the mature, widely-used open-source projects **SoCo** (Python),
**node-sonos** / **node-sonos-ts** (JavaScript), and the **Home Assistant Sonos
integration**. It talks to the owner's players using the devices' own,
community-documented local protocols (SSDP, UPnP/SOAP on port 1400, GENA
events) and reads the owner's own Spotify library through the official Spotify
Web API with the owner's own credentials.

This is legitimate, ordinary home automation. Owning the hardware and
controlling it on your own network is exactly what these protocols are for.

## In scope (build these)

- SSDP discovery of the owner's players; fetching and parsing their public
  device-description XML.
- UPnP/SOAP control requests to the owner's players (transport, volume,
  grouping, topology, content browse, queue) — the documented :1400 services.
- GENA event subscriptions to the owner's players (state push), with an HTTP
  callback sink the daemon hosts.
- DIDL-Lite metadata and `x-sonos-spotify:` URI construction, where the
  per-household parameters are **discovered from the household's own favorites
  at runtime** (the owner's own linked Spotify account) — not hard-coded, not
  extracted from anywhere else.
- Spotify **Web API** reads of the **owner's own** saved albums and liked
  tracks, using the owner's own OAuth (Authorization Code + PKCE) with scope
  `user-library-read`. Tokens stay in a local, git-ignored cache.
- The daemon, HTTP API, MCP server, CLI, DJ engine, local store, and the
  Tailscale deployment that fronts **the daemon** (never the speakers).

## Out of scope for the autonomous swarm (do not do these)

The following are **not** part of the agent swarm's backlog and must not be
started by an autonomous agent. Some are simply unnecessary (external UPnP
control already meets every project goal); all are the owner's call to pursue
deliberately and by hand if ever:

- **Firmware reflashing or modification** of any Sonos device, or building
  alternative firmware images. This risks permanently bricking expensive
  hardware the owner relies on, and is not needed for reliability, the DJ, or
  agent control. If ever explored, it is a separate, human-led research effort
  outside this repo's automated work.
- **Reverse-engineering, disassembling, or patching Sonos's compiled
  binaries.** Not required for interoperability; the local protocols are
  already community-documented.
- **Defeating, bypassing, or probing authentication, encryption, DRM, or access
  controls** on anything — Sonos services, Spotify, or otherwise.
- **Extracting, cracking, exfiltrating, or re-using credentials, tokens, or
  secrets** beyond the owner's own OAuth tokens held in the local cache for the
  owner's own accounts. We never lift secrets off the wire or out of other
  software; the only Spotify linkage we rely on is the one the owner
  established, in their own app, on their own account.
- **Anything targeting devices, networks, or accounts the owner does not
  control**, and any form of mass scanning or operation against third parties.
- **Capturing network traffic** as an engineering task for the swarm. (The
  owner may, entirely separately and by hand, observe their *own* network with
  ordinary tools for their own interoperability learning; that is the owner's
  prerogative and is not swarm work, not committed to this repo, and never
  includes captured secrets or personal data.)

## Owner-authorized reverse-engineering lane (exception, 2026-10-06)

Per Rule 0 (direct instructions), the owner has **explicitly and directly
instructed** a reverse-engineering lane, superseding the swarm-default
prohibitions above **only as enumerated here**:

- **Passive observation of the owner's own LAN**: capturing and analyzing
  traffic between the owner's own Sonos devices, apps, and controllers
  (SSDP, SOAP, GENA, SMAPI) with ordinary tools (`tcpdump`, `tshark`) to
  document the wire protocols — including read-only/anonymous calls to the
  same public endpoints the devices themselves use (e.g. the SMAPI service
  descriptor URLs). All captures are stored locally in a git-ignored
  directory and are never committed.
- **Static analysis of Sonos firmware and software binaries** for
  interoperability understanding (update mechanism, service architecture,
  protocol handlers), using disassemblers and debuggers. Findings are
  committed only as scrubbed prose documentation — never decompiled code,
  copyrighted firmware images, keys, or certificates.
- **Protocol parameters observed on the owner's own system** (e.g. SMAPI
  `sid`/`flags`/`sn` values, household and device identifiers): may be used
  at runtime to control the owner's own devices, learned at runtime from the
  owner's own devices — but never committed to this public repository.

These remain prohibited even under this exception:

- **Flashing, patching, or modifying device firmware** — brick risk on
  hardware the owner relies on. Firmware *study* is authorized; *writing* to
  devices stays human-led, one explicit approval per operation.
- **Defeating DRM or access controls on third-party services**, or accessing
  any account or device the owner does not control.
- **Committing** captures, secrets, serials, IPs, MACs, household IDs, or
  firmware images to git. The privacy rule below binds this lane unchanged.

## Privacy / publishability rule

This repository is **public**. No details of any one person's home network,
devices, or accounts belong in git: no real IPs, MAC addresses, serial numbers,
household ids, room lists, tokens, or packet captures. The code learns all of
that at runtime and stores it locally (git-ignored). Test fixtures captured from
real devices must have site-specific identifiers scrubbed before being
committed. See `.gitignore`.

## If in doubt

Ask the owner. "Could this be read as attacking something, or as handling
someone's secrets?" If yes, it's out of scope. The whole project succeeds by
being a clean, well-built interoperability controller — nothing here needs to
touch the lines above.
