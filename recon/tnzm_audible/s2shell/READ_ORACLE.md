# S2 (86.10-81100) read-surface map — CORRECTED 2026-11-06

## The verdict on /status/ZPLocalSettingsFile/<name>

It is the **/status/<name> dispatcher passthrough**, NOT a /jffs file read.

Live evidence (.135, 2026-11-06):
- `ZPLocalSettingsFile/../VERSION` ≡ `/status/VERSION` (ZPSupportInfo, `File name='/VERSION'`, body = 86.10-81100)
- `../zp`, `../ifconfig`, `../enetports`, `../wireless`, `../showstp`, `../proc/ath_rincon/status` → 200 with real status-table content
- `../etc/passwd`, `../etc/shadow`, `../k`, `../app/log/upgrade.log`, `../localsettings.txt` → **empty ZPSupportInfo** (111B envelope) — only names in the status-handler table resolve
- `../../VERSION` → 404 (≥2 `..` hops rejected); `./VERSION`, `../VERSION/`, `../%2e%2e/VERSION` → 200 (same passthrough)
- The 2026-10-29 note ("`../VERSION` → 86.10-81100, one `..` honored = /jffs") misread this passthrough as a settings-root file read.

## Consequence for the campaign

- This route is **not** a payload-log read-back channel. The dropbear-matrix
  feedback design pivots to **self-reporting payloads**: bind a port (nc -e
  shell, dropbear), or beacon out (`wget` from the child), or change polled
  state (/status fields, crash/reboot oracle).
- Pre-auth reads that DO exist and are useful: the whole `/status/*` table
  (`/status/zp` = zone/serial/fw/HwFlags; `/status/upgradeinfo` for the
  arming predicate; `/support/review`).
- The S1 57.23 route of the same name was never shown to read /jffs either
  (the q57 2026-10-24 note says the S1 read side is `..`-accepting on
  `/status/ZPLocalSettingsFile` — same class of passthrough to re-verify
  there before relying on it).
