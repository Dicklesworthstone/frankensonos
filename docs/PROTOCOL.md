# FrankenSonos protocol notes — the Sonos local wire protocol, verified live

Everything here was verified against real S1 (firmware 57.23) and S2 (86.10,
97.1) players on the owner's LAN on 2026-10-06/07, or is marked as community
knowledge. No site-specific identifiers appear in this file: `<IP>` stands for
a player address, `RINCON_<MAC>01400` for a player UUID, `Sonos_<ID>` for a
household ID.

## 1. Discovery and addressing

- SSDP M-SEARCH to `239.255.255.250:1900`, ST `urn:schemas-upnp-org:device:ZonePlayer:1`.
  Every response carries `X-RINCON-HOUSEHOLD: Sonos_<ID>` — filter client-side
  to pick a household. On this LAN a single scan returned only one household
  per attempt (observed 2026-10-06); scan repeatedly, or fall back to direct
  seeds: GET `http://<IP>:1400/xml/device_description.xml` on candidate IPs.
- mDNS (`_sonos._tcp.local`) is a second, independent discovery channel the
  players run themselves (anacapa reconciles SSDP vs mDNS sightings).
  Verified live 2026-10-07: S2 players advertise
  `RINCON_<UUID>01400@<Room>` with TXT `uuid=`, `hhid=Sonos_<HOUSEHOLD>`,
  `mhhid=` (same with session suffix), `bootseq=` (reboot detection),
  `location=` (description URL), `sslport=1443`, `wss=/websocket/api`;
  S1 players advertise `Sonos-<MAC>` with a minimal TXT
  (`info=`, `vers=1`, `protovers=`) — no household field, get it from the
  description instead. Bridges do not advertise.
- All control is HTTP on TCP **1400**. Device description:
  `GET /xml/device_description.xml`. Status pages: `GET /status`,
  `/status/zp`, `/status/VERSION` (build string), `/status/ifconfig`,
  `/status/proc/ath_rincon/status` (SonosNet radio diagnostics).
  Hidden pages verified live (S1): `/advconfig.htm` (FirstZP/PriorityBridge
  toggles; POST needs form data) and `/tools.htm` (ping/traceroute/nslookup
  forms with CSRF tokens — note: the HTML forms are CSRF-protected while the
  SOAP control surface is not).
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

Exact URL map (from a live S1 Play:5 description; S2 identical where the
service exists): top-level `/AlarmClock`, `/MusicServices`, `/AudioIn`,
`/DeviceProperties`, `/SystemProperties`, `/ZoneGroupTopology`,
`/GroupManagement`, `/QPlay`, each with `/Control` + `/Event`;
`/MediaServer/ContentDirectory` and `/MediaServer/ConnectionManager`;
`/MediaRenderer/RenderingControl`, `/MediaRenderer/AVTransport`,
`/MediaRenderer/GroupRenderingControl`, `/MediaRenderer/VirtualLineIn`,
`/MediaRenderer/ConnectionManager` (ConnectionManager appears twice — once per
embedded device). **Namespaces differ**: Queue is
`urn:schemas-sonos-com:service:Queue:1` at `/MediaRenderer/Queue/Control`,
QPlay is `urn:schemas-tencent-com:service:QPlay:1` at `/QPlay/Control` —
everything else is `urn:schemas-upnp-org:service:<Name>:1`. Always read the
URLs from the description; do not guess.

(The model labels in the table header — `S1`, `S12`, `S13` — are Sonos
modelNumber strings, unrelated to the S1/S2 *generation* split.)

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
S1: x-sonos-spotify:spotify%3atrack%3a<TRACK_ID>?sid=<SID>&flags=8224&sn=<SN>
S2: x-sonos-spotify:spotify%3Atrack%3A<TRACK_ID>?sid=<SID>&flags=8232&sn=<SN>
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
`resMD` of the favorite). Playlists use class `object.container.playlistContainer`
and — **the prefix varies** (`1006206c`, `10062a6c` both observed in one
favorites list) — learn it from a playlist favorite rather than hardcoding.
Playlist URIs in favorites use the **legacy** form
`spotify:user:<USER>:playlist:<ID>`; the modern `spotify:playlist:<ID>` form
should be accepted too (both are accepted by the bridge).

