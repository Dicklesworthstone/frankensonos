# Deploying FrankenSonos (launchd + Tailscale)

This guide runs the `fsonos` daemon on an always-on Mac on the speaker LAN
(for example a Mac mini). launchd keeps it running across reboots and crashes,
and Tailscale lets your agents reach it from anywhere on your tailnet. The
speakers stay on the LAN and are never fronted.

> **Status.** `fsonos serve` runs the HTTP API and the MCP server (streamable
> HTTP at `/mcp`) over the house policy, verified end to end against the
> built-in simulator. Not yet: the GENA event listener (state is read from
> the speakers on each call) and the DJ. Items marked *(planned)* do not
> exist yet.

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
   event deliveries only, not control requests. (Its port is set by the GENA
   lane, `a-gena`.)
5. **Browsers can't drive it.** A web page open on the Mac or a tailnet
   device could otherwise reach the unauthenticated API. So each listener
   admits only its own Host names (loopback, its address, the tailnet's
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
| HTTP API address | `FSONOS_HTTP_ADDR` | `127.0.0.1:8099` | Keep on loopback behind Tailscale Serve. |
| MCP (streamable HTTP) address | `FSONOS_MCP_HTTP_ADDR` | `127.0.0.1:8098` | Endpoint path `/mcp`. |
| Data directory | `FSONOS_DATA_DIR` | `~/Library/Application Support/fsonos` | Store DB and Spotify token cache. |
| Direct-seed list | `FSONOS_SEEDS` | unset | Optional file of player addresses for flaky-SSDP networks; every IP address in it is tried (e.g. TOML `players = ["192.0.2.10"]`, or one per line). Every command also takes `--seed <ip>`. Keep the file under `local/` or outside the repo. |
| Routes file | `FSONOS_ROUTES` | unset | Only for `fsonos sim`: maps the virtual players' advertised addresses to the loopback sockets that serve them, plus the simulator's SSDP target. `fsonos sim` writes it; real players need none. While it is set, `fsonos` reaches nothing the file does not name (other addresses and multicast are refused). |
| Spotify client id | `FSONOS_SPOTIFY_CLIENT_ID` | unset | Needed only for the DJ (see §5). |
| Spotify redirect URI | `FSONOS_SPOTIFY_REDIRECT_URI` | `http://127.0.0.1:8099/auth/spotify/callback` | Must match the URI registered for your Spotify app. |
| Log filter | `RUST_LOG` | `info` | `tracing` EnvFilter syntax. Logs go to stderr. |

**Bind guard**: `serve` refuses a wildcard
(`0.0.0.0`, `::`) or public bind address for the API or MCP server unless you
pass `--allow-unsafe-bind`. It logs a warning for a private-LAN address.
Loopback and tailnet (`100.64.0.0/10`, `fd7a:115c:a1e0::/48`) addresses are
accepted silently.

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

## 4. Run it under launchd

### Why a LaunchDaemon (recommended)

On macOS 15 and later, **Local Network privacy** gates the daemon's core work:
SSDP multicast and SOAP connections to the speakers. Per Apple's
[TN3179](https://developer.apple.com/documentation/technotes/tn3179-understanding-local-network-privacy),
macOS automatically allows local-network access for:

- launchd **daemons**;
- processes running as root;
- command-line tools run from Terminal or over SSH, including their children.

The exemption does **not** cover launchd **agents**. An agent gets the Local
Network alert, and its LAN traffic is blocked until someone approves it. A
headless Mac can't answer that prompt. Approval is also tracked by code
signature, which is unreliable for the ad-hoc-signed binaries `cargo`
produces, so a rebuild can trigger the prompt again.

A **LaunchDaemon** with a `UserName` key avoids all of that. It starts at boot
without anyone logging in and runs as your user, not root. Inbound connections
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
(`ThrottleInterval`). It also gets 20 s between SIGTERM and SIGKILL
(`ExitTimeOut`) so the daemon can cancel its event subscriptions cleanly.

### Alternative: a LaunchAgent

Use a LaunchAgent if you want the daemon to run only while you are logged in.
Delete the `UserName` key from the filled-in plist, put it in
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

## 5. Tailscale

### Recommended: Tailscale Serve in front of loopback

Enable **MagicDNS** and **HTTPS certificates** in the Tailscale admin console
(DNS page). Then, on the Mac:

```bash
tailscale serve --bg --https=443  http://127.0.0.1:8099   # HTTP API
tailscale serve --bg --https=8443 http://127.0.0.1:8098   # MCP
tailscale serve status
```

- The API is at `https://<mac>.<tailnet>.ts.net/` and the MCP endpoint at
  `https://<mac>.<tailnet>.ts.net:8443/mcp`.
- `--bg` configurations persist across reboots; `tailscaled` holds them.
- The daemon only ever binds loopback, so there is no boot-time race with
  Tailscale coming up.
- To remove one mapping, run `tailscale serve --https=8443 off`.
  `tailscale serve reset` clears **all** Serve config on the machine.
- For requests from user-owned devices, Serve adds `Tailscale-User-Login` and
  `Tailscale-User-Name` headers. The daemon ignores them today. They are the
  hook for per-user authorization later.

### Alternative: bind the tailnet address directly

Set `FSONOS_HTTP_ADDR` and `FSONOS_MCP_HTTP_ADDR` to the Mac's tailnet address
(`tailscale ip -4`) instead of loopback. Traffic is plain HTTP inside
WireGuard. The address must exist when the daemon binds. At boot the daemon
can start before Tailscale, fail to bind, and get restarted by launchd every
10 s until Tailscale is up. Serve avoids that, and it keeps the daemon
reachable on loopback for local agents.

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

### Spotify sign-in (one time, for the DJ) *(planned, `c-auth`)*

The OAuth redirect URI is loopback, `http://127.0.0.1:8099/auth/spotify/callback`.
Register exactly that URI for your app in the Spotify developer dashboard.
Spotify accepts plain `http` only for a loopback IP literal, not `localhost`.

Complete the sign-in in a browser on the Mac. From another machine, tunnel the
port first:

```bash
ssh -L 8099:127.0.0.1:8099 <mac>
```

Then open the authorize URL locally. The refresh token is cached under
`FSONOS_DATA_DIR`, never in the repo.

## 6. Verify and troubleshoot

```bash
sudo launchctl print system/$LABEL | grep -E 'state|last exit'   # want: state = running
grep 'fsonos serve: ready' ~/Library/Logs/fsonos/fsonos.log       # the bound addresses
curl -fsS http://127.0.0.1:8099/health                           # on the Mac
curl -fsS http://127.0.0.1:8099/zones                            # rooms and what they play
curl -fsS http://127.0.0.1:8099/openapi.json                     # every route, body and error code
tailscale serve status
curl -fsS https://<mac>.<tailnet>.ts.net/health                  # from another tailnet device
```

| Symptom | Likely cause |
|---|---|
| Discovery finds no players under a LaunchAgent; connects fail with "No route to host" | Local Network access denied or never approved. Approve it in System Settings, or switch to the LaunchDaemon. |
| Every call answers `NOT_READY` ("no rooms discovered yet") | The daemon found no players: SSDP is filtered on this network (set `FSONOS_SEEDS`), or Local Network access is missing (above). It keeps retrying; no restart needed. |
| Players found, but state never updates after changes made in the Sonos app | Event callbacks are blocked inbound. Check the Application Firewall (§4). |
| The log shows bind failures ("Can't assign requested address") right after boot | Direct tailnet bind started before Tailscale. It self-heals via `KeepAlive`; prefer Serve. |
| `launchctl bootstrap` fails with an I/O or permission error | Plist not `root:wheel` `0644`, or the job is already loaded. Run `bootout` first. |
| Tailnet clients time out but loopback works | Tailnet policy doesn't grant the port, or Serve isn't configured (`tailscale serve status`). |
