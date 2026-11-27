//! `control` sends each command to the right player: group-wide commands to
//! the coordinator's address, room volume to the room's own player, joins to
//! the member. Households come from the scrubbed S1/S2 fixtures.

use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::didl::SpotifyContainer;
use fsonos_proto::{ProtoError, Transport, soap, topology};
use fsonos_types::PlayerId;
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

#[path = "control/grouping_fixture.rs"]
mod grouping_fixture;
#[path = "control/queue_fixture.rs"]
mod queue_fixture;
use queue_fixture::QueueFixture;

const ZGS_S1: &str = include_str!("../../fsonos-proto/tests/fixtures/zgs_s1.xml");
const ZGS_S2: &str = include_str!("../../fsonos-proto/tests/fixtures/zgs_s2.xml");

fn household(body: &str) -> HouseholdState {
    let response = soap::parse_response(body, "GetZoneGroupState").unwrap();
    let zgs =
        topology::parse_zone_group_state(response.require("ZoneGroupState").unwrap()).unwrap();
    let mut st = HouseholdState::default();
    st.apply_topology(&zgs);
    st
}

fn pid(n: u8) -> PlayerId {
    PlayerId(format!("RINCON_000E58A000{n:02X}01400"))
}

fn ip(s: &str) -> IpAddr {
    s.parse().unwrap()
}

/// Keep request argument text XML-escaped when reflecting it in a response.
fn argument<'a>(body: &'a str, name: &str) -> &'a str {
    body.split_once(&format!("<{name}>"))
        .unwrap()
        .1
        .split_once(&format!("</{name}>"))
        .unwrap()
        .0
}

/// Records (host, action, body) and answers every action with `out_args`,
/// except `refuse`'s action, which gets its UPnP fault.
#[derive(Default)]
struct Recorder {
    sent: RefCell<Vec<(IpAddr, String, String)>>,
    out_args: &'static str,
    refuse: Option<(&'static str, u16)>,
}

impl Transport for Recorder {
    fn soap_post(
        &self,
        host: IpAddr,
        _control_path: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let action = soap_action
            .trim_matches('"')
            .rsplit('#')
            .next()
            .unwrap()
            .to_string();
        self.sent
            .borrow_mut()
            .push((host, action.clone(), body.into()));
        if let Some((refused, code)) = self.refuse
            && refused == action
        {
            return Err(ProtoError::SoapFault {
                code,
                reason: "refused".into(),
            });
        }
        if action == "GetZoneGroupState" {
            let topology = if household(ZGS_S2)
                .players
                .iter()
                .any(|player| player.ip == host)
            {
                ZGS_S2
            } else {
                ZGS_S1
            };
            return Ok(topology.to_string());
        }
        Ok(format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:{action}Response xmlns:u=\"urn:x\">{}</u:{action}Response></s:Body></s:Envelope>",
            self.out_args
        ))
    }
}

impl Recorder {
    fn actions(&self) -> Vec<(IpAddr, String)> {
        self.sent
            .borrow()
            .iter()
            .map(|(h, a, _)| (*h, a.clone()))
            .collect()
    }
}

fn house() -> Vec<HouseholdState> {
    vec![household(ZGS_S1), household(ZGS_S2)]
}

#[test]
fn group_commands_reach_the_coordinator_address() {
    let h = house();
    let t = Recorder::default();
    // RINCON_..02 coordinates the S1 group with the stereo-paired study.
    control::pause(&t, &h, &pid(2)).unwrap();
    control::resume(&t, &h, &pid(2)).unwrap();
    control::next(&t, &h, &pid(2)).unwrap();
    assert_eq!(
        t.actions(),
        [
            (ip("192.0.2.13"), "GetZoneGroupState".into()),
            (ip("192.0.2.13"), "Pause".into()),
            (ip("192.0.2.13"), "GetZoneGroupState".into()),
            (ip("192.0.2.13"), "Play".into()),
            (ip("192.0.2.13"), "GetPositionInfo".into()),
            (ip("192.0.2.13"), "GetZoneGroupState".into()),
            (ip("192.0.2.13"), "Next".into()),
        ]
    );
}

#[test]
fn a_refused_pause_is_read_back_and_a_playing_stream_stops() {
    let h = house();
    let refusing = |state: &'static str| Recorder {
        out_args: state,
        refuse: Some(("Pause", 701)),
        ..Recorder::default()
    };
    let names = |t: &Recorder| t.actions().into_iter().map(|(_, a)| a).collect::<Vec<_>>();
    // Still playing: a stream that cannot pause, so it stops.
    let t = refusing("<CurrentTransportState>PLAYING</CurrentTransportState>");
    control::pause(&t, &h, &pid(2)).unwrap();
    assert_eq!(
        names(&t),
        [
            "GetZoneGroupState",
            "Pause",
            "GetZoneGroupState",
            "GetTransportInfo",
            "GetZoneGroupState",
            "Stop"
        ]
    );
    // Already paused (or stopped): nothing more to do.
    let t = refusing("<CurrentTransportState>PAUSED_PLAYBACK</CurrentTransportState>");
    control::pause(&t, &h, &pid(2)).unwrap();
    assert_eq!(
        names(&t),
        [
            "GetZoneGroupState",
            "Pause",
            "GetZoneGroupState",
            "GetTransportInfo",
            "GetZoneGroupState"
        ]
    );
    // Any other fault stands, unread.
    let t = Recorder {
        refuse: Some(("Pause", 800)),
        ..Recorder::default()
    };
    assert!(matches!(
        control::pause(&t, &h, &pid(2)),
        Err(CoreError::Proto(ProtoError::SoapFault { code: 800, .. }))
    ));
    assert_eq!(names(&t), ["GetZoneGroupState", "Pause"]);
}

