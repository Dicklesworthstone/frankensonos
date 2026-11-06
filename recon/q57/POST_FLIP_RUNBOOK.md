# q57 post-DNS-flip runbook (owner flipped router DHCP DNS → 192.168.4.160)

## Preconditions (all verified live 2026-11-06)
- DNS responder on 192.168.4.160:53 (root): `system-api.sonos.com` A → 192.168.4.160
  - check: `dig +short @192.168.4.160 system-api.sonos.com A` → 192.168.4.160
- HTTPS responder on 0.0.0.0:8643 (q57_https_server.py): dual-shape downgrade
  policy on `/product-settings/v3/settings` (200), `pwned-q57.upd` delivery
  (sha256 73587fac…)
- pf rdr: en0 tcp from {84,202,135,86,81} to 192.168.4.160:443 → :8643
  - check: `sudo pfctl -s nat | grep rdr`
- units: .84 (Play:1), .202 (Play:5 Office), .135 (Living Room), .86, .81

## Sequence (in order)
1. Confirm DNS renewal: `tail -f /tmp/q57-serve/dns.log` — expect
   `A system-api.sonos.com -> 192.168.4.160 from 192.168.4.{84,202}` within
   the lease half-life (force with a unit power cycle if impatient).
2. Settings fetch: `tail -f /tmp/q57-serve/requests.log` — expect
   `HIT GET GET /product-settings/v3/settings` from a unit IP. If nothing
   within ~30 min: the settings job runs on the deferred-shutdown path —
   trigger CheckForUpdate + BeginSoftwareUpdate (SOAP) on the unit, then
   watch again.
3. Arming check: poll `http://<unit>:1400/status/upgradeinfo` — expect
   `URL: ...` + `MinVersion: [57.16-41110]` lines (the writer appends only
   when armed).
4. Fire BeginSoftwareUpdate (Flags 0, then 1): the unit's /bin/upgrade
   fetches `http(s)://…/pwned-q57.upd` from us — confirmed by
   `UPD-SERVED /... 5606749 bytes` in requests.log.
5. Watch the unit: accepted-image reboot ~50-60s (+ /jffs wipe); the
   patched rootfs plants /jffs/system/Configure.dev → boot chain →
   telnetd 2323 + unlock flag. Verify: `nc -z <unit> 2323`, then SSH/telnet.
6. Rollback (if needed): the units' stock images are preserved in
   captures/firmware/; re-serve the STOCK .upd through the same responder
   path (rename/symlink) and re-run BeginSoftwareUpdate.

## Abort conditions
- Unit wedged >10 min: power-cycle (the c25-class S2 wedge recovers or
  clears on power cycle).
- `UPD-SERVED` never appears while upgradeinfo IS armed: the fetch host in
  upgradeinfo points elsewhere — read the URL line and adjust the
  responder's served names.

## rdr proof note
The pf rdr rules can only be exercised by an external source (a Sonos unit
or any LAN device whose traffic actually traverses en0) — Mac-local curl
bypasses en0 and hits whatever binds wildcard :443 directly. After the
flip, verify with `sudo pfctl -s info | grep -A2 rdr` (counter deltas) or
simply the requests.log HIT lines.
