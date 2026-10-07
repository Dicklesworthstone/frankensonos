//! Golden tests: the protocol parsers against real response bodies captured
//! from an S1 and an S2 household, scrubbed of site identifiers (see
//! `tests/fixtures/README.md`). Fetch paths run through a recording
//! in-memory [`Transport`] that replays those bodies.

use fsonos_proto::content::{self, BrowseFlag};
use fsonos_proto::description::parse_device_description;
use fsonos_proto::didl::{
    DidlKind, DidlObject, SpotifyRenderParams, parse_didl, spotify_track_uri,
    spotify_uri_from_renderer_uri,
};
use fsonos_proto::topology::{self, ZoneGroupState};
use fsonos_proto::{ProtoError, Transport, soap};
use fsonos_types::PlayerId;
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

const ZGS_S1: &str = include_str!("fixtures/zgs_s1.xml");
const ZGS_S2: &str = include_str!("fixtures/zgs_s2.xml");
const FAV_S1: &str = include_str!("fixtures/browse_favorites_s1.xml");
const FAV_S2: &str = include_str!("fixtures/browse_favorites_s2.xml");
const QUEUE_S1: &str = include_str!("fixtures/browse_queue_s1.xml");
const QUEUE_S2: &str = include_str!("fixtures/browse_queue_s2.xml");
const QUEUE_EMPTY: &str = include_str!("fixtures/browse_queue_empty.xml");
const FAULT_701: &str = include_str!("fixtures/soap_fault_701.xml");

fn pid(n: u8) -> PlayerId {
    PlayerId(format!("RINCON_000E58A000{n:02X}01400"))
}

fn host() -> IpAddr {
    "192.0.2.13".parse().unwrap()
}

fn zgs(body: &str) -> ZoneGroupState {
    let response = soap::parse_response(body, "GetZoneGroupState").unwrap();
    topology::parse_zone_group_state(response.require("ZoneGroupState").unwrap()).unwrap()
}

/// One recorded `soap_post`.
#[derive(Debug, Clone)]
struct Call {
    host: IpAddr,
    path: String,
    action: String,
    body: String,
}

/// Replays canned bodies chosen by `respond` and records every request.
struct Replay<F: Fn(&Call) -> String> {
    respond: F,
    calls: RefCell<Vec<Call>>,
}

impl<F: Fn(&Call) -> String> Replay<F> {
    fn new(respond: F) -> Self {
        Self {
            respond,
            calls: RefCell::new(Vec::new()),
        }
    }
}

impl<F: Fn(&Call) -> String> Transport for Replay<F> {
    fn soap_post(
        &self,
        host: IpAddr,
        control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let call = Call {
            host,
            path: control_path.to_string(),
            action: soap_action.to_string(),
            body: body.to_string(),
        };
        let reply = (self.respond)(&call);
        self.calls.borrow_mut().push(call);
        Ok(reply)
    }
}

/// The `StartingIndex` a recorded Browse asked for.
fn starting_index(call: &Call) -> u32 {
    let start = call.body.find("<StartingIndex>").unwrap() + "<StartingIndex>".len();
    let end = call.body[start..].find('<').unwrap();
    call.body[start..start + end].parse().unwrap()
}

#[test]
fn s1_topology_pairs_groups_and_bridges() {
    let state = zgs(ZGS_S1);
    assert_eq!(state.groups.len(), 4);
    assert_eq!(state.vanished.len(), 0);

    // Two zone bridges, each alone in its own group.
    let bridges: Vec<_> = state
        .groups
        .iter()
        .filter(|g| g.members.iter().all(|m| m.is_zone_bridge))
        .collect();
    assert_eq!(bridges.len(), 2);
    assert!(
        bridges
            .iter()
            .all(|g| g.members.len() == 1 && g.members[0].invisible)
    );

    // Two stereo pairs grouped together; the group id's prefix is NOT the
    // coordinator's uuid (it is a leftover from an earlier coordinator).
    let g = &state.groups[1];
    assert_eq!(g.coordinator, pid(2));
    assert_eq!(g.id, "RINCON_000E58A0000301400:4078894429");
    assert_eq!(g.members.len(), 4);
    let study_rf = &g.members[0];
    assert_eq!(study_rf.uuid, pid(4));
    assert_eq!(study_rf.zone_name, "Owner\u{2019}s Study");
    assert!(study_rf.invisible);
    assert_eq!(study_rf.sw_gen, Some(1));
    assert_eq!(study_rf.software_version.as_deref(), Some("57.23-74170"));
    assert_eq!(study_rf.ip(), Some("192.0.2.11".parse().unwrap()));
    assert_eq!(study_rf.channel_map.len(), 2);
    assert_eq!(study_rf.channel_map[0].player, pid(5));
    assert_eq!(study_rf.channel_map[0].channels, "LF,LF");
    assert!(!g.members[1].invisible);

    let all: Vec<_> = state.groups.iter().flat_map(|g| &g.members).collect();
    assert_eq!(all.len(), 9);
    assert!(all.iter().all(|m| m.sw_gen == Some(1) && m.ip().is_some()));
}