#[test]
fn play_uri_sets_the_uri_then_plays() {
    let h = house();
    let t = Recorder {
        out_args: "<CurrentURI>x-rincon-mp3radio://example.invalid/stream</CurrentURI>",
        ..Recorder::default()
    };
    control::play_uri(
        &t,
        &h,
        &pid(0x0A),
        "x-rincon-mp3radio://example.invalid/stream",
        "",
    )
    .unwrap();
    let sent = t.sent.borrow();
    assert_eq!(sent[0].1, "GetZoneGroupState");
    assert_eq!(sent[1].0, ip("192.0.2.19"));
    assert_eq!(sent[1].1, "SetAVTransportURI");
    assert!(
        sent[1]
            .2
            .contains("x-rincon-mp3radio://example.invalid/stream")
    );
    assert_eq!(sent.last().unwrap().1, "Play");
    assert!(
        sent[2..sent.len() - 1]
            .iter()
            .all(|(_, action, _)| action.starts_with("Get"))
    );
}

#[test]
fn room_volume_goes_to_the_room_player_and_group_volume_to_the_coordinator() {
    let h = house();
    let t = Recorder {
        out_args: "<NewVolume>27</NewVolume>",
        ..Recorder::default()
    };
    assert_eq!(control::set_volume(&t, &h, &pid(5), 130).unwrap(), 100);
    assert_eq!(control::adjust_volume(&t, &h, &pid(5), 3).unwrap(), 27);
    assert_eq!(
        control::adjust_group_volume(&t, &h, &pid(2), -2).unwrap(),
        27
    );
    assert_eq!(
        t.actions(),
        [
            (ip("192.0.2.12"), "GetZoneGroupState".into()),
            (ip("192.0.2.12"), "SetVolume".into()),
            (ip("192.0.2.12"), "GetZoneGroupState".into()),
            (ip("192.0.2.12"), "SetRelativeVolume".into()),
            (ip("192.0.2.13"), "GetZoneGroupState".into()),
            (ip("192.0.2.13"), "SnapshotGroupVolume".into()),
            (ip("192.0.2.13"), "GetZoneGroupState".into()),
            (ip("192.0.2.13"), "SetRelativeGroupVolume".into()),
        ]
    );
}

#[test]
fn join_targets_the_member_and_names_the_coordinator() {
    let h = house();
    let t = Recorder::default();
    // The fixed topology already places this room in the destination, so
    // the post-join confirmation observes the complete bonded room too.
    control::join(&t, &h, &pid(5), &pid(2)).unwrap();
    let sent = t.sent.borrow();
    assert_eq!(sent.len(), 4);
    assert_eq!(
        (&sent[0].0, sent[0].1.as_str()),
        (&ip("192.0.2.13"), "GetZoneGroupState")
    );
    assert_eq!(
        (&sent[1].0, sent[1].1.as_str()),
        (&ip("192.0.2.12"), "GetZoneGroupState")
    );
    assert_eq!(
        (&sent[2].0, sent[2].1.as_str()),
        (&ip("192.0.2.12"), "SetAVTransportURI")
    );
    assert!(sent[2].2.contains("x-rincon:RINCON_000E58A0000201400"));
    assert_eq!(
        (&sent[3].0, sent[3].1.as_str()),
        (&ip("192.0.2.13"), "GetZoneGroupState")
    );
    drop(sent);
    // Leave uses a self-join (docs/PROTOCOL.md §6). This fixed fixture already
    // shows the S2 room alone, so its complete departure is observable too.
    control::leave(&t, &h, &pid(0x0A)).unwrap();
    let sent = t.sent.borrow();
    assert_eq!(sent[4].1, "GetZoneGroupState");
    assert_eq!(sent[5].1, "SetAVTransportURI");
    assert!(sent[5].2.contains(&format!("x-rincon:{}", pid(0x0A).0)));
}

#[test]
fn unknown_players_are_refused_before_anything_is_sent() {
    let h = house();
    let t = Recorder::default();
    let ghost = PlayerId("RINCON_000E58A0009901400".into());
    assert!(matches!(
        control::pause(&t, &h, &ghost),
        Err(CoreError::UnknownPlayer(_))
    ));
    assert!(matches!(
        control::join(&t, &h, &pid(3), &ghost),
        Err(CoreError::UnknownPlayer(_))
    ));
    assert!(t.sent.borrow().is_empty());
}

