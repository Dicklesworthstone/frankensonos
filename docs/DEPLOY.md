# Deploying FrankenSonos (launchd + Tailscale)

This guide runs the `fsonos` daemon on an always-on Mac on the speaker LAN
(for example a Mac mini). launchd keeps it running across reboots and crashes,
and Tailscale lets your agents reach it from anywhere on your tailnet. The
speakers stay on the LAN and are never fronted.

> **Status.** `fsonos serve` runs the HTTP API and the MCP server (streamable
> HTTP at `/mcp`) over the house policy, keeps a live model of the speakers
> from their GENA events, and drives the DJ over your whole Spotify library
> (steer, status, why, moods, feedback), scenes, sleep timers and schedules, announcements,
> quiet hours, and `fsonos doctor` — all verified end to end against the
> built-in simulator. The behaviour here is simulator-verified; there is no
> real-hardware CI yet, and the live Tailscale Serve round trip is opt-in
> because it records the MagicDNS name in public CT logs (§5).

## 1. Shape of the deployment

```text
 tailnet device (agent, phone, laptop)
        │  HTTPS over WireGuard, admitted by your tailnet policy
        ▼
 Tailscale Serve on the Mac ── https://<mac>.<tailnet>.ts.net      → 127.0.0.1:8099  HTTP API
                            └─ https://<mac>.<tailnet>.ts.net:8443 → 127.0.0.1:8098  MCP (/mcp)
        │
 fsonos serve (launchd)
        │  SSDP multicast, SOAP to :1400        ▲  GENA NOTIFY event callbacks
        ▼                                       │  (speakers → daemon, LAN only)
 Sonos players on the LAN ──────────────────────┘
```

Six rules hold the design together:

1. **Reachability is control.** The HTTP API and the MCP server have no
   authentication of their own: anyone who can reach them can play, pause and
   regroup your speakers. They bind **loopback**, and Tailscale Serve plus your
   tailnet policy decide who else gets in.
2. **Tailscale fronts the daemon, never the speakers.** Do not advertise the
   speaker LAN as a subnet route (`tailscale set --advertise-routes=…`). That
   would put every speaker's unauthenticated port 1400 on the tailnet.
3. **Never `tailscale funnel`** these ports. Funnel publishes to the public
   internet.
4. **The GENA callback listener is the only LAN-facing socket.** The speakers
   must be able to reach the daemon to deliver state-change events. It accepts
   event deliveries only, not control requests, on `FSONOS_EVENTS_PORT`
   (default 8097) at the Mac's address facing the speakers.
5. **Browsers can't drive it.** A web page open on the Mac or a tailnet
   device could otherwise reach the unauthenticated API. So each listener
   (the API and MCP) admits only its own Host names (loopback, its address, the tailnet's
   MagicDNS name and addresses), defeating DNS rebinding. A request with a
   foreign `Origin` gets 403, and every control request must be
   `Content-Type: application/json` (415 otherwise), which closes the
   no-preflight cross-origin POST. No CORS grant is ever sent. CLI tools and
   agents send no `Origin` and are unaffected.
6. **No site data in git.** Filled-in plists, tailnet policies, seed lists,
   logs and the data directory all live outside the repository.

## 2. Configuration

`fsonos serve` reads its configuration from flags or the matching environment
variables. The environment form is what launchd uses.