#[test]
fn s2_topology_degraded_pair_and_vanished_devices() {
    let state = zgs(ZGS_S2);
    assert_eq!(state.groups.len(), 4);

    // The Lounge pair's visible LF and its sub are offline: the invisible RF
    // is the only member left, and it coordinates the group.
    let lounge = &state.groups[0];
    assert_eq!(lounge.coordinator, pid(0x0A));
    assert_eq!(lounge.members.len(), 1);
    let rf = &lounge.members[0];
    assert!(rf.invisible);
    assert_eq!(rf.sw_gen, Some(2));
    let channels: Vec<_> = rf.channel_map.iter().map(|c| c.channels.as_str()).collect();
    assert_eq!(channels, ["LF,LF", "RF,RF", "SW,SW"]);

    let vanished: Vec<_> = state
        .vanished
        .iter()
        .map(|v| (v.uuid.clone(), v.model.as_deref(), v.zone_name.as_str()))
        .collect();
    assert_eq!(
        vanished,
        [
            (pid(0x0B), Some("S12"), "Lounge"),
            (pid(0x0C), Some("Sub"), "Lounge"),
            (pid(0x11), Some("S3"), "Dining Room"),
        ]
    );
    assert_eq!(
        state.vanished[0].last_known_ip,
        Some("192.0.2.24".parse().unwrap())
    );

    let kitchen = &state.groups[3];
    assert_eq!(kitchen.members.len(), 2);
    assert!(!kitchen.members[0].invisible && kitchen.members[1].invisible);
}

#[test]
fn topology_fetch_goes_through_transport() {
    let t = Replay::new(|_| ZGS_S2.to_string());
    let state = topology::get_zone_group_state(&t, host()).unwrap();
    assert_eq!(state.groups.len(), 4);
    let calls = t.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].host, host());
    assert_eq!(calls[0].path, "/ZoneGroupTopology/Control");
    assert_eq!(
        calls[0].action,
        "\"urn:schemas-upnp-org:service:ZoneGroupTopology:1#GetZoneGroupState\""
    );
    assert!(calls[0].body.contains("<u:GetZoneGroupState xmlns:u="));
}

fn find<'a>(objects: &'a [DidlObject], id: &str) -> &'a DidlObject {
    objects.iter().find(|o| o.id == id).unwrap()
}