#[test]
fn playback_reads_transport_and_position() {
    let h = house();
    let t = Recorder {
        out_args: "<CurrentTransportState>PLAYING</CurrentTransportState>\
                   <CurrentTransportStatus>OK</CurrentTransportStatus>\
                   <Track>2</Track><TrackDuration>0:03:00</TrackDuration>\
                   <TrackMetaData></TrackMetaData><TrackURI>x-file:a</TrackURI><RelTime>0:00:30</RelTime>",
        ..Recorder::default()
    };
    let p = control::playback(&t, &h, &pid(0x0A)).unwrap();
    assert_eq!(p.transport.state, fsonos_types::TransportState::Playing);
    assert_eq!((p.position.track, p.position.position_secs), (2, Some(30)));
}

const FAV_S1: &str = include_str!("../../fsonos-proto/tests/fixtures/browse_favorites_s1.xml");
const EMPTY: &str = include_str!("../../fsonos-proto/tests/fixtures/browse_queue_empty.xml");

/// Answers every Browse with one fixed response body.
struct Favorites(&'static str);

impl Transport for Favorites {
    fn soap_post(
        &self,
        _: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        assert_eq!(path, "/MediaServer/ContentDirectory/Control");
        assert!(action.ends_with("#Browse\""), "{action}");
        assert!(body.contains("<ObjectID>FV:2</ObjectID>"), "{body}");
        Ok(self.0.to_string())
    }
}

#[test]
fn spotify_tracks_render_with_params_from_the_households_favorites() {
    let h = house();
    let (uri, didl) = control::spotify_track_source(
        &Favorites(FAV_S1),
        &h,
        &pid(2),
        "spotify:track:0FixtureSpotify0000099",
        "Partita",
    )
    .unwrap()
    .expect("the S1 household has Spotify favorites");
    assert!(
        uri.starts_with("x-sonos-spotify:spotify%3atrack%3a0FixtureSpotify0000099?sid="),
        "{uri}"
    );
    assert!(didl.contains("SA_RINCON"), "{didl}");
    assert!(didl.contains("<dc:title>Partita</dc:title>"));
}

#[test]
fn a_household_without_spotify_favorites_has_nothing_to_render_with() {
    let h = house();
    assert_eq!(
        control::spotify_track_source(&Favorites(EMPTY), &h, &pid(2), "spotify:track:x", "x")
            .unwrap(),
        None
    );
}

/// Answers Browse with the S1 favorites or a coherent queue initially holding
/// four tracks; records every action with its body.
#[derive(Default)]
struct QueueLan {
    sent: RefCell<Vec<(IpAddr, String, String)>>,
    queue: QueueFixture,
    uri: RefCell<String>,
    metadata: RefCell<String>,
    position: Cell<u32>,
}

impl Transport for QueueLan {
    fn soap_post(
        &self,
        host: IpAddr,
        _: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let action = action
            .trim_matches('"')
            .rsplit('#')
            .next()
            .unwrap()
            .to_string();
        self.sent
            .borrow_mut()
            .push((host, action.clone(), body.into()));
        if action == "GetZoneGroupState" {
            let topology = if household(ZGS_S2)
                .players
                .iter()
                .any(|player| player.ip == host)
            {
                ZGS_S2
            } else {
                ZGS_S1
            };
            return Ok(topology.to_string());
        }
        if action == "Browse" {
            return Ok(if body.contains("<ObjectID>Q:0</ObjectID>") {
                self.queue.browse(body)
            } else {
                FAV_S1.to_string()
            });
        }
        let out = match action.as_str() {
            "GetMediaInfo" => format!(
                "<CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData>",
                self.uri.borrow(),
                self.metadata.borrow()
            ),
            "SetAVTransportURI" => {
                *self.uri.borrow_mut() = argument(body, "CurrentURI").into();
                *self.metadata.borrow_mut() = argument(body, "CurrentURIMetaData").into();
                self.position.set(1);
                String::new()
            }
            "Seek" => {
                self.position.set(argument(body, "Target").parse().unwrap());
                String::new()
            }
            "GetPositionInfo" => soap::args_xml(&[
                ("Track", &self.position.get().to_string()),
                (
                    "TrackURI",
                    self.queue.at(self.position.get()).as_deref().unwrap_or(""),
                ),
                ("TrackDuration", "0:04:00"),
                ("RelTime", "0:00:00"),
            ]),
            "AddURIToQueue" => self.queue.add(),
            "RemoveTrackRangeFromQueue" => self.queue.remove(body),
            _ => String::new(),
        };
        Ok(format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:{action}Response xmlns:u=\"urn:x\">{out}</u:{action}Response></s:Body></s:Envelope>"
        ))
    }
}

