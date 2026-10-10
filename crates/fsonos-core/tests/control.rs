//! `control` sends each command to the right player: group-wide commands to
//! the coordinator's address, room volume to the room's own player, joins to
//! the member. Households come from the scrubbed S1/S2 fixtures.

use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::didl::SpotifyContainer;
use fsonos_proto::{ProtoError, Transport, soap, topology};
use fsonos_types::PlayerId;
use std::cell::RefCell;
use std::net::IpAddr;

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

/// Records (host, action, body) and answers every action with `out_args`.
#[derive(Default)]
struct Recorder {
    sent: RefCell<Vec<(IpAddr, String, String)>>,
    out_args: &'static str,
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
            (ip("192.0.2.13"), "Pause".into()),
            (ip("192.0.2.13"), "Play".into()),
            (ip("192.0.2.13"), "Next".into()),
        ]
    );
}

#[test]
fn play_uri_sets_the_uri_then_plays() {
    let h = house();
    let t = Recorder::default();
    control::play_uri(
        &t,
        &h,
        &pid(0x0A),
        "x-rincon-mp3radio://example.invalid/stream",
        "",
    )
    .unwrap();
    let sent = t.sent.borrow();
    assert_eq!(sent.len(), 2);
    assert_eq!(sent[0].0, ip("192.0.2.19"));
    assert_eq!(sent[0].1, "SetAVTransportURI");
    assert!(
        sent[0]
            .2
            .contains("x-rincon-mp3radio://example.invalid/stream")
    );
    assert_eq!(sent[1].1, "Play");
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
            (ip("192.0.2.12"), "SetVolume".into()),
            (ip("192.0.2.12"), "SetRelativeVolume".into()),
            (ip("192.0.2.13"), "SnapshotGroupVolume".into()),
            (ip("192.0.2.13"), "SetRelativeGroupVolume".into()),
        ]
    );
}

#[test]
fn join_targets_the_member_and_names_the_coordinator() {
    let h = house();
    let t = Recorder::default();
    control::join(&t, &h, &pid(3), &pid(2)).unwrap();
    let sent = t.sent.borrow();
    assert_eq!(sent[0].0, ip("192.0.2.15"));
    assert!(sent[0].2.contains("x-rincon:RINCON_000E58A0000201400"));
    drop(sent);
    control::leave(&t, &h, &pid(3)).unwrap();
    assert_eq!(
        t.actions()[1],
        (
            ip("192.0.2.15"),
            "BecomeCoordinatorOfStandaloneGroup".into()
        )
    );
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

/// Answers Browse with the S1 favorites and AddURIToQueue with consecutive
/// queue positions from 5; records every action with its body.
#[derive(Default)]
struct QueueLan {
    sent: RefCell<Vec<(IpAddr, String, String)>>,
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
        if action == "Browse" {
            return Ok(FAV_S1.to_string());
        }
        let enqueued = self
            .sent
            .borrow()
            .iter()
            .filter(|(_, a, _)| a == "AddURIToQueue")
            .count();
        let out = if action == "AddURIToQueue" {
            format!(
                "<FirstTrackNumberEnqueued>{}</FirstTrackNumberEnqueued>",
                4 + enqueued
            )
        } else {
            String::new()
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
    let actions: Vec<&str> = sent.iter().map(|(_, a, _)| a.as_str()).collect();
    assert_eq!(
        actions,
        [
            "Browse",
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
        sent[3]
            .2
            .contains("x-rincon-queue:RINCON_000E58A0000201400#0")
    );
    assert!(
        sent[4]
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
    let actions: Vec<&str> = sent.iter().map(|(_, a, _)| a.as_str()).collect();
    assert_eq!(
        actions,
        [
            "Browse",
            "RemoveAllTracksFromQueue",
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
    // The household's own album prefix and account, as an album.
    let add = &sent[2].2;
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
        sent[4]
            .2
            .contains("<Unit>TRACK_NR</Unit><Target>5</Target>")
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
    assert!(
        sent[2]
            .2
            .contains("x-rincon-cpcontainer:1006206cspotify%3aplaylist%3a0FixtureSpotify0000202"),
        "{}",
        sent[2].2
    );
    assert!(sent[2].2.contains("object.container.playlistContainer"));
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
/// `sn=1` with UPnP 800 (every one, when `persistent`).
struct ContainerDrift {
    persistent: bool,
    browses: std::cell::Cell<usize>,
    actions: RefCell<Vec<String>>,
    adds: RefCell<Vec<String>>,
}

impl ContainerDrift {
    fn new(persistent: bool) -> Self {
        Self {
            persistent,
            browses: std::cell::Cell::new(0),
            actions: RefCell::default(),
            adds: RefCell::default(),
        }
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
            "Browse" => {
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
                if self.persistent || body.contains("sn=1<") {
                    return Err(ProtoError::SoapFault {
                        code: 800,
                        reason: "refused".to_string(),
                    });
                }
                "<FirstTrackNumberEnqueued>1</FirstTrackNumberEnqueued>"
            }
            _ => "",
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