#[test]
fn s1_favorites_carry_household_render_params() {
    let page = content::parse_browse_response(FAV_S1).unwrap();
    assert_eq!((page.number_returned, page.total_matches), (4, 4));
    assert_eq!(page.objects.len(), 4);
    assert!(page.objects.iter().all(|o| o.kind == DidlKind::Item
        && o.parent_id == "FV:2"
        && o.class == "object.itemobject.item.sonos-favorite"));
    let ordinals: Vec<_> = page.objects.iter().map(|o| o.ordinal).collect();
    assert_eq!(ordinals, [Some(0), Some(1), Some(2), Some(3)]);

    // A shortcut favorite carries `<res/>` and only metadata.
    let shortcut = find(&page.objects, "FV:2/3");
    assert_eq!(shortcut.favorite_type.as_deref(), Some("shortcut"));
    assert!(shortcut.res.is_none());
    let inner = shortcut.res_md_object().unwrap().unwrap();
    assert_eq!(inner.class, "object.container.person.musicArtist");
    assert_eq!(
        inner.desc.unwrap().value,
        "SA_RINCON52231_X_#Svc52231-0-Token"
    );

    // An album favorite: a cpcontainer URI with the album flags.
    let album = find(&page.objects, "FV:2/0");
    let res = album.res.as_ref().unwrap();
    assert_eq!(
        res.protocol_info.as_deref(),
        Some("x-rincon-cpcontainer:*:*:*")
    );
    assert_eq!(
        spotify_uri_from_renderer_uri(&res.uri).as_deref(),
        Some("spotify:album:0FixtureSpotify0000001")
    );
    assert!(res.uri.ends_with("?sid=12&flags=8300&sn=1"));

    // A track favorite: its resMD holds the cdudn descriptor and item-id
    // prefix this household renders Spotify tracks with, and the documented
    // URI builder reproduces the household's own renderer URI byte for byte.
    let track = find(&page.objects, "FV:2/1");
    assert_eq!(track.favorite_type.as_deref(), Some("instantPlay"));
    let res = track.res.as_ref().unwrap();
    assert_eq!(
        res.protocol_info.as_deref(),
        Some("sonos.com-spotify:*:audio/x-spotify:*")
    );
    let md = track.res_md_object().unwrap().unwrap();
    assert_eq!(md.class, "object.item.audioItem.musicTrack");
    assert_eq!(md.id, "10032020spotify%3atrack%3a0FixtureSpotify0000002");
    let desc = md.desc.unwrap();
    assert_eq!(desc.id, "cdudn");
    assert_eq!(
        desc.name_space,
        "urn:schemas-rinconnetworks-com:metadata-1-0/"
    );
    assert_eq!(desc.value, "SA_RINCON3079_X_#Svc3079-0-Token");
    let params = SpotifyRenderParams {
        sid: 12,
        flags: 8224,
        sn: 1,
        cdudn: desc.value,
        item_id_prefix: "10032020".into(),
    };
    let source = spotify_uri_from_renderer_uri(&res.uri).unwrap();
    assert_eq!(source, "spotify:track:0FixtureSpotify0000002");
    assert_eq!(spotify_track_uri(&source, &params), res.uri);
}

#[test]
fn s2_favorites_show_per_favorite_variation() {
    let page = content::parse_browse_response(FAV_S2).unwrap();
    assert_eq!(page.objects.len(), 11);
    assert_eq!(page.number_returned, 11);

    // Spotify track favorites in one household do not share one flags value
    // or one item-id prefix: learning render params must look at all of them.
    let mut flags = Vec::new();
    let mut prefixes = Vec::new();
    for o in &page.objects {
        let Some(res) = &o.res else { continue };
        if !res.uri.starts_with("x-sonos-spotify:") {
            continue;
        }
        flags.push(
            res.uri
                .split("flags=")
                .nth(1)
                .unwrap()
                .split('&')
                .next()
                .unwrap()
                .to_string(),
        );
        prefixes.push(o.res_md_object().unwrap().unwrap().id[..8].to_string());
    }
    flags.sort();
    flags.dedup();
    prefixes.sort();
    prefixes.dedup();
    assert_eq!(flags, ["8224", "8232"]);
    assert_eq!(prefixes, ["00032020", "10032020", "10032028"]);

    // Upper-case `%3A` escapes decode the same as lower-case ones.
    let upper = find(&page.objects, "FV:2/45");
    assert_eq!(
        spotify_uri_from_renderer_uri(&upper.res.as_ref().unwrap().uri).as_deref(),
        Some("spotify:track:0FixtureSpotify0000008")
    );

    // A container favorite saved without a query string.
    let bare = find(&page.objects, "FV:2/3");
    assert!(!bare.res.as_ref().unwrap().uri.contains('?'));

    // Non-Spotify favorites parse but are not Spotify URIs.
    let radio = find(&page.objects, "FV:2/43");
    assert!(
        radio
            .res
            .as_ref()
            .unwrap()
            .uri
            .starts_with("x-rincon-mp3radio://")
    );
    assert_eq!(
        spotify_uri_from_renderer_uri(&radio.res.as_ref().unwrap().uri),
        None
    );
    let other = find(&page.objects, "FV:2/38")
        .res_md_object()
        .unwrap()
        .unwrap();
    assert_eq!(
        other.desc.unwrap().value,
        "SA_RINCON40967_X_#Svc40967-0-Token"
    );

    let library = find(&page.objects, "FV:2/33");
    assert!(library.res.is_none());
    assert_eq!(
        library.res_md_object().unwrap().unwrap().id,
        "10082064yourmusic_root"
    );
}