#[test]
fn spotify_tracks_queue_in_order_and_play_continues_through_the_queue() {
    let h = house();
    let t = QueueLan::default();
    let first = control::queue_spotify_tracks(
        &t,
        &h,
        &pid(2),
        &[
            ("spotify:track:0FixtureSpotify0000101", "I. Allegro"),
            ("spotify:track:0FixtureSpotify0000102", "II. Andante"),
        ],
    )
    .unwrap();
    assert_eq!(first, Some(5));
    control::play_queue_from(&t, &h, &pid(2), 5).unwrap();

    let sent = t.sent.borrow();
    let actions: Vec<&str> = sent
        .iter()
        .map(|(_, a, _)| a.as_str())
        .filter(|action| !action.starts_with("Get") && *action != "Browse")
        .collect();
    assert_eq!(
        actions,
        [
            "AddURIToQueue",
            "AddURIToQueue",
            "SetAVTransportURI",
            "Seek",
            "Play"
        ]
    );
    assert!(
        sent.iter().all(|(host, ..)| *host == ip("192.0.2.13")),
        "all on the coordinator"
    );
    // Bare Spotify URIs, in order, each with the household's descriptor.
    assert!(
        sent[1]
            .2
            .contains("<EnqueuedURI>spotify%3atrack%3a0FixtureSpotify0000101</EnqueuedURI>")
    );
    assert!(
        sent[2]
            .2
            .contains("<EnqueuedURI>spotify%3atrack%3a0FixtureSpotify0000102</EnqueuedURI>")
    );
    assert!(sent[1].2.contains("SA_RINCON"), "{}", sent[1].2);
    assert!(
        sent.iter()
            .find(|(_, action, _)| action == "SetAVTransportURI")
            .unwrap()
            .2
            .contains("x-rincon-queue:RINCON_000E58A0000201400#0")
    );
    assert!(
        sent.iter()
            .find(|(_, action, _)| action == "Seek")
            .unwrap()
            .2
            .contains("<Unit>TRACK_NR</Unit><Target>5</Target>")
    );
}

#[test]
fn nothing_is_queued_without_spotify_favorites_or_tracks() {
    let h = house();
    assert_eq!(
        control::queue_spotify_tracks(&Favorites(EMPTY), &h, &pid(2), &[("spotify:track:x", "x")])
            .unwrap(),
        None
    );
    let t = QueueLan::default();
    assert_eq!(
        control::queue_spotify_tracks(&t, &h, &pid(2), &[]).unwrap(),
        None
    );
    assert!(!t.sent.borrow().iter().any(|(_, a, _)| a == "AddURIToQueue"));
}

/// How the drifting lan misbehaves on `SetAVTransportURI`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Drift {
    /// The first render (stale `sn=1`) is refused 800; after a relearn the
    /// fresh `sn=7` render succeeds.
    Recovers,
    /// Every render is refused 800, even after the relearn.
    Persistent,
    /// The first render fails with a non-800 fault, which must propagate
    /// without a retry.
    OtherFault,
}

/// A lan whose household's Spotify render parameters drift: the first
/// `Browse FV:2` serves favorites whose track carries `sn=1`, later browses
/// serve the same favorites with `sn=7` (Spotify was relinked). The renderer
/// refuses a stale render with UPnP 800, per `mode`.
struct DriftLan {
    mode: Drift,
    browses: std::cell::Cell<usize>,
    renders: RefCell<Vec<String>>,
    plays: std::cell::Cell<usize>,
}

impl DriftLan {
    fn new(mode: Drift) -> Self {
        Self {
            mode,
            browses: std::cell::Cell::new(0),
            renders: RefCell::default(),
            plays: std::cell::Cell::new(0),
        }
    }

    fn ok(action: &str) -> String {
        format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\">\
             <s:Body><u:{action}Response xmlns:u=\"urn:x\">\
             </u:{action}Response></s:Body></s:Envelope>"
        )
    }

    fn fault(code: u16) -> ProtoError {
        ProtoError::SoapFault {
            code,
            reason: "refused".to_string(),
        }
    }
}

