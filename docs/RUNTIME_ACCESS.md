# Runtime access surfaces (RE lane, live-verified)

Companion to [`PROTOCOL.md`](PROTOCOL.md) and [`FIRMWARE_PATHS.md`](FIRMWARE_PATHS.md). Everything
here is reachable from the LAN with no Sonos account, no developer program and no vendor key.
Placeholders: `<player>` = a player's LAN address, `<host>` = the controller host, `<port>` = a
chosen TCP port. Firmware generations differ; each item names the generation where it was seen.

## The boot-time root path (the goal)

The shipped boot script executes a file from writable flash:

```sh
# /etc/Configure, before the network is up
if [ -f /etc/unlocked_build_flag -o -e /jffs/system/Configure.dev ]; then
    ln -s /opt/htdocs_locked /tmp/htdocs_locked
    touch /tmp/device_unlocked_flag
fi
if [ -f /jffs/system/Configure.dev ]; then
    sh /jffs/system/Configure.dev
fi
```

and `dropbear` is an init-respawned job (`/etc/scripts/run_sshd.sh`) that blocks on the unlock flag:

```sh
waitwhiletrue "[ ! -f /tmp/device_unlocked_flag ]"
exec /usr/bin/dropbear -R -F          # host key /jffs/persist/ssh, keys /jffs/sys/debug/ssh
```

So **one file at `/jffs/system/Configure.dev` = root at the next boot plus an SSH daemon**, and it
also satisfies the unlock gate that the devmode statement otherwise has to satisfy. The whole
problem reduces to a file write with a caller-controlled path into `/jffs/system/`.

## Local API on `:1443` (TLS, S2)

- A key header is required but not secret: `X-Sonos-Api-Key: Sonos` (any value passes; only presence
  and a minimum length are checked).
- The path parser also requires `?protocolVersion=<n>`; without it every request fails with
  `ERROR_INVALID_PARAMETER "Parsing terminated:protocolVersion"`.
- Read routes are open (`/api/v1/players/local/info`, `/time/relative`); settings, update and
  `localContentLibrary/*` are behind an `Authorization: Bearer` device token.
- **Unauthenticated UPnP proxy.** `POST /api/v1/players/local/upnp{DeviceProperties,SystemProperties,
  ZoneGroupTopology}?protocolVersion=1` executes a SOAP action on the player. No token:

  ```json
  {"method":"GetZoneGroupState"}
  {"method":"SetString","input":[{"name":"VariableName","value":"<key>"},
                                 {"name":"StringValue","value":"<value>"}]}
  ```

  Required member order is `method`, then `input` as an array of `{name,value}` pairs in SOAP
  argument order. An `input` object (rather than array) fails with the misleading
  `Parsing terminated:input.name`. Wrong argument names reach the SOAP layer and return UPnP 800 in
  a 499 envelope.
- Consequence: the whole `SystemProperties` string store is readable and writable unauthenticated.
  The binary carries 69 `O_*` override keys (cloud/update/diagnostic/voice URLs, intervals,
  `O_ZONE_EXPERIMENTS`). `CheckForUpdate` does **not** consume `O_UPDATE_SERVER_URL`; that key is
  consumed inside the updater only.

## SMB client from an unauthenticated GET (`/getaa`)

`GET /getaa?u=x-file-cifs://<host>:<port>/<share>/<file>&w=N&h=N` makes an S2 player open an SMB2
session to the host and read the album-art file. The client is libsmb2 inside `anacapad` (its wire
ClientGuid is ASCII `libsmb2-<build>`), authenticates as `guest`, and issues NEGOTIATE / SESSION_SETUP
/ TREE_CONNECT / CREATE / QUERY_INFO / READ (fixed 16 KiB). No share has to be configured first.

Consequences: a rogue SMB server is reachable without touching the owner's library configuration,
and the READ reply parser can be driven directly. On a build whose libsmb2 predates the 2026
hardening the reply's `DataOffset`/`DataLength` are unchecked; on the newer builds the offset must
equal the canonical one and an over-advertised length only stalls the read.

## Denial of service (works on current builds)

`anacapa.conf` ships `NumThreads 4`, `MaxConn 4`, `TimeOut 10`: four worker threads serve all HTTP.
Any unauthenticated handler that blocks longer than the timeout parks a worker, and five of them
stop the daemon answering on `:1400` **and** `:1443` while SSDP and audio keep running. Two known
blocking handlers:

- `GET /getaa?u=x-file-cifs://…` against a peer that completes the handshake and never answers the
  READ — each request pins one thread until that socket dies.
- `POST /setstring` on the S1 line (`{key,value,csrfToken}`) hangs and does not persist on that
  build; write settings through the API proxy instead.

No watchdog recovers it; a power cycle is required. Recovery is immediate once the offending sockets
close (killing the server is enough), so the wedge only becomes permanent when the sockets are held.

## Firmware scripts that gate convenience features

- Serial console: `/proc/sonos-lock/console_enable == '1'` gates the ttyS0 getty; `secure_console.sh`
  then auto-logs-in root (`login -f root`) — no password once the lock is lifted.
- `/unlock`, `/devunlock`, `/mfgunlock` create `/tmp/device_unlocked_flag` and reboot; a per-device
  attempt budget answers `Too Many Unlocks`.
- Devmode: `POST /devmode` verifies an RSA-PKCS1v15 signed TLV (magic `0x28721E71`, device id inside
  the digest) against keys in `libsonoscrypto`; `button=delete` removes a statement unsigned.

## What this build lacks (model/line differences)

On the Play:1 S2 (86.10-81100) the following newer routes are absent from the binary and answer
404/500: `/putDSP`, `/save_eq_presets`, `/setPersistentEQ`, `/customsd`, `/indexrepl`, `/msprox`,
`/pcap`, `/ssh/authorized_keys`; `/logger` and `/setstring` exist only on the S1 line; `/reboot`
is 403-gated. A `/status/...` exec page is a command runner (`/bin/ps`, `/bin/dmesg`, …) but is empty
on both tested lines.

## Status of the file write (both candidate routes are signature-gated)

There is no caller-controlled path write: every fixed target of the HTTP writers is a name in `/tmp`
or a compiled-in `/jffs` path, and the album-art cache does not persist to disk under the requested
basename. Neither route therefore creates `/jffs/system/Configure.dev` on its own:

- **Update package.** `BeginSoftwareUpdate` fetches any URL without authentication and parses before
  verifying, but an unsigned package never reaches the type-3 preinstall script; it is rejected and
  the player reboots. An RSA signature over the package (see
  [`UPDATE_FEED_AND_SIGNATURE.md`](UPDATE_FEED_AND_SIGNATURE.md)) is required, and a script that
  writes `Configure.dev` gets root the moment a valid package installs.
- **Devmode statement.** `/devmode` accepts a correctly signed TLV, needs nothing but the network,
  and creates the unlock flag (so `dropbear` starts) — again gated by the vendor key.

So the shortest path to root remains a package that plants `/jffs/system/Configure.dev`, which needs a
package signature the vendor issues; the alternative is forging the devmode statement.