#[test]
fn queues_become_tracks() {
    let s1 = content::parse_browse_response(QUEUE_S1).unwrap();
    assert_eq!(
        (s1.number_returned, s1.total_matches, s1.update_id),
        (3, 10, 3)
    );
    let ids: Vec<_> = s1.objects.iter().map(|o| o.id.as_str()).collect();
    assert_eq!(ids, ["Q:0/1", "Q:0/2", "Q:0/3"]);
    let track = s1.objects[0].to_track().unwrap();
    assert_eq!(track.duration_secs, Some(239));
    assert!(
        track
            .source_uri
            .starts_with("spotify:track:0FixtureSpotify")
    );
    assert!(
        track
            .uri
            .unwrap()
            .starts_with("x-sonos-spotify:spotify%3atrack%3a")
    );
    assert!(track.artist.unwrap().starts_with("Artist "));
    assert!(track.album.unwrap().starts_with("Album "));

    let s2 = content::parse_browse_response(QUEUE_S2).unwrap();
    assert_eq!((s2.number_returned, s2.total_matches), (3, 8));
    assert!(
        s2.objects
            .iter()
            .all(|o| o.res.as_ref().unwrap().uri.contains("flags=8232"))
    );

    let empty = content::parse_browse_response(QUEUE_EMPTY).unwrap();
    assert_eq!((empty.objects.len(), empty.total_matches), (0, 0));
}

#[test]
fn browse_through_transport_and_paging() {
    // A single page that is the whole container: one request.
    let t = Replay::new(|_| FAV_S1.to_string());
    let all = content::browse_all(&t, host(), content::FAVORITES).unwrap();
    assert_eq!(all.len(), 4);
    let calls = t.calls.borrow();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].path, "/MediaServer/ContentDirectory/Control");
    assert_eq!(
        calls[0].action,
        "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\""
    );
    assert!(calls[0].body.contains("<ObjectID>FV:2</ObjectID>"));
    assert!(
        calls[0]
            .body
            .contains("<RequestedCount>100</RequestedCount>")
    );
    drop(calls);

    // 3 of 10 returned, then a page with no objects that still claims 10
    // matches (the queue shrank mid-walk): browse_all asks for the next page
    // and then stops instead of looping forever.
    let stale_empty = QUEUE_EMPTY.replace(
        "<TotalMatches>0</TotalMatches>",
        "<TotalMatches>10</TotalMatches>",
    );
    let requests = Cell::new(0);
    let t = Replay::new(|call| {
        requests.set(requests.get() + 1);
        assert!(requests.get() <= 2, "browse_all kept paging");
        if starting_index(call) == 0 {
            QUEUE_S1.to_string()
        } else {
            stale_empty.clone()
        }
    });
    let all = content::browse_all(&t, host(), content::QUEUE).unwrap();
    assert_eq!(all.len(), 3);
    let starts: Vec<_> = t.calls.borrow().iter().map(starting_index).collect();
    assert_eq!(starts, [0, 3]);

    // Pages are concatenated in order until TotalMatches is reached.
    let t = Replay::new(|call| match starting_index(call) {
        0 => QUEUE_S1.replace(
            "<TotalMatches>10</TotalMatches>",
            "<TotalMatches>6</TotalMatches>",
        ),
        3 => QUEUE_S2.replace(
            "<TotalMatches>8</TotalMatches>",
            "<TotalMatches>6</TotalMatches>",
        ),
        n => panic!("browse_all read past TotalMatches (StartingIndex {n})"),
    });
    let all = content::browse_all(&t, host(), content::QUEUE).unwrap();
    assert_eq!(all.len(), 6);
    assert!(
        all[..3]
            .iter()
            .all(|o| o.res.as_ref().unwrap().uri.contains("flags=8224"))
    );
    assert!(
        all[3..]
            .iter()
            .all(|o| o.res.as_ref().unwrap().uri.contains("flags=8232"))
    );

    let t = Replay::new(|_| QUEUE_S2.to_string());
    let page =
        content::browse(&t, host(), content::QUEUE, BrowseFlag::DirectChildren, 0, 3).unwrap();
    assert_eq!(page.objects.len(), 3);
    assert!(
        t.calls.borrow()[0]
            .body
            .contains("<RequestedCount>3</RequestedCount>")
    );
}