impl Transport for DriftLan {
    fn soap_post(
        &self,
        _: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action
            .trim_matches('"')
            .rsplit('#')
            .next()
            .unwrap()
            .to_string();
        match name.as_str() {
            "GetZoneGroupState" => Ok(ZGS_S1.to_string()),
            "GetMediaInfo" => {
                let renders = self.renders.borrow();
                let selected = renders.last().unwrap();
                let out = format!(
                    "<CurrentURI>{}</CurrentURI><CurrentURIMetaData>{}</CurrentURIMetaData>",
                    argument(selected, "CurrentURI"),
                    argument(selected, "CurrentURIMetaData")
                );
                Ok(Self::ok("GetMediaInfo").replace(
                    "</u:GetMediaInfoResponse>",
                    &format!("{out}</u:GetMediaInfoResponse>"),
                ))
            }
            "Browse" => {
                assert_eq!(path, "/MediaServer/ContentDirectory/Control");
                let stale = self.browses.get() == 0;
                self.browses.set(self.browses.get() + 1);
                Ok(if stale {
                    FAV_S1.to_string()
                } else {
                    FAV_S1.replace("sn=1", "sn=7")
                })
            }
            "SetAVTransportURI" => {
                self.renders.borrow_mut().push(body.to_string());
                let stale = body.contains("sn=1<");
                match self.mode {
                    Drift::Recovers if stale => Err(Self::fault(800)),
                    Drift::Persistent => Err(Self::fault(800)),
                    Drift::OtherFault => Err(Self::fault(701)),
                    Drift::Recovers => Ok(Self::ok("SetAVTransportURI")),
                }
            }
            "Play" => {
                self.plays.set(self.plays.get() + 1);
                Ok(Self::ok("Play"))
            }
            other => panic!("unexpected action {other}"),
        }
    }
}

#[test]
fn render_800_relearns_params_and_retries_once() {
    let h = house();
    let t = DriftLan::new(Drift::Recovers);
    control::play_spotify_track(
        &t,
        &h,
        &pid(2),
        "spotify:track:0FixtureSpotify0000002",
        "Drift Test",
    )
    .unwrap();
    assert_eq!(t.browses.get(), 2, "one browse before the 800, one relearn");
    let renders = t.renders.borrow();
    assert_eq!(renders.len(), 2, "exactly one retry");
    assert!(renders[0].contains("sn=1<"), "{}", renders[0]);
    assert!(renders[1].contains("sn=7<"), "{}", renders[1]);
    assert_eq!(t.plays.get(), 1);
}

#[test]
fn persistent_render_800_reports_stale_after_one_retry() {
    let h = house();
    let t = DriftLan::new(Drift::Persistent);
    let err = control::play_spotify_track(
        &t,
        &h,
        &pid(2),
        "spotify:track:0FixtureSpotify0000002",
        "Drift Test",
    )
    .unwrap_err();
    assert!(matches!(err, CoreError::RenderParamsStale), "{err}");
    assert_eq!(t.renders.borrow().len(), 2, "the retry never loops");
    assert_eq!(t.browses.get(), 2);
    assert_eq!(
        t.plays.get(),
        0,
        "nothing starts when the render is refused"
    );
}

#[test]
fn a_non_800_render_fault_propagates_without_retry() {
    let h = house();
    let t = DriftLan::new(Drift::OtherFault);
    let err = control::play_spotify_track(
        &t,
        &h,
        &pid(2),
        "spotify:track:0FixtureSpotify0000002",
        "Drift Test",
    )
    .unwrap_err();
    match err {
        CoreError::Proto(ProtoError::SoapFault { code, .. }) => assert_eq!(code, 701),
        other => panic!("expected the raw fault, got {other:?}"),
    }
    assert_eq!(t.renders.borrow().len(), 1, "no retry for foreign faults");
    assert_eq!(t.browses.get(), 1);
}

#[test]
fn a_spotify_album_replaces_the_queue_and_plays_from_its_first_track() {
    let h = house();
    let t = QueueLan::default();
    control::play_spotify_container(
        &t,
        &h,
        &pid(2),
        SpotifyContainer::Album,
        "spotify:album:0FixtureSpotify0000201",
        "Symphonies",
    )
    .unwrap();

    let sent = t.sent.borrow();
    let actions: Vec<&str> = sent
        .iter()
        .map(|(_, action, _)| action.as_str())
        .filter(|action| !action.starts_with("Get") && *action != "Browse")
        .collect();
    assert_eq!(
        actions,
        [
            "AddURIToQueue",
            "RemoveTrackRangeFromQueue",
            "SetAVTransportURI",
            "Seek",
            "Play"
        ]
    );
    assert!(
        sent.iter().all(|(host, ..)| *host == ip("192.0.2.13")),
        "all on the coordinator"
    );
    // The household's own album prefix and account, as an album.
    let add = &sent
        .iter()
        .find(|(_, action, _)| action == "AddURIToQueue")
        .unwrap()
        .2;
    assert!(
        add.contains(
            "<EnqueuedURI>x-rincon-cpcontainer:1004206cspotify%3aalbum%3a0FixtureSpotify0000201\
             ?sid=12&amp;flags=8300&amp;sn=1</EnqueuedURI>"
        ),
        "{add}"
    );
    assert!(
        add.contains("object.container.album.musicAlbum")
            && add.contains("SA_RINCON3079_X_#Svc3079-0-Token"),
        "{add}"
    );
    assert!(
        sent.iter()
            .find(|(_, action, _)| action == "Seek")
            .unwrap()
            .2
            .contains("<Unit>TRACK_NR</Unit><Target>1</Target>")
    );

    // A playlist, on a household with no playlist favorite: the common prefix.
    let t = QueueLan::default();
    control::play_spotify_container(
        &t,
        &h,
        &pid(2),
        SpotifyContainer::Playlist,
        "spotify:playlist:0FixtureSpotify0000202",
        "Mix",
    )
    .unwrap();
    let sent = t.sent.borrow();
    let add = &sent
        .iter()
        .find(|(_, action, _)| action == "AddURIToQueue")
        .unwrap()
        .2;
    assert!(
        add.contains("x-rincon-cpcontainer:1006206cspotify%3aplaylist%3a0FixtureSpotify0000202"),
        "{}",
        add
    );
    assert!(add.contains("object.container.playlistContainer"));
}