| Setting | Env var | Default | Notes |
|---|---|---|---|
| HTTP API address | `FSONOS_HTTP_ADDR` | unset: `127.0.0.1:8099` plus this host's tailnet addresses when Tailscale is up | Set it to bind exactly one address, e.g. loopback behind Tailscale Serve. |
| MCP (streamable HTTP) address | `FSONOS_MCP_HTTP_ADDR` | unset: `127.0.0.1:8098` | Endpoint path `/mcp`. Loopback unless set: reach it from the tailnet through Serve. |
| Tailscale detection | `FSONOS_TAILSCALE` | `auto` | `off` (or `--tailscale off`) stops `fsonos` looking for Tailscale: unconfigured listeners bind loopback only, and `fsonos doctor` skips its `tailscale.*` checks. |
| Tailscale Serve at startup | `FSONOS_TAILSCALE_SERVE` | off | `true` (or `--tailscale-serve`) runs `fsonos tailscale setup` when the daemon starts (§ "Recommended: Tailscale Serve"); a refusal or failure is logged and the daemon serves on. |
| Events port | `FSONOS_EVENTS_PORT` | `8097` | Where the speakers deliver state-change events (GENA), on the Mac's LAN address. The only LAN-facing socket: allow it inbound (§4). `0` picks any free port. |
| Data directory | `FSONOS_DATA_DIR` | `~/Library/Application Support/fsonos` | Store DB (including the players last found) and Spotify token cache. |
| Direct-seed list | `FSONOS_SEEDS` | `seeds.toml` in the data directory, when present | Optional file of player addresses for flaky-SSDP networks; every IP address in it is tried (e.g. TOML `players = ["192.0.2.10"]`, or one per line). `fsonos setup` writes the default one when multicast fails but the players answer. Every command also takes `--seed <ip>`. Keep the file under `local/` or outside the repo. A successful seed-only survey saves the players even on the first run, so removing the seed file keeps remembered discovery available. When discovery and the seeds find nothing, the players the last survey found (remembered in the store) are tried too. |
| Routes file | `FSONOS_ROUTES` | unset | Only for `fsonos sim`: maps the virtual players' advertised addresses to the loopback sockets that serve them, plus the simulator's SSDP target. `fsonos sim` writes it; real players need none. While it is set, `fsonos` reaches nothing the file does not name (other addresses and multicast are refused): it asks only a daemon running under the same file, and unless `FSONOS_DATA_DIR` says otherwise it keeps its data in `data` beside the file, never the real house's store. |
| Spotify client id | `FSONOS_SPOTIFY_CLIENT_ID` | `spotify.toml` in the data directory, else unset | Needed only for the DJ (see §5); `fsonos setup` saves it there. |
| Spotify redirect URI | `FSONOS_SPOTIFY_REDIRECT_URI` | `http://127.0.0.1:8099/auth/spotify/callback` | Must match the URI registered for your Spotify app. |
| Log filter | `RUST_LOG` | `info` | `tracing` EnvFilter syntax. Logs go to stderr. The embedded database's own logging (per-statement telemetry at `info`) is held to errors unless the filter names `fsqlite`. |

