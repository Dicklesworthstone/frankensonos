//! `control` sends each command to the right player: group-wide commands to
//! the coordinator's address, room volume to the room's own player, joins to
//! the member. Households come from the scrubbed S1/S2 fixtures.

use fsonos_core::{CoreError, HouseholdState, control};
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
