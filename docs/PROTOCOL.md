# FrankenSonos protocol notes — the Sonos local wire protocol, verified live

Everything here was verified against real S1 (firmware 57.23) and S2 (86.10,
97.1) players on the owner's LAN on 2026-10-06/07, or is marked as community
knowledge. No site-specific identifiers appear in this file: `<IP>` stands for
a player address, `RINCON_<MAC>01400` for a player UUID, `Sonos_<ID>` for a
household ID.

## 1. Discovery and addressing

- SSDP M-SEARCH to `239.255.255.250:1900`, ST `urn:schemas-upnp-org:device:ZonePlayer:1`.
  Multicast reaches only one household per scan on some networks (whichever
  answers first); scan twice, or fall back to direct seeds: GET
  `http://<IP>:1400/xml/device_description.xml` on candidate IPs.
- All control is HTTP on TCP **1400**. Device description:
  `GET /xml/device_description.xml`. Status pages: `GET /status`,
  `/status/zp`, `/status/VERSION` (build string), `/status/ifconfig`,
  `/status/proc/ath_rincon/status` (SonosNet radio diagnostics).
- Player UUID: `RINCON_<MAC>01400` (MAC without colons). This is the
  `udn`/`UUID` used in topology and GENA subscription IDs.
- Group commands MUST go to the group's **coordinator**; members reject or
  mis-handle coordinator verbs. Coordinator comes from ZoneGroupTopology.

## 2. Service matrix (from live device descriptions)

| Service | S1 Play:5 (S5) | S2 Play:1/One (S1/S12/S13) | S1 Bridge (ZB100) |
|---|---|---|---|
| AVTransport | ✅ | ✅ | ❌ |
| RenderingControl | ✅ | ✅ | ❌ |
| GroupRenderingControl | ✅ | ✅ | ❌ |
| ContentDirectory | ✅ | ✅ | ❌ |
| MusicServices | ✅ | ✅ | ❌ |
| Queue | ✅ | ✅ | ❌ |
| GroupManagement | ✅ | ✅ | ✅ |
| ZoneGroupTopology | ✅ | ✅ | ✅ |
| DeviceProperties | ✅ | ✅ | ✅ |
| SystemProperties | ✅ | ✅ | ✅ |
| AlarmClock | ✅ | ✅ | ❌ |
| ConnectionManager | ✅ | ✅ | ❌ |
| VirtualLineIn | ✅ | ✅ | ❌ |
| QPlay | ✅ | ✅ | ❌ |
| AudioIn | ✅ (line-in hardware) | ❌ | ❌ |

Control URL pattern: `/<Category>/<Service>/Control`, event URL
`/<Category>/<Service>/Event`. Categories: `MediaRenderer` (AVTransport,
RenderingControl, Queue?, ConnectionManager), `MediaServer` (ContentDirectory),
top-level for ZoneGroupTopology, DeviceProperties, SystemProperties,
GroupManagement, MusicServices, AlarmClock, GroupRenderingControl, AudioIn,
VirtualLineIn, QPlay. (Read the exact URLs from each description; do not
guess.)

## 3. SOAP

- Envelope: standard UPnP. Header `SOAPACTION: "urn:schemas-upnp-org:service:<Service>:1#<Action>"`,
  body `<u:<Action> xmlns:u="urn:schemas-upnp-org:service:<Service>:1">`.
- Errors: `HTTP 500` with `<UPnPError><errorCode>`. Observed:
  - **714** `Illegal MIME-Type` — wrong URI scheme for the service (e.g. a raw
    or percent-encoded `spotify:` URI instead of `x-sonos-spotify:`).
  - **800** — generic AVTransport failure; observed when the DIDL `desc`
    content-description element does not match the service (`SA_RINCON3079…`
    required for Spotify). Also reported by the community for stale service
    linkage.