**Bind guard**: `serve` refuses a wildcard
(`0.0.0.0`, `::`) or public bind address for the API or MCP server unless you
pass `--allow-unsafe-bind`. It logs a warning for a private-LAN address.
Loopback addresses, and this Mac's own tailnet addresses as the `tailscale`
CLI reports them, are accepted silently. Another address in the tailnet
ranges (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`), or any of them while the CLI
does not answer, gets a warning: carrier-grade NAT (a phone hotspot, some
ISPs) hands out addresses from the same IPv4 block.

## 3. Build and install

Install to one stable path. The launchd job, the macOS firewall and the
privacy records all key on the executable's location.

```bash
cargo build --release -p fsonos-cli          # or offload via rch
install -d ~/.local/bin
install -m 0755 target/release/fsonos ~/.local/bin/fsonos
~/.local/bin/fsonos --version
mkdir -p ~/Library/Logs/fsonos "$HOME/Library/Application Support/fsonos"
```

To upgrade later, rebuild, re-run the `install` line, and restart the job with
`kickstart -k` (§4).

No toolchain on the Mac? Every push to `main` builds a release binary for
Apple Silicon (and x86_64 Linux) in CI, the "Mac and Linux binaries"
workflow; download it instead of building:

```bash
gh run download --repo Dicklesworthstone/frankensonos \
  --name fsonos-aarch64-apple-darwin \
  $(gh run list --repo Dicklesworthstone/frankensonos --workflow binaries.yml \
      --branch main --status success --limit 1 --json databaseId -q '.[0].databaseId')
tar -xzf fsonos-*-aarch64-apple-darwin.tar.gz
install -m 0755 fsonos-*-aarch64-apple-darwin/fsonos ~/.local/bin/fsonos
```

macOS keeps its Local Network approval (§4) per code signature, and a CI or
`cargo` build is ad-hoc signed, so each new build would need approving
again. Sign it with your own Developer ID once installed, and the approval
carries over between builds:

```bash
codesign --force --sign "Developer ID Application: <you>" \
  --identifier io.github.dicklesworthstone.fsonos ~/.local/bin/fsonos
```

## 4. Run it under launchd

> **macOS 26: check that launchd can reach the speakers.** On a Mac running
> macOS 26.2 (2026-10-14), `fsonos serve` installed as the LaunchDaemon below
> could not reach a single speaker: every connection failed at once with "No
> route to host (os error 65)", and so did a ten-line Rust program run the
> same way, while Apple's `curl` and an approved Python got through. macOS
> keeps Local Network approvals per code identity, and a daemon cannot show
> the prompt that would grant one. The `fsonos` binary now carries an
> `Info.plist` (a bundle identifier and the usage string) so it has an
> identity to approve; sign it with your own Developer ID (§3) so the approval
> survives rebuilds, then approve it in System Settings > Privacy & Security >
> Local Network. Until `fsonos doctor` passes when run by launchd
> (`sudo launchctl kickstart -k system/$LABEL`, then `GET /doctor`), run
> `fsonos serve` from a Terminal session instead: it inherits the terminal's
> approval, and keeps running when detached (`nohup fsonos serve &`), though
> not across a reboot.

### Why a LaunchDaemon (recommended)

On macOS 15 and later, **Local Network privacy** gates the daemon's core work:
SSDP multicast and SOAP connections to the speakers. Per Apple's
[TN3179](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy),
macOS automatically allows local-network access for:

- launchd **daemons**;
- processes running as root;
- command-line tools run from Terminal or over SSH, including their children.

On macOS 26.2 the first of these did not hold (the note above): the daemon
reached no speaker until its code identity was approved. Check with
`fsonos doctor` run by launchd rather than trusting the exemption.

The exemption does **not** cover launchd **agents**. An agent gets the Local
Network alert, and its LAN traffic is blocked until someone approves it. A
headless Mac can't answer that prompt; a Mac that stays logged in at its
console can, once (not yet tried with `fsonos`). Approval is also tracked by
code signature, which is unreliable for the ad-hoc-signed binaries `cargo`
produces, so a rebuild can trigger the prompt again; a Developer ID
signature (§3) keeps it.

A **LaunchDaemon** with a `UserName` key is still the shape to use. It starts
at boot without anyone logging in and runs as your user, not root. Inbound connections
(the speakers' event callbacks) need no Local Network privilege in either mode.

### Install the LaunchDaemon

The template lives at
[`docs/launchd/io.github.dicklesworthstone.fsonos.plist`](launchd/io.github.dicklesworthstone.fsonos.plist).
launchd does not expand `~`, so fill in absolute paths:

```bash
LABEL=io.github.dicklesworthstone.fsonos
sed -e "s|__USER__|$(id -un)|g" -e "s|__HOME__|$HOME|g" \
    docs/launchd/$LABEL.plist > "${TMPDIR:-/tmp}/$LABEL.plist"
plutil -lint "${TMPDIR:-/tmp}/$LABEL.plist"
# LaunchDaemon plists must be root:wheel and not group/world-writable.
sudo install -m 0644 -o root -g wheel "${TMPDIR:-/tmp}/$LABEL.plist" /Library/LaunchDaemons/
sudo launchctl bootstrap system /Library/LaunchDaemons/$LABEL.plist
```

Day-to-day management:

```bash
sudo launchctl print system/$LABEL | grep -E 'state|pid|last exit'
sudo launchctl kickstart -k system/$LABEL     # restart (e.g. after an upgrade)
sudo launchctl bootout system/$LABEL          # stop and unload
tail -f ~/Library/Logs/fsonos/fsonos.log
```

The job sets `KeepAlive` to restart on any exit, at most once every 10 s
(`ThrottleInterval`). It also gets 90 s between SIGTERM and SIGKILL
(`ExitTimeOut`): announcements in flight put the music back (the daemon waits
up to 60 s for them), then it cancels its event subscriptions cleanly.
A DJ that was playing keeps playing across a restart: serve keeps its queues
in `dj-feeds.json` in the data directory and feeds them on, unless the queue
was cleared or replaced meanwhile. Sleep timers carry over the same way, in
`sleep-timers.json`: each still fades out and pauses on time, unless its
speaker's own timer was cancelled meanwhile (from the Sonos app, say).

### Alternative: a LaunchAgent

Use a LaunchAgent if you want the daemon to run only while you are logged in.
`fsonos service install` writes one for you, with this binary's absolute
path, your data directory, the log, your shell's `PATH` (so the service finds
the programs you do), and the flags and environment you give it (the same as
`serve`'s). `--load` starts it, or restarts it if it runs, unless another
daemon serves the data directory:

```bash
fsonos service install --tailscale-serve --dry-run   # print it; change nothing
fsonos service install --tailscale-serve --load      # write it and start it
fsonos service start                                 # start it as installed, or restart it
fsonos service install --tailscale-serve --selftest-daily 04:30 --load  # and a daily self-test
fsonos service status                                # installed, running, pid, answering
fsonos service uninstall                             # stop it and remove only its file
```

The file holds the flags given when it was written, and nothing else: to
change one setting (adding `--spotify-client-id`, say), install again with
every flag you want and `--load`.

`--selftest-daily HH:MM` adds a second job that runs the inaudible `fsonos
selftest` at that local time each day (launchd: at the next wake when the
Mac slept through it), logging to `selftest.log` beside `fsonos.log`. Each
report is kept in the data directory (the newest 14 under `selftest/`), and
`fsonos doctor` warns (`selftest.last`) when the newest is a failure, or is
over two days old while the daily run is scheduled; `GET /health` carries
the newest's time and verdict. Pick a time the Mac is usually awake: a run
launchd makes up at wake can start before the Wi-Fi is back.

On Linux the same commands write and manage a systemd user unit,
`~/.config/systemd/user/fsonos.service` (`loginctl enable-linger` keeps it
running when you are logged out).

By hand instead: delete the `UserName` key from the filled-in plist, put it in
`~/Library/LaunchAgents/`, and manage it in the `gui/$(id -u)` domain:

```bash
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/$LABEL.plist
launchctl kickstart -k gui/$(id -u)/$LABEL
```

Approve the Local Network prompt on first run (System Settings → Privacy &
Security → Local Network). `serve` is designed to keep running when discovery
fails, and that matters: TN3179 notes that macOS skips the alert for a process
that exits right after its first failed local-network operation. Expect to
re-approve after rebuilds. The Mac also needs a logged-in GUI session (auto-login) after
a power cut.

### Foreground (development)

`fsonos serve` in Terminal or over SSH needs no launchd and is automatically
allowed local-network access.

### macOS Application Firewall

If the firewall is on, it can block the speakers' event callbacks, and live
state then goes stale. This is a separate gate from Local Network privacy.
Allow the binary:

```bash
sudo /usr/libexec/ApplicationFirewall/socketfilterfw --getglobalstate
sudo /usr/libexec/ApplicationFirewall/socketfilterfw --add ~/.local/bin/fsonos
sudo /usr/libexec/ApplicationFirewall/socketfilterfw --unblockapp ~/.local/bin/fsonos
```

A port-based firewall (pf, or a third-party one) must admit inbound TCP on
the events port (`FSONOS_EVENTS_PORT`, default 8097) from the speakers'
subnet. Nothing else needs to be reachable from the LAN.

## 5. Tailscale

### Recommended: Tailscale Serve in front of loopback

Enable **MagicDNS** and **HTTPS certificates** in the Tailscale admin console
(DNS page). Then, on the Mac:

```bash
fsonos tailscale setup      # both mappings below; running it again changes nothing
fsonos tailscale status     # what Serve does on 443 / 8443, and the URLs
fsonos tailscale teardown   # remove the daemon's mappings (only those)
```

Port 443 is the **web remote**, so the Serve root (`/`) opens the house's
browser remote, not a directory listing: open `https://<mac>.<tailnet>.ts.net/`
on any tailnet device. `/mcp` on 8443 is the agent endpoint.

`setup` refuses while **Funnel** is on for either port (Funnel would publish
the unauthenticated API to the public internet; FrankenSonos never uses it),
and never replaces Serve config it did not make. `--dry-run` shows the
commands. By hand, the same thing is:

```bash
tailscale serve --bg --https=443  http://127.0.0.1:8099   # HTTP API
tailscale serve --bg --https=8443 http://127.0.0.1:8098   # MCP
tailscale serve status
```

- The API is at `https://<mac>.<tailnet>.ts.net/` and the MCP endpoint at
  `https://<mac>.<tailnet>.ts.net:8443/mcp`.
- `--bg` configurations persist across reboots; `tailscaled` holds them.
- The daemon only ever binds loopback, so there is no boot-time race with
  Tailscale coming up: its loopback listeners answer any `*.ts.net` Host, the
  name Serve forwards, even if Tailscale was down when the daemon started.
- The HTTPS certificate is issued for the Mac's MagicDNS name, and issued
  certificates are recorded in public Certificate Transparency logs: the
  machine and tailnet names become public (nothing else does).
- To remove one mapping, run `tailscale serve --https=8443 off`.
  `tailscale serve reset` clears **all** Serve config on the machine.
- For requests from user-owned devices, Serve adds `Tailscale-User-Login` and
  `Tailscale-User-Name` headers. With `FSONOS_TAILSCALE_SERVE` set, the HTTP
  API and MCP take `Tailscale-User-Login` on their loopback listeners as the caller:
  a `[clients."<login>"]` table in `policy.toml` applies to that person, and
  the action log names them. Requests from tagged devices carry no login and
  stay `loopback-http`. Any process on the Mac that can reach the loopback
  port could send the header too, so grant a login no more than you trust
  the Mac's own processes with. MCP tools, resources, subscriptions, action
  history, and undo use that caller's policy and ownership.

### Alternative: listen on the tailnet directly

Leave `FSONOS_HTTP_ADDR` unset and `fsonos serve` listens on loopback **and**
on every tailnet address of the Mac whenever Tailscale is up at startup; the
startup log prints the URLs (MagicDNS first) for tailnet devices. Traffic is
plain HTTP inside WireGuard. Two things to know:

- On a direct tailnet listener, Tailscale names each caller by its address
  (WhoIs: the user's login, or a tagged device's tag such as `tag:agent`),
  and the house policy treats it as that client. Identified callers get
  every tool by default, still volume-capped; a `[clients."<login>"]` or
  `[clients."tag:<name>"]` table in `policy.toml` narrows that. A caller
  Tailscale can't name is `unknown`, which the default policy keeps
  read-only. A process on the Mac itself that calls the Mac's own tailnet
  address is `loopback-http`, as over loopback, never the owner's login.
  Serve traffic arrives on loopback and uses `loopback-http` unless
  `FSONOS_TAILSCALE_SERVE` enables the caller headers described above.
  The house policy and volume caps still apply.
- The tailnet addresses are read once, at startup. If the daemon starts
  before Tailscale is up, it listens on loopback only and logs why; restart it
  (`launchctl kickstart -k`) once Tailscale is up. Serve avoids this.
  `fsonos doctor --only tailscale` checks the whole chain: Tailscale up and
  logged in, the MagicDNS name, and the daemon answering over the tailnet,
  with the connect URLs or the fix. With Serve in front, `tailscale.serve`
  checks it fronts both listeners, that Funnel is off for them, and that the
  daemon names Serve's callers (`FSONOS_TAILSCALE_SERVE`); it reads Serve's
  config and sends nothing through it.

The MCP server stays on loopback unless `FSONOS_MCP_HTTP_ADDR` is set.
An explicitly configured direct tailnet MCP listener resolves each connection's
peer through the same WhoIs rules as the HTTP API. Multiple MCP listeners share
the live house state, while each request uses its own caller identity and MCP
sessions remain bound to that identity. A CLI token never grants native MCP
requests the local CLI or stdio caller's rights.

### Restrict who can reach it (tailnet policy)

A new tailnet's default policy lets all of your devices reach each other. To
limit the daemon to specific people or agent machines, merge grants like these
into your policy file (HuJSON). Note that grants only **add** access: while
the default allow-all rule is still present, these restrict nothing. Narrow
that rule first, and keep whatever other access you rely on, such as SSH to
the Mac.

```jsonc
{
  "tagOwners": { "tag:agent": ["autogroup:admin"] },
  // An alias for the Mac; replace with its `tailscale ip -4`. A host alias,
  // not a tag, so the Mac keeps belonging to you.
  "hosts": { "fsonos-host": "100.101.102.103" },
  "grants": [
    // You: the API and MCP from any of your devices.
    { "src": ["you@example.com"], "dst": ["fsonos-host"], "ip": ["tcp:443", "tcp:8443"] },
    // Agent machines tagged tag:agent: MCP only.
    { "src": ["tag:agent"],       "dst": ["fsonos-host"], "ip": ["tcp:8443"] }
  ]
}
```

If you bind the tailnet address directly, grant `tcp:8099` and `tcp:8098`
instead.

### Point your agents at it

```bash
# Claude Code, any tailnet machine (streamable HTTP):
claude mcp add --transport http fsonos https://<mac>.<tailnet>.ts.net:8443/mcp

# A local agent on the Mac itself, over HTTP (shares the daemon's live state):
claude mcp add --transport http fsonos http://127.0.0.1:8098/mcp

# A local agent that only speaks stdio:
claude mcp add fsonos -- ~/.local/bin/fsonos mcp
```

The MCP endpoint speaks the current MCP protocol era (`2026-07-28`); a client
that only speaks an older streamable-HTTP revision may be refused there, in
which case use `fsonos mcp` over stdio on the Mac. Other MCP clients take the
same URL, typically as
`{"mcpServers": {"fsonos": {"type": "http", "url": "https://<mac>.<tailnet>.ts.net:8443/mcp"}}}`.
Plain HTTP clients use the API directly, e.g.
`curl https://<mac>.<tailnet>.ts.net/zones`.

The `fsonos` CLI on another tailnet machine sends every command to the Mac's
daemon by its URL: `fsonos --daemon-url https://<mac>.<tailnet>.ts.net zones`,
or `export FSONOS_DAEMON_URL=https://<mac>.<tailnet>.ts.net` once. It answers
as that machine's tailnet login under the house policy, never surveys the LAN
it is on, and fails rather than runs directly when the daemon does not answer.
`fsonos events` streams only on the daemon's own host.

Over stdio, `fsonos mcp` hands the tools that need the daemon's running state to
`fsonos serve` whenever it runs: `dj_start`, `dj_steer`, `dj_skip`, `dj_stop`,
`dj_status`, `apply_scene` and the sleep-timer tools act on serve's DJ (which
keeps its queues fed) and serve's fading timers; `move_playback` goes to serve
so its DJ and sleep timer follow the music, and `fade_volume` and
`fade_and_pause` so a fade ends serve's other fades of those rooms. They reach
serve as the `mcp-stdio` client, so `[clients."mcp-stdio"]` in `policy.toml`
applies either way. Without a running serve (or with `--direct`), `fsonos mcp`
does them itself: the DJ queues its first works but nothing tops the queue up,
and a sleep timer is the speaker's own, without a fade. Reads and the other
controls answer the same either way.

