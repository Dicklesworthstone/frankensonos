use super::*;
use std::cell::{Cell, RefCell};
use std::collections::VecDeque;
use std::net::Ipv4Addr;

type Step = (&'static str, Result<String, ProtoError>);

struct Script(RefCell<VecDeque<Step>>);

impl Script {
    fn new(steps: Vec<Step>) -> Self {
        Self(RefCell::new(steps.into()))
    }

    fn done(&self) {
        assert!(
            self.0.borrow().is_empty(),
            "all expected reads were performed"
        );
    }
}

impl Transport for Script {
    fn soap_post(&self, _: IpAddr, _: &str, action: &str, _: &str) -> Result<String, ProtoError> {
        let (expected, reply) = self.0.borrow_mut().pop_front().expect("no write replay");
        assert!(action.ends_with(&format!("#{expected}\"")), "{action}");
        reply
    }
}

fn host() -> IpAddr {
    Ipv4Addr::LOCALHOST.into()
}

fn player() -> PlayerId {
    PlayerId("synthetic-player".into())
}

fn reply(action: &str, args: &str) -> String {
    format!(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
         <u:{action}Response xmlns:u=\"urn:x\">{args}</u:{action}Response></s:Body></s:Envelope>"
    )
}

fn status(state: &str) -> Step {
    (
        "GetTransportInfo",
        Ok(reply(
            "GetTransportInfo",
            &format!("<CurrentTransportState>{state}</CurrentTransportState>"),
        )),
    )
}

fn topology(coordinator: &str) -> Step {
    (
        "GetZoneGroupState",
        Ok(reply(
            "GetZoneGroupState",
            &format!(
                "<ZoneGroupState>&lt;ZoneGroups&gt;\
             &lt;ZoneGroup Coordinator=\"{coordinator}\" ID=\"g\"&gt;\
             &lt;ZoneGroupMember UUID=\"{coordinator}\" ZoneName=\"Test\" \
             Location=\"http://127.0.0.1:1400/xml/device_description.xml\"/&gt;\
             &lt;/ZoneGroup&gt;&lt;/ZoneGroups&gt;</ZoneGroupState>"
            ),
        )),
    )
}

fn lost() -> ProtoError {
    ProtoError::Network {
        target: "simulator".into(),
        detail: "write reply lost".into(),
    }
}

fn refused() -> ProtoError {
    ProtoError::SoapFault {
        code: 701,
        reason: "transition unavailable".into(),
    }
}

#[test]
fn acknowledged_commands_do_not_add_reads() {
    let t = Script::new(vec![("Play", Ok(reply("Play", "")))]);
    play(&t, host(), &player()).unwrap();
    t.done();
    let t = Script::new(vec![("Pause", Ok(reply("Pause", "")))]);
    pause(&t, host()).unwrap();
    t.done();
    let t = Script::new(vec![("Stop", Ok(reply("Stop", "")))]);
    stop(&t, host()).unwrap();
    t.done();
}

#[test]
fn uncertain_pause_accepts_inactive_states_without_stop_or_replay() {
    for state in ["PAUSED_PLAYBACK", "STOPPED"] {
        let t = Script::new(vec![("Pause", Err(lost())), status(state)]);
        pause(&t, host()).unwrap();
        t.done();
    }
}

#[test]
fn uncertain_active_or_unknown_pause_is_never_escalated_to_stop() {
    for state in ["PLAYING", "TRANSITIONING", "UNKNOWN"] {
        let t = Script::new(vec![("Pause", Err(lost())), status(state)]);
        assert!(matches!(pause(&t, host()), Err(ProtoError::Network { .. })));
        t.done();
    }
}

#[test]
fn definite_701_preserves_inactive_playback_and_only_stops_active_streams() {
    for state in ["PAUSED_PLAYBACK", "STOPPED"] {
        let t = Script::new(vec![("Pause", Err(refused())), status(state)]);
        pause(&t, host()).unwrap();
        t.done();
    }
    for state in ["PLAYING", "TRANSITIONING"] {
        let t = Script::new(vec![
            ("Pause", Err(refused())),
            status(state),
            ("Stop", Ok(reply("Stop", ""))),
        ]);
        pause(&t, host()).unwrap();
        t.done();
    }
}

#[test]
fn stream_stop_with_a_lost_reply_can_confirm_silence() {
    let t = Script::new(vec![
        ("Pause", Err(refused())),
        status("PLAYING"),
        ("Stop", Err(lost())),
        status("STOPPED"),
    ]);
    pause(&t, host()).unwrap();
    t.done();
}

#[test]
fn a_stop_requires_stopped_not_merely_paused() {
    for (state, success) in [("STOPPED", true), ("PAUSED_PLAYBACK", false)] {
        let t = Script::new(vec![("Stop", Err(lost())), status(state)]);
        assert_eq!(stop(&t, host()).is_ok(), success);
        t.done();
    }
}

#[test]
fn malformed_pause_response_uses_the_same_readback_path() {
    let t = Script::new(vec![
        ("Pause", Ok("not an acknowledgement".into())),
        status("PAUSED_PLAYBACK"),
    ]);
    pause(&t, host()).unwrap();
    t.done();
}

#[test]
fn a_lost_play_reply_checks_coordinator_before_and_after_observing_playing() {
    let t = Script::new(vec![
        ("Play", Err(lost())),
        topology("synthetic-player"),
        status("PLAYING"),
        topology("synthetic-player"),
    ]);
    play(&t, host(), &player()).unwrap();
    t.done();
}

#[test]
fn changed_coordinator_and_nonplaying_states_do_not_confirm_play() {
    let t = Script::new(vec![("Play", Err(lost())), topology("another-player")]);
    assert!(play(&t, host(), &player()).is_err());
    t.done();
    for state in ["PAUSED_PLAYBACK", "STOPPED", "UNKNOWN"] {
        let t = Script::new(vec![
            ("Play", Err(lost())),
            topology("synthetic-player"),
            status(state),
        ]);
        assert!(play(&t, host(), &player()).is_err());
        t.done();
    }
}

#[test]
fn explicit_faults_skip_observation_and_read_failures_preserve_the_write_cause() {
    let reads = Cell::new(0);
    let result = complete("Play", Err(refused()), || {
        reads.set(reads.get() + 1);
        Ok(true)
    });
    assert!(matches!(
        result,
        Err(ProtoError::SoapFault { code: 701, .. })
    ));
    assert_eq!(reads.get(), 0);
    let result = complete("Play", Err(lost()), || {
        Err(ProtoError::Malformed("read-back failed".into()))
    });
    assert!(matches!(result, Err(ProtoError::Network { detail, .. })
            if detail == "write reply lost"));
}