#[test]
fn bare_spotify_kinds_enqueue_with_the_sid_only() {
    let h = house();
    for (kind, class) in [
        ("artist", "object.container.person.musicArtist"),
        ("episode", "object.item.audioItem.musicTrack"),
        ("show", "object.container"),
    ] {
        let t = QueueLan::default();
        let uri = format!("spotify:{kind}:0FixtureSpotify0000203");
        let added = control::queue_spotify_tracks(&t, &h, &pid(2), &[(&uri, "Bare")]).unwrap();
        assert_eq!(added, Some(5), "{kind}");
        let sent = t.sent.borrow();
        let add = sent
            .iter()
            .map(|(_, _, body)| body)
            .find(|body| body.contains("EnqueuedURI"))
            .unwrap_or_else(|| panic!("{kind}: no enqueue sent: {sent:?}"));
        assert!(
            add.contains(&format!(
                "x-sonos-spotify:spotify%3a{kind}%3a0FixtureSpotify0000203?sid=12"
            )),
            "{kind}: {add}"
        );
        assert!(add.contains(class), "{kind}: {add}");
        assert!(!add.contains("flags="), "{kind}: {add}");
        assert!(!add.contains("sn="), "{kind}: {add}");
    }
}

#[test]
fn a_household_without_spotify_favorites_keeps_its_queue() {
    let h = house();
    // `Favorites` answers Browse only: clearing the queue would fail the test.
    let err = control::play_spotify_container(
        &Favorites(EMPTY),
        &h,
        &pid(2),
        SpotifyContainer::Album,
        "spotify:album:x",
        "x",
    )
    .unwrap_err();
    assert!(matches!(err, CoreError::NoSpotifyFavorite), "{err}");
}

/// Serves the S1 favorites with `sn=1` on the first browse and `sn=7` after
/// (Spotify was relinked), and refuses a container enqueue that carries
/// `sn=1` with UPnP 800 (every one, when `persistent`). With `at_play` the
/// enqueue goes through and the Play after it is refused instead.
struct ContainerDrift {
    persistent: bool,
    at_play: bool,
    browses: std::cell::Cell<usize>,
    actions: RefCell<Vec<String>>,
    adds: RefCell<Vec<String>>,
    queue: QueueFixture,
    selected: std::cell::Cell<bool>,
    playing: std::cell::Cell<bool>,
}

impl ContainerDrift {
    fn new(persistent: bool) -> Self {
        Self {
            persistent,
            at_play: false,
            browses: std::cell::Cell::new(0),
            actions: RefCell::default(),
            adds: RefCell::default(),
            queue: QueueFixture::new(0),
            selected: std::cell::Cell::new(false),
            playing: std::cell::Cell::new(false),
        }
    }

    fn refusing_at_play(persistent: bool) -> Self {
        Self {
            at_play: true,
            ..Self::new(persistent)
        }
    }

    /// The renderer refuses what carries the stale `sn=1` (anything, when
    /// `persistent`).
    fn refuses(&self, body: &str) -> bool {
        self.persistent || body.contains("sn=1<")
    }
}