Container playback (verified live on both generations, 2026-10-07):
`AddURIToQueue` with the cpcontainer URI + container DIDL makes the **speaker
expand the container into its full track list at enqueue time** — the
response carries `FirstTrackNumberEnqueued` (position of track 1) and
`NumTracksAdded` (8 for the test album on both households). Then point the
transport at `x-rincon-queue:<COORD_UUID>#0`, `Seek` `TRACK_NR` to the
returned position, `Play`. Whole-album removal is per-position
`RemoveTrackFromQueue` in descending order (both queues restored exactly).
For whole-container removal, `RemoveTrackRangeFromQueue(UpdateID=0,
StartingIndex=N, NumberOfTracks=M)` deletes a range in ONE call — verified on
both generations (a 491-track playlist removed atomically; queues restored).

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

## 6. Topology and grouping

`ZoneGroupTopology#GetZoneGroupState` returns escaped XML:
`<ZoneGroups><ZoneGroup Coordinator="RINCON_…" ID="RINCON_…:N">` containing
`<ZoneGroupMember UUID=… ZoneName=… Location="http://<IP>:1400/xml/device_description.xml" …/>`.
Solo players and Bridges appear as single-member groups; `:0` group IDs are
invisible/satellite entries. Room names may contain non-ASCII (curly
apostrophes) — normalize quotes when matching by name.

Grouping verbs (verified live on S2 97.1, 2026-10-07, with full restore):

- **Join**: `SetAVTransportURI(InstanceID=0, CurrentURI="x-rincon:<COORD_UUID>",
  CurrentURIMetaData="")` on the *member* — it slaves to that coordinator.
  (GroupManagement `AddMember(MemberID, BootSeq)` is the newer structured
  path; it also returns volume/transport settings for the join.)
- **Ungroup**: `SetAVTransportURI(InstanceID=0,
  CurrentURI="x-rincon:<OWN_UUID>", CurrentURIMetaData="")` on the *member*
  — "join yourself". **Returns UPnP 402 but ungroups immediately anyway**
  (verified on BOTH generations 2026-10-07: solo within ~1 s of the call,
  timed; the 402 is a red herring — the player rejects then executes).
  Dead ends, also verified: `BecomeCoordinatorOfStandaloneGroup` returns
  1023 on both generations (SoCo's `unjoin()` is broken on current firmware);
  `GroupManagement#RemoveMember` returns success but is a **no-op** on both
  generations (S1 confirmed still grouped after 95 s; S2's apparent effect
  was actually the subsequent self-join). A controller must treat the 402
  on the self-join as success and verify via a topology read-back.
- ZGT events fire on grouping changes; each NOTIFY carries the new full
  ZoneGroupState (fixtures: solo-state and grouped-state S2 captures).

## 7. GENA events

- `SUBSCRIBE http://<IP>:1400/<EventURL>` with headers
  `CALLBACK: <http://<CONTROLLER_IP>:<PORT>/cb>`, `NT: upnp:event`,
  `TIMEOUT: Second-300`. Response carries `SID: uuid:RINCON_<MAC>01400_subNNNNN`.
- Speaker then POSTs `NOTIFY /cb` per change; body is
  `<e:propertyset><e:property><LastChange>ESCAPED-XML</LastChange>…`.
- Initial NOTIFY (SEQ 0) carries full state; later NOTIFYs carry changed
  variables only. Renew before the timeout (community reports Sonos grants
  86400 s regardless of the request); `UNSUBSCRIBE` with the `SID`.
- AVTransport LastChange root: `<Event xmlns="urn:schemas-upnp-org:metadata-1-0/AVT/">`
  with `<InstanceID val="0">` children: `TransportState`, `CurrentTrackURI`,
  `CurrentTrackDuration`, `CurrentTrackMetaData` (DIDL, triple-escaped at the
  wire level), `AVTransportURI`, `CurrentTransportActions`, …
- RenderingControl LastChange root: `…/RCS/`, children like
  `<Volume channel="Master" val="19"/>`, plus `LF`/`RF` fixed at 100.
- ZoneGroupTopology NOTIFYs carry the full current `ZoneGroupState` (not a
  diff) — a single subscription anywhere in a household tracks all grouping.
- Queue LastChange root: `<Event xmlns="urn:schemas-sonos-com:metadata-1-0/Queue/">`
  with a `<QueueID val="0">` container (NOT `InstanceID`) holding an
  incrementing `<UpdateID val="N"/>` per mutation (verified live on
  `AddURIToQueue`/`RemoveTrackFromQueue`, S1 57.23).
- ContentDirectory events are NOT LastChange-wrapped: plain properties —
  `SystemUpdateID`, `ContainerUpdateIDs` (`R:,2`), `FavoritesUpdateID`
  (`RINCON_…,N`), `ShareIndexInProgress` (verified live, S1).
- GroupRenderingControl events are likewise plain properties: `GroupVolume`,
  `GroupMute`, `GroupVolumeChangeable` — verified live on a `SetGroupVolume`
  round-trip (S1). So: LastChange services are AVTransport, RenderingControl
  and Queue; ZoneGroupTopology, ContentDirectory and GroupRenderingControl
  send plain properties.

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
  `update_manifest` with `system_version`, a `base_url` ending in a
  `^<version>` filename template, `swgen`, and per-model `<image>` entries.
  The image list is mostly *exceptions*: controller apps (model 3 = Windows
  `.exe`, 4 = macOS `.dmg`, 10 = iOS store link, 11 = Android market link) and
  milestone steps with `fromver_min/max` + `milestone_index` (a player below
  the milestone fetches it first — e.g. `<34.7` → `34.16-37101` — then
  continues to current). Speaker images are NOT listed per model: players
  expand `base_url`'s template to `<ver>-1-<model>.upd` themselves. Numeric
  model ids correspond to the description's modelNumber digits (S5 → 5,
  S12 → 12, S13 → 13); `model_list` entries use `model.submodel` notation.