S2 additionally exposes a cloud-style REST API on **TCP 1443** (TLS,
self-signed): probing `/api/v1/players/local/info` on the owner's One returns
`403 ERROR_API_KEY_VALIDATION_FAILED` — key-gated, unlike the open :1400
UPnP surface. FrankenSonos therefore standardizes on :1400 for both
generations (the WS API some S2 projects use was not listening on the
owner's players).

## 4. Spotify rendering (verified live on both households, 2026-10-07)

Sonos renders Spotify through its SMAPI integration, not the Spotify Web API.
The speaker itself fetches the stream after `SetAVTransportURI` + `Play` on the
group coordinator.

SMAPI descriptor (MusicServices → `ListAvailableServices`, control URL
`/MusicServices/Control`): Spotify is **service Id 12** in both households,
`Uri=https://spotify-v5.ws.sonos.com/smapi`, `Policy Auth="AppLink"`. The Id is
the `sid` URI parameter.

URI template (track):

```
S1: x-sonos-spotify:spotify%3atrack%3a<TRACK_ID>?sid=12&flags=8224&sn=<SN>
S2: x-sonos-spotify:spotify%3Atrack%3A<TRACK_ID>?sid=12&flags=8232&sn=<SN>
```

- `%3a` vs `%3A` case differs between generations' own favorites (both are
  accepted by both; match the household's own convention).
- `flags`: 8224 (0x2020) observed on S1 favorites, 8232 (0x2028) on S2.
  **Learn it from the household's own favorites at runtime**; do not hardcode.
- `sn`: the SMAPI account serial for Spotify in that household (observed `1`
  on both of the owner's). Learn from favorites or the Accounts data.

DIDL-Lite metadata (`CurrentURIMetaData`):

```xml
<DIDL-Lite xmlns:dc="http://purl.org/dc/elements/1.1/"
 xmlns:upnp="urn:schemas-upnp-org:metadata-1-0/upnp/"
 xmlns:r="urn:schemas-rinconnetworks-com:metadata-1-0/"
 xmlns="urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/">
 <item id="10032020spotify%3atrack%3a<TRACK_ID>" parentID="-1" restricted="true">
  <dc:title>TITLE</dc:title>
  <upnp:class>object.item.audioItem.musicTrack</upnp:class>
  <desc id="cdudn" nameSpace="urn:schemas-rinconnetworks-com:metadata-1-0/">SA_RINCON3079_X_#Svc3079-0-Token</desc>
 </item>
</DIDL-Lite>
```

- The **`desc` is load-bearing**: `SA_RINCON3079_X_#Svc3079-0-Token` where 3079
  is Spotify's SMAPI service type (2311 = world variant, 3079 = US; learn the
  household's from its favorites). A wrong desc against the household's
  registered type yields UPnP **800** at/after SetAVTransportURI (800's
  documented meaning: "command not supported or not a coordinator" — it also
  fires for wrong `sid`; sonos2mqtt issue #59 is the canonical repro).
- Item id prefix: `10032020` (S1 favorites) / `10032028` (S2 favorites) both
  work; third-party implementations also succeed with `00032020` (SoCo
  sharelink, gilbert). `sid` is the service `Id` from `ListAvailableServices`
  (household-specific; 12 on the owner's, 9 in some published captures) —
  always learn it, never hardcode. `flags` is an undocumented bitmask
  (8224 track / 8300 container observed); SoCo omits it entirely with success.
- `parentID`: S1 accepts `-1` (verified live); S2's own favorites use the
  item's own id (self-referential). Community implementations all use `-1`.
- `dc:title` is display-only; the speaker replaces the DIDL with full SMAPI
  metadata (real title/artist/album/art) once playback starts — visible in
  the next AVTransport LastChange event.
- A single `SetAVTransportURI` track plays to completion, then the transport
  goes `STOPPED` (no implicit continuation) — use the queue flow below.

### Queue flow (the DJ's continuous-playback pattern, verified live on S1)

```
AddURIToQueue(InstanceID=0,
              EnqueuedURI="spotify%3atrack%3a<ID>",     # bare: NO x-sonos-spotify: scheme
              EnqueuedURIMetaData=<DIDL item id="00032020spotify%3atrack%3a<ID>" parentID="-1"
                                    class musicTrack, desc SA_RINCON3079_X_#Svc3079-0-Token>,
              DesiredFirstTrackNumberEnqueued=0,        # append
              EnqueueAsNext=0)                          # -> returns FirstTrackNumberEnqueued
SetAVTransportURI(InstanceID=0, CurrentURI="x-rincon-queue:<COORD_UUID>#0", CurrentURIMetaData="")
Seek(InstanceID=0, Unit=TRACK_NR, Target=<returned position>)
Play(InstanceID=0, Speed=1)      # -> PLAYING; Next advances within the queue
```

Verified 2026-10-07 on S1 57.23: two tracks appended at positions 11–12 of an
existing queue, `Seek`→`Play`→`Next` all PLAYING, then the two tracks removed
with `RemoveTrackFromQueue` (`ObjectID=Q:0/<n>`, descending order) restoring
the queue exactly. Album/playlist containers enqueue with
`x-rincon-cpcontainer:1004206c<enc>` (albums) / `1006206c<enc>` (playlists).

Album/container favorites use `x-rincon-cpcontainer:1004206c<enc-uri>` with
`flags=8300` and a container DIDL (`object.container.album.musicAlbum` via the
`resMD` of the favorite).

Arbitrary HTTP streams also render directly: `x-rincon-mp3radio://host/path`
via `SetAVTransportURI` (class `object.item.audioItem.audioBroadcast`) played a
public MP3 radio stream on the S1 group on first try (verified 2026-10-07).
This is the fallback/bypass path for sources with no SMAPI integration: serve
the audio locally, point any speaker at it.

## 5. Favorites (ground truth for render params)

`ContentDirectory#Browse` with `ObjectID=FV:2`, `BrowseFlag=BrowseDirectChildren`,
control URL `/MediaServer/ContentDirectory/Control`. Each favorite item:

```xml
<item id="FV:2/N" parentID="FV:2" restricted="false">
 <dc:title>NAME</dc:title>
 <upnp:class>object.itemobject.item.sonos-favorite</upnp:class>
 <r:ordinal>N</r:ordinal>
 <res protocolInfo="sonos.com-spotify:*:audio/x-spotify:*">x-sonos-spotify:…?sid=12&amp;flags=8224&amp;sn=1</res>
 <upnp:albumArtURI>…</upnp:albumArtURI>
 <r:type>instantPlay</r:type>
 <r:description>By ARTIST</r:description>
 <r:resMD>ESCAPED-DIDL-TO-USE-VERBATIM</r:resMD>
</item>
```

`r:resMD` holds the exact DIDL the app passes to `SetAVTransportURI` —
double-escaped inside the SOAP `Result`. This is the canonical way to learn a
household's render parameters.

## 6. Topology

`ZoneGroupTopology#GetZoneGroupState` returns escaped XML:
`<ZoneGroups><ZoneGroup Coordinator="RINCON_…" ID="RINCON_…:N">` containing
`<ZoneGroupMember UUID=… ZoneName=… Location="http://<IP>:1400/xml/device_description.xml" …/>`.
Solo players and Bridges appear as single-member groups; `:0` group IDs are
invisible/satellite entries. Room names may contain non-ASCII (curly
apostrophes) — normalize quotes when matching by name.

## 7. GENA events

- `SUBSCRIBE http://<IP>:1400/<EventURL>` with headers
  `CALLBACK: <http://<CONTROLLER_IP>:<PORT>/cb>`, `NT: upnp:event`,
  `TIMEOUT: Second-300`. Response carries `SID: uuid:RINCON_<MAC>01400_subNNNNN`.
- Speaker then POSTs `NOTIFY /cb` per change; body is
  `<e:propertyset><e:property><LastChange>ESCAPED-XML</LastChange>…`.
- Initial NOTIFY (SEQ 0) carries full state; later NOTIFYs carry changed
  variables only. Renew before the timeout; `UNSUBSCRIBE` with the `SID`.
- AVTransport LastChange root: `<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/">`
  with `<InstanceID val="0">` children: `TransportState`, `CurrentTrackURI`,
  `CurrentTrackDuration`, `CurrentTrackMetaData` (DIDL, triple-escaped at the
  wire level), `AVTransportURI`, `CurrentTransportActions`, …
- RenderingControl LastChange root: `…/RCS/`, children like
  `<Volume channel="Master" val="19"/>`, plus `LF`/`RF` fixed at 100.
- ZoneGroupTopology NOTIFYs carry the full current `ZoneGroupState` (not a
  diff) — a single subscription anywhere in a household tracks all grouping.

## 8. Network notes for the daemon host

- Players may sit on multiple subnets; the controller host can be dual-homed
  (the owner's Mac reaches the speaker VLAN via `en1` while `en0` carries
  default traffic). Bind GENA callbacks to the interface that routes to the
  players, and put that address in `CALLBACK`.
- SonosNet (S1 mesh) operates on 2.4 GHz channel 11 (2462 MHz) in the owner's
  deployment; S1 players report `WM: 0` (SonosNet), S2 `WM: 1` (Wi-Fi).

## 9. Firmware (study lane)

Builds observed: S1 `57.23-74170` (frozen line), S2 `86.10-80260` and
`97.1-80312`. `/status/VERSION` returns `<VER>-<BUILD>`. Analysis tracked in
bead `frankensonos-re-firmware-kds`; images stay local (gitignored).

Update mechanism (verified 2026-10-07):

- Players learn update URLs out of band; the `ZoneGroupTopology` initial
  NOTIFY carries `AvailableSoftwareUpdate` → `<UpdateItem … UpdateURL=…
  ManifestURL=… Swgen="2"/>`. The S1 household's element is empty (frozen
  line); the S2 household's pointed at the current GA train.
- Manifests are plain-HTTP XML at
  `http://update.sonos.com/firmware/Prod/<train>-<token>-<GA|RC|LR>-<n>/update.upm`:
  `update_manifest` with `system_version`, `base_url` containing a `^<ver>`
  filename template, `swgen`, and per-model `<image model=…>URL</image>`
  upgrade-chain entries (players step through intermediate builds; e.g.
  34.16 → 55.1 → 57.5/57.19 → 73.0 → 86.10 → current). The manifest for the
  owner's S2 GA train listed 61 images incl. controller `.exe`/`.dmg`,
  app-store redirects, and headphone DFU payloads.
- Images: `http://update-firmware.sonos.com/firmware/Prod/<ver>-v<mkt>-<token>-<GA|RC|LR>-<n>/<ver>-1-<model>.upd`,
  no auth, CloudFront-fronted, old builds not garbage-collected. Numeric
  model ids (Play:1 = 12, One = 13, Play:5 Gen1 = 5). Both the owner's
  S1 (`57.23-74170-1-5.upd`, 2.2 MB) and S2 (`86.10-80260-1-12.upd`,
  7.5 MB) images were downloaded and parsed.
- `.upd` container (fully decoded): a sequence of records
  `[magic=0x35167F49 LE][type u32][total_len u32][reserved u32][payload]`.
  Record type 1 = header (major/minor/build u32s, e.g. 57/23/74170);
  type 21 = plaintext patch-train list (`2021_11_PATCHES:` …);
  type 22 = certificate bundle (plaintext CA roots); types 3/4/6 = payloads;
  type 23 = a **plain tar overlay** of the rootfs; type 17 = trailer.
- S2 86.10 payload (type 23) extracts to `bin/anacapad`: the 3 MB zone
  daemon, ELF 32-bit MSB MIPS32 (the Play:1 is big-endian MIPS), stripped.
  Strings reveal the service architecture (`oc/zone/common/*.cxx`):
  `device_description`, `upnpeventing_sender/source` (GENA), `mdns_*`
  (mDNS discovery runs alongside SSDP and reconciles with it),
  `websocketserver` (the S2 local WS API), `museclient_authhelper`
  ("Muse" = the Sonos cloud; hence `MuseHouseholdId` in ZGT events).
  The `SA_RINCON%u_X_#Svc%u-0-Token` format strings live in the daemon —
  the cdudn descriptor is validated/generated server-side, confirming why a
  wrong desc fails playback with 800. Config flags found:
  `useLegacySpotifySmapiPlayback`, `enableSpotifySMAPIVolumeNormalization`.
- S1 57.23 payloads (types 4/6) measure 8.00 bits/byte entropy with no known
  compression magic — encrypted with device-family keys, per published
  research (NCC Group BH-US 2024; blasty/sonos `sonostool`). Decryption is
  **not pursued**: unneeded for interoperability, and key extraction crosses
  into defeating protection measures.
- `ZoneGroupTopology` also exposes `BeginSoftwareUpdate(UpdateURL, Flags,
  ExtraOptions)` — the install path. **Never called by this project**:
  flashing is human-led per `docs/SCOPE.md`.

## 10. SMAPI (the speaker↔Spotify bridge) — research notes

Sonos renders music services through SMAPI, a SOAP API Sonos operates per
provider (Spotify: `https://spotify-v5.ws.sonos.com/smapi`, from the service
descriptor). The speaker calls it directly; controllers may too (SoCo does).

- Auth: SOAP header `<credentials xmlns="http://www.sonos.com/Services/1.1">`
  with `deviceId` (the player's `RINCON_…` UUID), `deviceProvider` = `Sonos`,
  and for linked accounts a `loginToken` block carrying `householdId` plus the
  token/key pair minted during AppLink (`getDeviceLinkCode(householdId)` →
  `getAppLink(householdId)`).
- Calls: `getMetadata(id, index, count)` (browse tree), `search(category,
  term)`, `getMediaURI(item_id)` (fresh stream URL per play), all returning
  SMAPI-typed metadata whose `<desc>`/item-id conventions match §4.
- Status: not yet driven directly by FrankenSonos — render params are learned
  from favorites instead (works, simpler auth). Direct SMAPI drive is tracked
  in bead `frankensonos-re-smapi-rbz`; an independent audio path via
  `x-rincon-mp3radio:` (verified working) is bead `frankensonos-re-spotify-audio-w21`.