### Moving an agent off an earlier HTTP bridge

An agent written against a small home-made Sonos bridge (a `soco` script
answering `GET /status?zone=`, `POST /play {zone, uri}` and the like) moves to
FrankenSonos route by route. Its calls then go through the house policy, land
in the action log and can be undone. Simplest is to give the agent the MCP
endpoint above: the tools take the same room names. Over HTTP, the routes map
like this (fsonos refuses unknown body fields, so rename them):

| Bridge | FrankenSonos HTTP | MCP tool | Differences |
|---|---|---|---|
| `GET /health` | `GET /health` | | `{status, version}` instead of `{ok, devices}`; `GET /players` lists the players |
| `GET /zones` | `GET /zones` | `list_zones` | an array of zones (coordinator, members, room names) |
| `GET /status?zone=R` | `GET /zones/R/state` | `get_zone_state` | `transport_state` is lowercase (`playing`, `paused`, `stopped`); `mute` not `muted`; the track sits under `track` (`title`, `creator` for artist, `album`, `position_secs`, `duration_secs` in seconds, not `H:MM:SS`) |
| `GET /mediainfo?zone=R` | `GET /zones/R/state` | `get_zone_state` | `track.uri` is the playing track's URI, where the bridge gave the transport's (`x-rincon-queue:…` while the queue plays); the raw DIDL metadata is not exposed |
| `GET /favorites?zone=R` | `GET /favorites?zone=R` | `list_favorites` | an array of `{id, title, kind, ...}`; play one by title with `POST /play/favorite {zone, favorite}` (`play_favorite`), no URI or metadata to round-trip |
| `POST /play {zone, uri, title}` | `POST /play {zone, source_uri, title}` | `play` | `uri` becomes `source_uri`: a `spotify:<kind>:<id>` URI, an `open.spotify.com` link or a renderer URI (a stream, say) |
| `POST /pause`, `/resume`, `/next`, `/previous` `{zone}` | the same routes, `{zone}` | `pause`, `resume`, `next`, `previous` | answers `{done, changed, notes}` instead of `{ok, zone}` |
| `POST /volume {zone, level}` | `POST /volume {zone, volume}` | `set_volume` | `level` becomes `volume`; `delta` adjusts instead, `group: true` sets the group's; the policy may clamp it (a note says so) |
| `POST /mute {zone, muted}` | `POST /mute {zone, mute}` | `mute` | `muted` becomes `mute` |
| `POST /rediscover` | none | | the daemon keeps its picture of the house current on its own |