#[test]
fn upnp_fault_surfaces_its_error_code() {
    let expect_701 = |r: Result<_, ProtoError>| match r {
        Err(ProtoError::SoapFault { code, reason }) => {
            assert_eq!(code, 701);
            assert_eq!(reason, "UPnPError");
        }
        other => panic!("expected UPnP fault 701, got {other:?}"),
    };
    expect_701(soap::parse_response(FAULT_701, "Browse").map(|_| ()));
    let t = Replay::new(|_| FAULT_701.to_string());
    expect_701(content::browse(&t, host(), "NOPE:0", BrowseFlag::DirectChildren, 0, 5).map(|_| ()));
}

#[test]
fn device_descriptions_identify_model_room_and_generation() {
    let play5 =
        parse_device_description(include_str!("fixtures/device_description_s1_play5.xml")).unwrap();
    assert_eq!(play5.udn, pid(4));
    assert_eq!(play5.room_name, "Owner\u{2019}s Study");
    assert_eq!(play5.model_name, "Sonos Play:5");
    assert_eq!(play5.model_number, "S5");
    assert_eq!(play5.display_name.as_deref(), Some("Play:5"));
    assert_eq!(play5.display_version.as_deref(), Some("11.16.1"));
    assert_eq!(play5.sw_gen, Some(1));
    assert!(play5.is_renderer());
    assert!(play5.has_service(soap::CONTENT_DIRECTORY.service_type));
    assert!(play5.has_service(soap::ZONE_GROUP_TOPOLOGY.service_type));
    let avt = play5
        .services
        .iter()
        .find(|s| s.service_type == soap::AV_TRANSPORT.service_type)
        .unwrap();
    assert_eq!(avt.control_url, soap::AV_TRANSPORT.control_path);
    assert_eq!(avt.event_sub_url, soap::AV_TRANSPORT.event_path);

    let bridge =
        parse_device_description(include_str!("fixtures/device_description_s1_bridge.xml"))
            .unwrap();
    assert_eq!(
        (bridge.model_number.as_str(), bridge.sw_gen),
        ("ZB100", Some(1))
    );
    assert!(!bridge.is_renderer());

    // A Play:1 reports model number "S1" while running S2 software.
    let play1 =
        parse_device_description(include_str!("fixtures/device_description_s2_play1.xml")).unwrap();
    assert_eq!((play1.model_number.as_str(), play1.sw_gen), ("S1", Some(2)));
    assert_eq!(play1.udn, pid(0x0A));
    assert!(play1.is_renderer());

    let one =
        parse_device_description(include_str!("fixtures/device_description_s2_one.xml")).unwrap();
    assert_eq!((one.model_number.as_str(), one.sw_gen), ("S13", Some(2)));
    assert_eq!(one.room_name, "Parlor");
}

// ── GENA NOTIFY fixtures (captured live from both households 2026-10-07) ──

const GENA_AVT_S1: &str = include_str!("fixtures/gena_notify_avt_initial_s1.xml");
const GENA_AVT_PAUSE_S1: &str = include_str!("fixtures/gena_notify_avt_pause_s1.xml");
const GENA_RCS_VOL_S1: &str = include_str!("fixtures/gena_notify_rcs_volume_s1.xml");
const GENA_RCS_INIT_S1: &str = include_str!("fixtures/gena_notify_rcs_initial_s1.xml");
const GENA_ZGT_S1: &str = include_str!("fixtures/gena_notify_zgt_s1.xml");
const GENA_ZGT_S2: &str = include_str!("fixtures/gena_notify_zgt_s2.xml");
const GENA_AVT_S2: &str = include_str!("fixtures/gena_notify_avt_initial_s2.xml");
const MSERVICES_S1: &str = include_str!("fixtures/musicservices_list_s1.xml");

