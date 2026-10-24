# RE_ZONEICON — does an S2 player fetch the `SetZoneAttributes` icon URL?

Question: if `DesiredIcon` points at our server, does the player GET it (and so expose its PNG/JPEG parser
pre-auth)? Tested on `.81` and `.86` with a byte-logging server on `192.168.4.160:8270`.

Action is on **DeviceProperties** (`DesiredZoneName, DesiredIcon, DesiredConfiguration`), not
ZoneGroupTopology.

## Result: the URL is stored and never fetched

| step | `.86` | `.81` |
|---|---|---|
| `SetZoneAttributes` with `DesiredIcon=http://192.168.4.160:8270/i.png` | 200 | 200 |
| any GET on the byte server within 26 s | **none** | **none** |
| change to `…/i2.png` (does a *change* trigger a fetch?) | 200, no GET | — |
| ~40 s idle afterwards | no GET | — |
| `GetZoneAttributes` | returns `CurrentIcon` = our URL (stored) | — |
| `/status/zp` | `<ZoneIcon>` = our URL | — |
| `/xml/device_description.xml` | **no occurrence of the URL** — the description keeps its built-in `/img/icon-S12.png` | — |
| `/img/icon.png`, `/img/zoneicon.png`, `/getzoneicon` | 404 (no proxy either) | — |

So the value round-trips through `GetZoneAttributes` and the status page, but the player neither fetches it,
proxies it, nor advertises it in the description. With no fetch there is nothing to feed a malformed
PNG/JPEG to, so **the image-parser shapes (truncated, huge IHDR, `0xffffffff` length) stay untested on this
path** — they are not reachable through `SetZoneAttributes` as 81100 stands.

## What could still flip it

- **Reboot.** Not tested: `/reboot` is 403 on S2 with both GET and POST, and no unauthenticated restart path
  was found. The icon survives in the settings store, so a reboot is the one remaining trigger worth trying
  the moment a restart is available.
- **Topology change.** Attempted with `RemoveZonePlayer`/`AddZonePlayer` (both 500 on `.86`, which is a slave
  in a group — they likely belong on the coordinator), so a group change is unproven as a trigger.

## Left on the devices

`DesiredIcon` cannot be cleared: sending it empty is accepted (200) but `GetZoneAttributes` still returns the
old URL, so `.81` and `.86` remain pointed at `http://192.168.4.160:8270/i2.png`. Harmless, and it keeps the
reboot test armed. Server script `captures/research/iconsrv.py` (valid 1×1 PNG served as `i.png`/`i2.png`).