Errors come back as `{code, detail, hint, ...}` with the HTTP status
`docs/ERRORS.md` lists (an unknown room is `UNKNOWN_ROOM`, 404), not
`{error}`. Give the agent its own identity: reach the daemon through Tailscale
Serve as its own tailnet login (or tag), and give that a
`[clients."<login>"]` table in `policy.toml` with the caps it should live
under. Run both side by side for a while, then stop the old bridge's launchd
job (`launchctl bootout gui/$(id -u)/<its label>`) and keep its files until
you are sure.

### Spotify sign-in (one time, for the DJ)

Run `fsonos setup` once on the Mac: it walks you through linking your own
Spotify account (Authorization Code + PKCE, requesting `user-library-read` plus
the read-only taste scopes `user-follow-read`, `user-top-read`,
`user-read-recently-played` and `playlist-read-private`; without the taste
scopes the DJ uses your library alone and `fsonos doctor` warns) and caches the
refresh token under `FSONOS_DATA_DIR`, never in the repo. The DJ reads through
a Spotify app of your own: create one at https://developer.spotify.com/dashboard
with the redirect URI above, and paste its client id when `fsonos setup` asks
(for PKCE the client id is not a secret). Setup saves it as `spotify.toml` in
the data directory, where serve reads it at every start, so neither the plist
nor a shell needs `FSONOS_SPOTIFY_CLIENT_ID` (which still wins when set). A
daemon already running when the app is saved picks it up when it next
starts. Without an app serve never refreshes the DJ's library, `fsonos doctor`
warns, and while it runs `fsonos dj sync` and the `dj_sync` tool answer
`NOT_IMPLEMENTED`; serve logs this at startup. `fsonos doctor` checks
that the sign-in still works (`dj.sign_in` reads one liked track, refreshing
the token as serve would) and says when to sign in again.