- Images: `http://update-firmware.sonos.com/firmware/Prod/<ver>-v<mkt>-<token>-<GA|RC|LR>-<n>/<ver>-1-<model>.upd`,
  no auth, CloudFront-fronted, old builds not garbage-collected. Both the
  owner's S1 (`57.23-74170-1-5.upd`, 2.2 MB, Play:5 Gen1) and S2
  (`86.10-80260-1-12.upd`, 7.5 MB, Play:1) images were downloaded and parsed.
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
  research (NCC Group BH-US 2024; blasty/sonos `sonostool`). The owner has
  since directed (2026-10-08) that defeating the protection on his own
  hardware is in-bounds for the replacement-software goal; see below.
- `ZoneGroupTopology` also exposes `BeginSoftwareUpdate(UpdateURL, Flags,
  ExtraOptions)` — the install path. **Never called by this project**:
  flashing is human-led per `docs/SCOPE.md`.

Protection analysis (S1 line, 2026-10-08 — owner-directed):

- The 34.16-era milestone image is **fully unencrypted**: gzipped MIPS kernel
  (type 6 record) + a big-endian CramFS rootfs (type 4) + shell-script
  records. The rootfs was extracted and read (62 files): `opt/bin/anacapad`
  (the 1.4 MB daemon — Anacapa is the combined HTTP/UPnP server, per
  `opt/conf/anacapa.conf`: Port 1400, SSLPort 1443), `bin/upgrade` (the
  on-device updater), busybox, uClibc libs, mbedTLS.
