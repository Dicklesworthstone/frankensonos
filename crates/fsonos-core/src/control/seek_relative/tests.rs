use super::*;
use std::cell::RefCell;
use std::collections::VecDeque;
use std::net::Ipv4Addr;

fn position(at: Option<u32>, duration: Option<u32>) -> PositionInfo {
    PositionInfo {
        track: 2,
        uri: "x-sonos-spotify:synthetic-track".into(),
        position_secs: at,
        duration_secs: duration,
        metadata: None,
    }
}

#[test]
fn unknown_position_is_not_the_beginning_of_a_track() {
    assert!(destination(&position(None, Some(180)), 30).is_err());
    assert_eq!(destination(&position(Some(0), Some(180)), 30).unwrap(), 30);
}

#[test]
fn relative_targets_saturate_and_stay_before_a_known_end() {
    let p = position(Some(70), Some(180));
    for (by, expected) in [(30, 100), (-15, 55), (i64::MIN, 0), (i64::MAX, 179)] {
        assert_eq!(destination(&p, by).unwrap(), expected);
    }
    assert_eq!(destination(&position(Some(0), Some(1)), 30).unwrap(), 0);
}

#[test]
fn unknown_duration_is_not_invented_and_invalid_samples_never_seek() {
    for duration in [None, Some(0)] {
        assert_eq!(destination(&position(Some(70), duration), 30).unwrap(), 100);
        assert_eq!(
            destination(&position(Some(70), duration), i64::MAX).unwrap(),
            u32::MAX
        );
    }
    assert!(destination(&position(Some(181), Some(180)), -15).is_err());
    let mut empty = position(Some(70), Some(180));
    empty.uri.clear();
    assert!(destination(&empty, 30).is_err());
}

#[test]
fn confirmation_requires_a_matching_track_slot_duration_and_clock() {
    let before = position(Some(70), Some(180));
    let good = position(Some(100), Some(180));
    let prove = |after: &PositionInfo| {
        reached(&before, after, 100, TransportState::Paused, Duration::ZERO)
    };
    assert!(prove(&good));
    for bad in [
        position(None, Some(180)),
        position(Some(99), Some(180)),
        position(Some(100), None),
    ] {
        assert!(!prove(&bad));
    }
    let mut other = good.clone();
    other.track = 3;
    assert!(!prove(&other));
    other = good.clone();
    other.uri.push_str("-other");
    assert!(!prove(&other));
}

#[test]
fn only_playing_can_advance_and_confirmation_has_a_finite_age() {
    let before = position(Some(70), Some(180));
    let after = position(Some(103), Some(180));
    assert!(reached(&before, &after, 100, TransportState::Playing, Duration::from_secs(2)));
    for state in [
        TransportState::Paused,
        TransportState::Stopped,
        TransportState::Unknown,
        TransportState::Transitioning,
    ] {
        assert!(!reached(&before, &after, 100, state, Duration::from_secs(2)));
    }
    assert!(!reached(&before, &after, 100, TransportState::Playing, Duration::ZERO));
    assert!(!reached(&before, &after, 100, TransportState::Playing, Duration::from_secs(31)));
    assert!(!reached(
        &before,
        &position(Some(180), Some(180)),
        179,
        TransportState::Playing,
        Duration::from_secs(2)
    ));
}

fn reply(action: &str, args: &str) -> String {
    format!(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
         <u:{action}Response xmlns:u=\"urn:x\">{args}</u:{action}Response></s:Body></s:Envelope>"
    )
}

fn clock(at: &str, slot: u32) -> String {
    reply("GetPositionInfo", &format!(
        "<Track>{slot}</Track><TrackURI>synthetic:track</TrackURI>\
         <TrackDuration>00:03:00</TrackDuration><RelTime>{at}</RelTime>"
    ))
}

fn lost() -> ProtoError {
    ProtoError::Network {
        target: "synthetic player".into(),
        detail: "seek reply lost".into(),
    }
}

