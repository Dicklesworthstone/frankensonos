# Protocol fixtures

Real response bodies from the owner's own players, fetched with ordinary
read-only requests (`GET /xml/device_description.xml`, `GetZoneGroupState`,
`Browse`) on 2026-10-06, then scrubbed. Two households are represented: S1
(software `57.23`, display `11.16.1`) and S2 (`86.10` / `97.1`, display
`17.2.7` / `18.8`).

| File | Request | Notes |
|---|---|---|
| `zgs_s1.xml` | ZoneGroupTopology `GetZoneGroupState` (S1) | two stereo pairs grouped together, a group whose `ID` prefix is not its coordinator, two zone bridges |
| `zgs_s2.xml` | same (S2) | a stereo pair + sub whose visible member and sub are offline (`VanishedDevices`), leaving an `Invisible` coordinator |
| `browse_favorites_s1.xml` | ContentDirectory `Browse FV:2` (S1) | all 4 favorites, including a `<res/>` shortcut |
| `browse_favorites_s2.xml` | same (S2) | 11 of 45 favorites kept to cover each variant; `NumberReturned`/`TotalMatches` set to 11 |
| `browse_queue_s1.xml`, `browse_queue_s2.xml` | `Browse Q:0`, `RequestedCount=3` | first page of a longer queue (`TotalMatches` 10 / 8) |
| `browse_queue_empty.xml` | `Browse Q:0` on an idle player | empty DIDL-Lite |
| `soap_fault_701.xml` | `Browse` of a nonexistent object | HTTP 500 body, UPnP error 701 |
| `device_description_*.xml` | `/xml/device_description.xml` | Play:5 Gen 1 and Bridge (S1); Play:1 and One (S2) |
| `gena_notify_avt_initial_s1.xml` | AVTransport SUBSCRIBE initial NOTIFY (S1, office coordinator) | seq 0 full state; `CurrentTrackMetaData` holds the player's own SMAPI-fetched DIDL one escape level deeper |
| `gena_notify_avt_pause_s1.xml` | AVTransport NOTIFY on `Pause` (S1) | mid-transition snapshot: `TransportState` = `TRANSITIONING` |
| `gena_notify_rcs_volume_s1.xml` | RenderingControl NOTIFY on `SetVolume` (S1) | per-channel `<Volume channel="Master|LF|RF">` attributes |
| `gena_notify_zgt_s1.xml` | ZoneGroupTopology SUBSCRIBE initial NOTIFY (S1) | full `ZoneGroupState` as element text (not LastChange), plus `MuseHouseholdId`, `AvailableSoftwareUpdate` |
| `gena_notify_rcs_initial_s1.xml` | RenderingControl SUBSCRIBE initial NOTIFY (S1) | full render state: per-channel Volume/Mute, Bass/Treble, Loudness, EQ presets |
| `gena_notify_zgt_s2.xml` | ZoneGroupTopology SUBSCRIBE initial NOTIFY (S2) | full state plus the `AvailableSoftwareUpdate` firmware-URL oracle and three `VanishedDevices` |
| `gena_notify_queue_s1.xml` | Queue NOTIFY on `AddURIToQueue` (S1) | LastChange nests under `<QueueID val="0">` (not InstanceID); carries incrementing `UpdateID` |
| `gena_notify_grc_s1.xml` | GroupRenderingControl NOTIFY on `SetGroupVolume` (S1) | plain properties: `GroupVolume`/`GroupMute`/`GroupVolumeChangeable`, no LastChange |
| `gena_notify_cd_initial_s1.xml` | ContentDirectory SUBSCRIBE initial NOTIFY (S1) | NOT LastChange-wrapped: plain `SystemUpdateID`/`ContainerUpdateIDs`/`FavoritesUpdateID` counters |
| `gena_notify_avt_initial_s2.xml` | AVTransport SUBSCRIBE initial NOTIFY (S2) | idle player shape |
| `musicservices_list_s1.xml` | MusicServices `ListAvailableServices` (S1) | full service descriptor list; Spotify is `Id="12"`, `Auth="AppLink"`, SMAPI endpoint `spotify-v5.ws.sonos.com` |

## Scrubbing

Every site identifier and every piece of personal content was replaced by a
deterministic synthetic value, consistently across files (the same player has
the same synthetic id and address everywhere):

- player ids, MACs and serials → `RINCON_000E58A0xxxx01400` / `00:0E:58:A0:xx:xx`;
- IP addresses → `192.0.2.0/24` (TEST-NET-1);
- zone UUIDs → `00000000-0000-4000-8000-…`;
- room names → generic names (`Owner’s Study`, `Den`, `Lounge`, …; curly
  apostrophes kept where the original had one);
- titles, artists, albums, descriptions → `Title n`, `Artist n`, `Album n`;
- album-art URLs → `https://art.example.invalid/n.jpg`;
- Spotify and other service item ids → `0FixtureSpotify…` / `1000000n`;
- Spotify user ids in playlist URIs → `fixtureuseri`; search terms in
  parent ids → `query`.

Everything else (attribute order, escaping, service descriptors such as
`SA_RINCON3079_X_#Svc3079-0-Token`, `sid`/`flags`/`sn` values, item-id
prefixes, firmware versions) is byte-for-byte as the players sent it. The
escaped layers were decoded, scrubbed and re-encoded with an encoder checked
to be the exact inverse of the decoder on every original body.

`tests/golden.rs::fixtures_are_scrubbed` fails if any fixture contains a
private-range address, a household id, or a player id outside the synthetic
range. Keep it passing when adding fixtures.