- The updater (`bin/upgrade`, `UpgradeController`) verifies a **Sonos
  signature section** against an on-device key ("Sonos signature verification
  failed", "No valid signature", `mbedtls_pk_parse_key`); encryption of
  *content* arrived later (57.23 payloads are encrypted; "encrypted signature
  section not supported" appears in the 34.16 build). So S1 protection =
  on-device signature check (+ content encryption in later builds), with keys
  compiled into the install environment — not per-device OTP like newer S2
  hardware (NCC/blasty: Amlogic eFUSE).
- The updater carries a **vendor debug path** (`update_debug_version`,
  `is_debug_version`, `allow_policy_bypass`, "No-op mode - download/reboot
  will be skipped") — the classic dev backdoor pattern. Its trigger is not in
  the readable settings store (SystemProperties GetString on the obvious
  names all 800); finding it wants `r2` on `bin/upgrade`.
- Feasibility map for owner-software replacement: S1 is the tractable target
  (readable images, decoded container, debug path to chase); S2-One class
  needs the published hardware-exploit route (USB bootrom, per-device keys) —
  disproportionate for reliability goals. Flashing anything remains
  human-led per `docs/SCOPE.md`, one explicit approval per operation.

More (2026-10-08): the encryption boundary is between 34.16 (readable) and
57.19 (encrypted) — 57.19 and 73.0 Play:1 images show high-entropy payloads
with no structure; both S1-gen platforms' milestone-era rootfs images (Play:5
Gen1 model 5, Play:1 model 12) are extracted and readable. The S1 controller
app (x86_64, from the still-served 57.23 DMG) shows the update *orchestration*
side: `BeginSoftwareUpdate` takes the update URL as an argument (the
controller picks the channel — `update.sonos.com/firmware/latest/` vs beta
via `x-sonos-scuri://settings/advanced/beta`), and carries a
`SCMockUpdateDebugPage` class in its wizard flow. Both S1-gen updaters share
the debug-path strings, so one trigger would open both lines.

## 10. SMAPI (the speaker↔Spotify bridge) — verified against the live endpoint

Sonos renders music services through SMAPI, a SOAP API Sonos operates per
provider (Spotify: `https://spotify-v5.ws.sonos.com/smapi`, from the service
descriptor). The speaker calls it directly; controllers may too — the
endpoint answers ordinary HTTPS from anywhere (verified from the owner's
Mac, 2026-10-07).

### Envelope and auth

```xml
<s:Envelope xmlns:s="http://schemas.xmlsoap.org/soap/envelope/">
 <s:Header>
  <credentials xmlns="http://www.sonos.com/Services/1.1">
   <deviceId>RINCON_<PLAYER_MAC>01400</deviceId>
   <deviceProvider>Sonos</deviceProvider>
   <context/>
   <!-- once linked: -->
   <loginToken>
    <token>AUTH_TOKEN</token>
    <key>PRIVATE_KEY</key>
    <householdId>Sonos_<HOUSEHOLD></householdId>
   </loginToken>
  </credentials>
 </s:Header>
 <s:Body><getMetadata xmlns="http://www.sonos.com/Services/1.1">…</getMetadata></s:Body>
</s:Envelope>
```

`SOAPACTION: "http://www.sonos.com/Services/1.1#<method>"`. Verified behavior:

- **Anonymous** (no `loginToken`): `getDeviceLinkCode(householdId)` succeeds —
  returns `regUrl` (`https://spotify-v5.ws.sonos.com/deviceLink/home?linkCode=…`)
  and a `linkCode`. Everything else fails with
  `Client.AuthTokenExpired` ("authTokenExpired").
- **Linked**: `loginToken` carries the `token`/`key` pair minted by the
  AppLink ceremony: `getDeviceLinkCode` → user authorizes at the regUrl
  (their own Spotify login) → `getAppLink(householdId)` returns
  `authToken` + `privateKey`. The pair is then reusable until revoked.
- The speaker never exposes its own existing pair on current firmware:
  `GET /status/accounts` returns an empty `ZPSupportInfo` on both S1 57.23
  and S2 97.1 (verified). Our own pair must be minted by our own AppLink.

### Calls (from SoCo's reference client, `soco/music_services/`)

| Call | Args | Returns |
|---|---|---|
| `getMetadata` | `id` (`root` or a SMAPI id), `index`, `count`, `recursive` | browse tree: tracks/albums/playlists with SMAPI ids + `desc` metadata |
| `search` | `id` (category: artists/albums/tracks/playlists…), `term`, `index`, `count` | same shape as getMetadata |
| `getMediaURI` | `id` (track) | fresh, short-lived stream URL (what the speaker fetches to render) |
| `getExtendedMetadata` | `id` | action/related-metadata for an item |
| `getDeviceLinkCode` / `getAppLink` | `householdId` | the AppLink ceremony above |

### Feasibility verdict

Direct SMAPI drive from fsonos is **feasible**: every step is plain SOAP over
HTTPS, the pre-auth step is verified working anonymously, and minting our own
token pair needs one interactive Spotify login by the owner. (Not yet
exercised end-to-end: whether the ceremony adds a second coexisting account
link or re-uses the household's existing one is untested — either way it is
reversible from the Sonos app.)
It is **not load-bearing**: the DJ already works via favorites-learned render
params + the speaker's own SMAPI session, and library reads use the official
Spotify Web API. Implement SMAPI-direct only as a hedge (Web API scope
erosion, or SMAPI-native search/browse without the Sonos app).

## 11. Spotify audio without SMAPI — the radio-bridge evaluation

Question (owner directive 2026-10-07): can the DJ stop depending on Spotify's
Sonos-facing integration entirely?

Substrate (verified live): `x-rincon-mp3radio://<host>/<path>` via
`SetAVTransportURI` plays arbitrary HTTP audio on both generations — a public
MP3 stream played on the S1 office group on first attempt.

Design: a librespot-class client (owner's own Premium account, e.g.
librespot/spotifyd) pulls the Spotify audio stream; fsonos serves it as a
local HTTP stream; any speaker(s) play it as a "radio station". Metadata and
queue logic stay with the DJ engine; the speakers become dumb renderers.

| | SMAPI path (primary) | Radio-bridge path (hedge) |
|---|---|---|
| Works today | ✅ verified end-to-end | substrate verified; source missing |
| Extra moving parts | none | librespot daemon + stream relay |
| Gapless / seek | speaker-managed | we'd manage it |
| Group sync | native (coordinator pulls once) | native (same URL on coordinator) |
| If Sonos kills S1 SMAPI | breaks | survives |
| Spotify dependency | Sonos SMAPI integration | owner's own Premium session (third-party client — same category as spotifyd/ncspot; ToS-gray but personal-use) |

**Decision: GO as an optional module, never the primary path.**
Prerequisites for a full proof-of-concept, both currently missing:

1. A streaming-capable credential for the owner's account — the Lane C OAuth
   cache (`user-library-read`) does not include the `streaming` scope; a
   second PKCE grant with `streaming` (or librespot's own login) is needed.
2. A librespot/spotifyd binary (not installed on the daemon host; Rust —
   buildable with the project toolchain when wanted).

Until then the substrate test stands as the PoC boundary: HTTP radio renders
natively on S1 and S2, so the bridge is ready when a source is plugged in.

## 12. Native Tailscale on the speakers — feasibility study (2026-10-07)

Question (owner): could the speakers themselves join the tailnet, instead of
the daemon fronting them?

Device classes from the firmware work (§9): Play:5 Gen1 (S1-era MIPS,
frozen firmware, encrypted payloads), Play:1 (MIPS32 big-endian, glibc,
~128 MB class RAM), One Gen1 (Amlogic A113x ARM, much more capable). Stock
firmware on all of them has **no owner code-execution channel**: no SSH, no
telnet, no debug console on :1400. The only vendor path for new code is a
firmware image through `BeginSoftwareUpdate`.

What native Tailscale would therefore require:

1. **A custom, signed firmware image.** Images are signature-checked
   (device-family keys in OTP/eFUSE). Building a valid image means Sonos's
   keys (not available) or exploiting a signature weakness (NCC Group
   documented one header malleability issue on one model — that is defeating
   the protection, fragile across releases, and out of this project's scope).
2. **Binary + resource fit.** Tailscale does ship `linux/mips` and
   `linux/arm64` builds, so the Play:1/One CPU families are covered in
   principle. The binding constraints are elsewhere: tailscaled's ~30-60 MB
   RSS against a ~128 MB device already running anacapa; `/dev/net/tun`
   availability on a 2.6.32-era kernel (userspace-networking mode avoids TUN
   but then inbound :1400 needs an on-box userspace TCP proxy — more moving
   parts on the weakest hardware in the system).
3. **Persistence across updates.** Sonos updates replace the whole image;
   any injected payload must be re-injected every update, forever. That
   recurring maintenance is exactly the reliability tax the owner is trying
   to escape.

**Verdict: native on-speaker Tailscale is technically imaginable only via
custom signed firmware — brick risk on hardware the owner relies on,
per-update re-fighting, and it would make the system *less* robust, not
more.** The daemon-fronted model (one supervised host fronts all speakers on
the tailnet; speakers stay stock) dominates on every axis: zero device risk,
works for the frozen S1 line forever, one point of maintenance. The
reliability budget belongs in the controller (see `docs/ROBUSTNESS.md`).
One firmware-flavored idea *is* worth keeping: the
`AvailableSoftwareUpdate` oracle (§7/§9) lets the daemon notice when Sonos
changes firmware under us, so an upstream change never silently breaks a
learned assumption.