type Step = (&'static str, Result<String, ProtoError>);

struct Script(RefCell<VecDeque<Step>>);

impl Script {
    fn new(replies: Vec<Step>) -> Self {
        Self(RefCell::new(replies.into()))
    }

    fn seek(&self) -> Result<u32, ProtoError> {
        let result = seek(
            self,
            Ipv4Addr::LOCALHOST.into(),
            &PlayerId("synthetic-player".into()),
            30,
        );
        assert!(self.0.borrow().is_empty(), "all planned reads should be consumed");
        result
    }
}

impl Transport for Script {
    fn soap_post(
        &self,
        _: IpAddr,
        _: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let (expected, reply) = self
            .0
            .borrow_mut()
            .pop_front()
            .expect("no unplanned I/O or replay");
        assert!(action.ends_with(&format!("#{expected}\"")), "{action}");
        if expected == "Seek" {
            assert!(body.contains("<Unit>REL_TIME</Unit>"));
            assert!(body.contains("<Target>00:01:40</Target>"));
        }
        reply
    }
}

fn recovery(last: String) -> Vec<Step> {
    vec![
        ("GetPositionInfo", Ok(clock("00:01:40", 2))),
        (
            "GetTransportInfo",
            Ok(reply(
                "GetTransportInfo",
                "<CurrentTransportState>PAUSED_PLAYBACK</CurrentTransportState>",
            )),
        ),
        (
            "GetZoneGroupState",
            Ok(reply(
                "GetZoneGroupState",
                "<ZoneGroupState>&lt;ZoneGroups&gt;\
                 &lt;ZoneGroup Coordinator=\"synthetic-player\" ID=\"group\"&gt;\
                 &lt;ZoneGroupMember UUID=\"synthetic-player\" ZoneName=\"Test\" \
                 Location=\"http://127.0.0.1:1400/xml/device_description.xml\"/&gt;\
                 &lt;/ZoneGroup&gt;&lt;/ZoneGroups&gt;</ZoneGroupState>",
            )),
        ),
        ("GetPositionInfo", Ok(last)),
    ]
}

#[test]
fn normal_success_keeps_the_existing_one_read_one_write_path() {
    let script = Script::new(vec![
        ("GetPositionInfo", Ok(clock("00:01:10", 2))),
        ("Seek", Ok(reply("Seek", ""))),
    ]);
    assert_eq!(script.seek().unwrap(), 100);
}

#[test]
fn unreported_clock_prevents_the_write() {
    let script = Script::new(vec![("GetPositionInfo", Ok(clock("NOT_IMPLEMENTED", 2)))]);
    assert!(
        matches!(script.seek(), Err(ProtoError::Malformed(reason))
            if reason.contains("reported playback position"))
    );
}

#[test]
fn lost_and_malformed_acknowledgements_can_complete_without_a_second_seek() {
    for answer in [Err(lost()), Ok("unreadable reply".into())] {
        let mut replies = vec![("GetPositionInfo", Ok(clock("00:01:10", 2))), ("Seek", answer)];
        replies.extend(recovery(clock("00:01:40", 2)));
        assert_eq!(Script::new(replies).seek().unwrap(), 100);
    }
}

#[test]
fn a_definite_fault_is_not_disguised_by_the_current_position() {
    let script = Script::new(vec![
        ("GetPositionInfo", Ok(clock("00:01:10", 2))),
        (
            "Seek",
            Err(ProtoError::SoapFault { code: 711, reason: "illegal seek".into() }),
        ),
    ]);
    assert!(matches!(script.seek(), Err(ProtoError::SoapFault { code: 711, .. })));
}

#[test]
fn failed_readback_keeps_the_original_write_failure_without_replay() {
    let script = Script::new(vec![
        ("GetPositionInfo", Ok(clock("00:01:10", 2))), ("Seek", Err(lost())),
        ("GetPositionInfo", Err(ProtoError::Malformed("read unavailable".into()))),
    ]);
    assert!(
        matches!(script.seek(), Err(ProtoError::Network { detail, .. })
            if detail == "seek reply lost")
    );
}

#[test]
fn a_track_changing_during_readback_is_not_confirmed() {
    let mut replies = vec![("GetPositionInfo", Ok(clock("00:01:10", 2))), ("Seek", Err(lost()))];
    replies.extend(recovery(clock("00:01:40", 3)));
    assert!(matches!(Script::new(replies).seek(), Err(ProtoError::Network { .. })));
}