/// One XML decode pass: named entities first, `&amp;` last.
fn xml_unescape(s: &str) -> String {
    s.replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&amp;", "&")
}

/// Text of the first `<Name>…</Name>` element (element text, not `val=`).
fn elem_text<'a>(doc: &'a str, name: &str) -> Option<&'a str> {
    let open = format!("<{name}>");
    let start = doc.find(&open)? + open.len();
    let end = doc[start..].find(&format!("</{name}>"))? + start;
    Some(&doc[start..end])
}

/// The `val="…"` attribute of the first `<Name …/>` in an AVT/RCS event.
fn event_val<'a>(event: &'a str, name: &str) -> Option<&'a str> {
    let pat = format!("{name} val=\"");
    let start = event.find(&pat)? + pat.len();
    let end = event[start..].find('"')? + start;
    Some(&event[start..end])
}

/// propertyset → LastChange → inner `<Event>` document (one decode pass).
fn last_change_event(notify: &str) -> String {
    let lc = elem_text(notify, "LastChange").expect("NOTIFY carries LastChange");
    xml_unescape(lc)
}

#[test]
fn gena_avt_initial_s1_carries_full_state_and_track_metadata() {
    let event = last_change_event(GENA_AVT_S1);
    assert!(event.contains("urn:schemas-upnp-org:metadata-1-0/AVT/"));
    assert_eq!(event_val(&event, "TransportState"), Some("STOPPED"));
    assert_eq!(event_val(&event, "NumberOfTracks"), Some("1"));
    let uri = event_val(&event, "CurrentTrackURI").unwrap();
    assert_eq!(
        spotify_uri_from_renderer_uri(uri).as_deref(),
        Some("spotify:track:0FixtureSpotifyTrack1")
    );
    assert!(
        xml_unescape(uri).ends_with("?sid=12&flags=8224&sn=1"),
        "{uri}"
    );

    // The player replaced our minimal DIDL with SMAPI-fetched metadata: a thin
    // shell with id="-1", real title/album, and NO cdudn desc — event-stream
    // metadata is display-grade, not render-grade.
    let md_escaped = event_val(&event, "CurrentTrackMetaData").unwrap();
    let objects = parse_didl(&xml_unescape(md_escaped)).unwrap();
    let track = objects.iter().find(|o| o.kind == DidlKind::Item).unwrap();
    assert_eq!(track.id, "-1");
    assert_eq!(track.class, "object.item.audioItem.musicTrack");
    assert_eq!(track.title, "Title 1");
    assert!(track.desc.is_none());
}

#[test]
fn gena_avt_pause_captures_transitioning_state() {
    let event = last_change_event(GENA_AVT_PAUSE_S1);
    // Mid-pause snapshot: Sonos reports TRANSITIONING, not PAUSED_PLAYBACK.
    assert_eq!(event_val(&event, "TransportState"), Some("TRANSITIONING"));
    let uri = event_val(&event, "CurrentTrackURI").unwrap();
    assert!(uri.starts_with("x-sonos-spotify:"), "{uri}");
}

#[test]
fn gena_rcs_volume_delta_uses_channel_attributes() {
    let event = last_change_event(GENA_RCS_VOL_S1);
    assert!(event.contains("urn:schemas-upnp-org:metadata-1-0/RCS/"));
    assert!(event.contains("<Volume channel=\"Master\" val=\"19\"/>"));
    assert!(event.contains("<Volume channel=\"LF\" val=\"100\"/>"));
}

#[test]
fn gena_zgt_notify_carries_full_zone_group_state() {
    // ZoneGroupState arrives as element text (not LastChange) in the NOTIFY.
    let zgs_escaped = elem_text(GENA_ZGT_S1, "ZoneGroupState").expect("ZoneGroupState property");
    let state = topology::parse_zone_group_state(&xml_unescape(zgs_escaped)).unwrap();
    assert_eq!(state.groups.len(), 4);
    let big = state.groups.iter().max_by_key(|g| g.members.len()).unwrap();
    assert_eq!(big.members.len(), 4);
    assert!(
        state
            .groups
            .iter()
            .flat_map(|g| g.members.iter())
            .all(|m| m.uuid.0.starts_with("RINCON_000E58A0"))
    );
    assert_eq!(
        state.groups.iter().map(|g| g.members.len()).sum::<usize>(),
        9
    );
}