impl Transport for ContainerDrift {
    fn soap_post(
        &self,
        _: IpAddr,
        _: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action
            .trim_matches('"')
            .rsplit('#')
            .next()
            .unwrap()
            .to_string();
        self.actions.borrow_mut().push(name.clone());
        let out = match name.as_str() {
            "GetZoneGroupState" => return Ok(ZGS_S1.to_string()),
            "GetMediaInfo" => {
                let uri = if self.selected.get() {
                    format!("x-rincon-queue:{}#0", pid(2).0)
                } else {
                    String::new()
                };
                soap::args_xml(&[("CurrentURI", &uri), ("CurrentURIMetaData", "")])
            }
            "GetTransportSettings" => "<PlayMode>NORMAL</PlayMode>".into(),
            "GetTransportInfo" => soap::args_xml(&[
                (
                    "CurrentTransportState",
                    if self.playing.get() {
                        "PLAYING"
                    } else {
                        "STOPPED"
                    },
                ),
                ("CurrentTransportStatus", "OK"),
            ]),
            "GetPositionInfo" => {
                let uri = self.selected.get().then(|| self.queue.first()).flatten();
                soap::args_xml(&[
                    ("Track", if uri.is_some() { "1" } else { "0" }),
                    ("TrackURI", uri.as_deref().unwrap_or("")),
                    ("TrackDuration", "0:04:00"),
                    ("RelTime", "0:00:00"),
                ])
            }
            "SetAVTransportURI" => {
                self.selected.set(true);
                self.playing.set(false);
                String::new()
            }
            "Browse" => {
                if body.contains("<ObjectID>Q:0</ObjectID>") {
                    return Ok(self.queue.browse(body));
                }
                let stale = self.browses.get() == 0;
                self.browses.set(self.browses.get() + 1);
                return Ok(if stale {
                    FAV_S1.to_string()
                } else {
                    FAV_S1.replace("sn=1", "sn=7")
                });
            }
            "AddURIToQueue" => {
                self.adds.borrow_mut().push(body.to_string());
                if !self.at_play && self.refuses(body) {
                    return Err(ProtoError::SoapFault {
                        code: 800,
                        reason: "refused".to_string(),
                    });
                }
                self.queue.add()
            }
            "RemoveTrackRangeFromQueue" => self.queue.remove(body),
            "Play"
                if self.at_play
                    && self
                        .adds
                        .borrow()
                        .last()
                        .is_some_and(|added| self.refuses(added)) =>
            {
                return Err(ProtoError::SoapFault {
                    code: 800,
                    reason: "refused".to_string(),
                });
            }
            "Play" => {
                self.playing.set(true);
                String::new()
            }
            _ => String::new(),
        };
        Ok(format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:{name}Response xmlns:u=\"urn:x\">{out}</u:{name}Response></s:Body></s:Envelope>"
        ))
    }
}

#[test]
fn a_refused_container_relearns_once_then_reports_stale() {
    let h = house();
    let play = |t: &ContainerDrift| {
        control::play_spotify_container(
            t,
            &h,
            &pid(2),
            SpotifyContainer::Album,
            "spotify:album:0FixtureSpotify0000201",
            "Symphonies",
        )
    };

    let t = ContainerDrift::new(false);
    play(&t).unwrap();
    assert_eq!(t.browses.get(), 2, "one browse before the 800, one relearn");
    let adds = t.adds.borrow();
    assert_eq!(adds.len(), 2, "exactly one retry");
    assert!(adds[0].contains("sn=1<") && adds[1].contains("sn=7<"));
    assert_eq!(t.actions.borrow().last().map(String::as_str), Some("Play"));

    let t = ContainerDrift::new(true);
    let err = play(&t).unwrap_err();
    assert!(matches!(err, CoreError::RenderParamsStale), "{err}");
    assert_eq!(t.adds.borrow().len(), 2, "the retry never loops");
    assert!(
        !t.actions.borrow().iter().any(|a| a == "Play"),
        "nothing starts when the render is refused"
    );
}

/// Answers every action with the S1 topology.
struct Topology;

impl Transport for Topology {
    fn soap_post(&self, _: IpAddr, _: &str, action: &str, _: &str) -> Result<String, ProtoError> {
        assert!(action.ends_with("#GetZoneGroupState\""), "{action}");
        Ok(ZGS_S1.to_string())
    }
}

#[test]
fn the_coordinator_now_comes_from_the_players_own_topology() {
    let h = house();
    // The study plays in the den's group; the bedroom leads its own.
    let now = |n| fsonos_core::moving::coordinator_now(&Topology, &h, &pid(n));
    assert_eq!(now(4).unwrap(), pid(2));
    assert_eq!(now(3).unwrap(), pid(3));
    assert!(matches!(
        fsonos_core::moving::coordinator_now(&Topology, &h, &PlayerId("RINCON_NOWHERE".into())),
        Err(CoreError::UnknownPlayer(_))
    ));
}

#[test]
fn a_container_refused_at_play_never_replays_the_committed_replacement() {
    let h = house();
    for persistent in [false, true] {
        let t = ContainerDrift::refusing_at_play(persistent);
        let error = control::play_spotify_container(
            &t,
            &h,
            &pid(2),
            SpotifyContainer::Album,
            "spotify:album:0FixtureSpotify0000201",
            "Symphonies",
        )
        .unwrap_err();
        assert!(matches!(error, CoreError::Uncertain(_)), "{error}");
        assert_eq!(t.browses.get(), 1, "a committed source is not relearned");
        assert_eq!(
            t.adds.borrow().len(),
            1,
            "the replacement is never repeated"
        );
        assert_eq!(
            t.actions.borrow().iter().filter(|a| *a == "Play").count(),
            1,
            "the refused Play is not replayed"
        );
    }
}

const ZGS_S1_STUDY_LEFT: &str = include_str!("fixtures/zgs_s1_study_left.xml");
const ZGS_S1_BEDROOM_HANDED_OVER: &str = include_str!("fixtures/zgs_s1_bedroom_handed_over.xml");

/// Delegation answers success, and the topology shows the hand-over as a
/// real S1 stereo pair does it (2026-10-14): Bedroom (a pair) leads a group
/// of its own, its hidden half left behind in Bedroom 2's group.
#[derive(Default)]
struct HandedOver {
    sent: RefCell<Vec<(IpAddr, String, String)>>,
    reunited: Cell<bool>,
}