Under the hood the OAuth redirect URI is loopback,
`http://127.0.0.1:8099/auth/spotify/callback`. Register exactly that URI for
your app; Spotify accepts plain `http` only for a loopback IP literal, not
`localhost`. Complete the sign-in in a browser on the Mac. From another
machine, tunnel the port first, then open the authorize URL locally:

```bash
ssh -L 8099:127.0.0.1:8099 <mac>
```

## 6. Verify and troubleshoot

```bash
sudo launchctl print system/$LABEL | grep -E 'state|last exit'   # want: state = running
grep 'fsonos serve: ready' ~/Library/Logs/fsonos/fsonos.log       # the bound addresses
grep 'fsonos serve: live' ~/Library/Logs/fsonos/fsonos.log        # households found, events address
curl -fsS http://127.0.0.1:8099/health                           # on the Mac
curl -fsS http://127.0.0.1:8099/zones                            # rooms and what they play
curl -fsS http://127.0.0.1:8099/openapi.json                     # every route, body and error code
curl -NsS http://127.0.0.1:8099/events                           # live changes (server-sent events)
tailscale serve status
curl -fsS https://<mac>.<tailnet>.ts.net/health                  # from another tailnet device
```

| Symptom | Likely cause |
|---|---|
| Discovery finds no players under a LaunchAgent; connects fail with "No route to host" | Local Network access denied or never approved. Approve it in System Settings, or switch to the LaunchDaemon. |
| Every call answers `NOT_READY` ("no rooms discovered yet") | The daemon found no players: SSDP is filtered on this network (set `FSONOS_SEEDS`), or Local Network access is missing (above). It keeps retrying; no restart needed. |
| Players found, but state never updates after changes made in the Sonos app | Event callbacks are blocked inbound: `GET /doctor` shows `daemon.live` with no event subscriptions. Check the Application Firewall and the events port (§4). |
| The log shows bind failures ("Can't assign requested address") right after boot | A tailnet address set in `FSONOS_HTTP_ADDR`, bound before Tailscale was up. It self-heals via `KeepAlive`; prefer Serve, or leave the address unset. |
| The log shows `store backend error: ... No space left on device`; recent actions are missing from `fsonos log` and cannot be undone | The data directory's volume is full: the daemon keeps serving while every store write fails. `fsonos doctor --only store` (`store.disk`) shows the free space and how much the data directory holds, and `GET /health` carries `disk`. Make room on that volume; fsonos deletes nothing itself. |
| `launchctl bootstrap` fails with an I/O or permission error | Plist not `root:wheel` `0644`, or the job is already loaded. Run `bootout` first. |
| Tailnet clients time out but loopback works | Run `fsonos doctor --only tailscale` on the Mac: it says whether Tailscale is up and whether the daemon listens on the tailnet. If both pass, the tailnet policy doesn't grant the port, or Serve isn't configured (`tailscale serve status`). |