#[test]
fn gena_avt_initial_s2_idle_player_shape() {
    let event = last_change_event(GENA_AVT_S2);
    assert!(event.contains("urn:schemas-upnp-org:metadata-1-0/AVT/"));
    assert!(event.contains("<InstanceID val=\"0\">"));
    // Idle S2 player: stopped, empty queue, and the empty-attribute edge case.
    assert_eq!(event_val(&event, "TransportState"), Some("STOPPED"));
    assert_eq!(event_val(&event, "NumberOfTracks"), Some("0"));
    assert_eq!(event_val(&event, "AVTransportURI"), Some(""));
}

#[test]
fn gena_rcs_initial_s1_carries_full_render_state() {
    let event = last_change_event(GENA_RCS_INIT_S1);
    assert!(event.contains("urn:schemas-upnp-org:metadata-1-0/RCS/"));
    assert!(event.contains("<Volume channel=\"Master\" val=\"18\"/>"));
    assert!(event.contains("<Mute channel=\"Master\" val=\"0\"/>"));
    assert_eq!(event_val(&event, "Bass"), Some("0"));
    assert_eq!(event_val(&event, "Loudness"), None); // value lives in channel attr
    assert!(event.contains("<Loudness channel=\"Master\" val=\"1\"/>"));
}

#[test]
fn gena_zgt_s2_carries_update_oracle_and_vanished_devices() {
    let zgs_escaped = elem_text(GENA_ZGT_S2, "ZoneGroupState").expect("ZoneGroupState property");
    let state = topology::parse_zone_group_state(&xml_unescape(zgs_escaped)).unwrap();
    assert!(!state.groups.is_empty());
    assert_eq!(state.vanished.len(), 3, "three offline players remembered");
    // The AvailableSoftwareUpdate property is the firmware-URL oracle.
    let update = elem_text(GENA_ZGT_S2, "AvailableSoftwareUpdate").unwrap();
    let update = xml_unescape(update);
    assert!(update.contains("UpdateURL=\"http://update-firmware.sonos.com/"));
    assert!(update.contains("ManifestURL=\"http://update.sonos.com/"));
    assert!(update.contains("Swgen=\"2\""));
}

#[test]
fn musicservices_list_describes_spotify_smapi() {
    // Fixture integrity anchors: the S1 household's Spotify descriptor.
    let inner = soap::parse_response(MSERVICES_S1, "ListAvailableServices")
        .unwrap()
        .require("AvailableServiceDescriptorList")
        .unwrap()
        .to_string();
    let list = xml_unescape(&xml_unescape(&inner));
    assert!(
        list.contains("Id=\"12\" Name=\"Spotify\""),
        "Spotify service id 12"
    );
    assert!(list.contains("https://spotify-v5.ws.sonos.com/smapi"));
    assert!(list.contains("Auth=\"AppLink\""));
}

#[test]
fn fixtures_are_scrubbed() {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures");
    let mut checked = 0;
    for entry in std::fs::read_dir(&dir).unwrap() {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("xml") {
            continue;
        }
        let text = std::fs::read_to_string(&path).unwrap();
        let name = path.display();
        for private in ["192.168.", "://10.", "://172.", "Sonos_"] {
            assert!(!text.contains(private), "{name} contains {private:?}");
        }
        for (at, _) in text.match_indices("RINCON_") {
            let id = &text[at + 7..];
            let is_service_descriptor =
                id.starts_with(|c: char| c.is_ascii_digit()) && text[..at].ends_with("SA_");
            assert!(
                is_service_descriptor || id.starts_with("000E58A0"),
                "{name} has a non-synthetic player id near {}",
                &text[at..(at + 24).min(text.len())]
            );
        }
        checked += 1;
    }
    // Lower bound (not an exact count): the loop already scrubs EVERY fixture,
    // so a fixed `== N` only broke the gate each time a fixture was added
    // without buying any extra safety. This still catches an empty/missing
    // fixtures dir.
    assert!(
        checked >= 18,
        "expected at least 18 scrubbed fixtures, found {checked}"
    );
}