impl Transport for HandedOver {
    fn soap_post(
        &self,
        host: IpAddr,
        _: &str,
        soap_action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let action = soap_action
            .trim_matches('"')
            .rsplit('#')
            .next()
            .unwrap()
            .to_string();
        self.sent
            .borrow_mut()
            .push((host, action.clone(), body.into()));
        if action == "SetAVTransportURI"
            && host == ip("192.0.2.17")
            && body.contains(&format!("x-rincon:{}", pid(3).0))
        {
            self.reunited.set(true);
        }
        Ok(if action == "GetZoneGroupState" {
            if self.reunited.get() {
                grouping_fixture::relocate(ZGS_S1_BEDROOM_HANDED_OVER, &pid(7), &pid(3))
            } else if self
                .sent
                .borrow()
                .iter()
                .any(|(_, action, _)| action == "DelegateGroupCoordinationTo")
            {
                ZGS_S1_BEDROOM_HANDED_OVER.to_string()
            } else {
                ZGS_S1.to_string()
            }
        } else {
            format!(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                 <u:{action}Response xmlns:u=\"urn:x\"></u:{action}Response></s:Body></s:Envelope>"
            )
        })
    }
}

#[test]
fn a_pair_that_hands_its_group_on_takes_its_hidden_half_back() {
    let h = house();
    let t = HandedOver::default();
    control::leave(&t, &h, &pid(3)).expect("Bedroom stands alone");
    let sent = t.sent.borrow();
    assert_eq!(sent[0].1, "GetZoneGroupState", "{sent:?}");
    assert_eq!(sent[1].0, ip("192.0.2.15"));
    assert_eq!(sent[1].1, "DelegateGroupCoordinationTo", "{sent:?}");
    assert!(
        sent[1]
            .2
            .contains(&format!("<NewCoordinator>{}</NewCoordinator>", pid(8).0))
    );
    assert!(sent[1].2.contains("<RejoinGroup>0</RejoinGroup>"));
    // The pair's hidden half (192.0.2.17) is asked back to its primary.
    let rejoin = sent
        .iter()
        .find(|(host, action, _)| *host == ip("192.0.2.17") && action == "SetAVTransportURI")
        .expect("the hidden half was asked back");
    assert!(rejoin.2.contains(&format!("x-rincon:{}", pid(3).0)));
    // Bedroom 2, which took the group over, is left where it is.
    assert!(
        !sent
            .iter()
            .any(|(host, action, _)| *host == ip("192.0.2.16") && action == "SetAVTransportURI")
    );
}

/// Serves the study's own reads stale (it is still grouped) and every other
/// player fresh (the study left, and leads a group of its own) — what the
/// fleet does to a player that just announced (bead frankensonos-p9f). The
/// leave verb itself is answered with the benign 402.
struct StaleSelf {
    study: IpAddr,
    reunited: Cell<bool>,
    writes: RefCell<Vec<IpAddr>>,
}

impl Transport for StaleSelf {
    fn soap_post(
        &self,
        host: IpAddr,
        _: &str,
        action: &str,
        _: &str,
    ) -> Result<String, ProtoError> {
        if action.ends_with("#SetAVTransportURI\"") {
            self.writes.borrow_mut().push(host);
            if host == ip("192.0.2.11") {
                self.reunited.set(true);
                return Ok(soap::envelope(
                    &soap::AV_TRANSPORT,
                    "SetAVTransportURIResponse",
                    "",
                ));
            }
            return Err(ProtoError::SoapFault {
                code: 402,
                reason: "asked to leave its group, the player is still in it".into(),
            });
        }
        assert!(action.ends_with("#GetZoneGroupState\""), "{action}");
        Ok(if host == self.study {
            ZGS_S1.to_string()
        } else if self.reunited.get() {
            grouping_fixture::relocate(ZGS_S1_STUDY_LEFT, &pid(4), &pid(5))
        } else {
            ZGS_S1_STUDY_LEFT.to_string()
        })
    }
}

#[test]
fn a_stale_own_read_back_is_confirmed_by_a_witness_player() {
    let h = house();
    let t = StaleSelf {
        study: ip("192.0.2.12"),
        reunited: Cell::new(false),
        writes: RefCell::new(Vec::new()),
    };
    let started = std::time::Instant::now();
    control::leave(&t, &h, &pid(5)).expect("the witness read confirms the leave");
    assert!(t.reunited.get(), "the study's bonded half follows it out");
    assert_eq!(
        *t.writes.borrow(),
        [ip("192.0.2.12"), ip("192.0.2.11")],
        "one self-join and one reunion, without repeating either"
    );
    assert!(
        started.elapsed() < std::time::Duration::from_secs(2),
        "confirmed on the first poll, not after the wait: {:?}",
        started.elapsed()
    );
}
