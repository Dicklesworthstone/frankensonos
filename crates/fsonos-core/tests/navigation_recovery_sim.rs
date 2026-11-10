//! Next/Previous are sent once to real S1/S2 simulator sockets. Discarding an
//! accepted reply must not turn a successful single step into another skip.

use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::content::{self, QUEUE};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::set_play_mode;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;
use std::time::Duration;

const TRACKS: [&str; 4] = [
    "x-file-cifs://example.invalid/first.flac",
    "x-file-cifs://example.invalid/repeated.flac",
    "x-file-cifs://example.invalid/repeated.flac",
    "x-file-cifs://example.invalid/last.flac",
];

struct Rig {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    id: PlayerId,
    host: IpAddr,
    peer: IpAddr,
}

impl Rig {
    fn new(s2: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::new("Target", model),
            SimPlayerSpec::new("Peer", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(players)
        } else {
            builder.s1(players)
        }
        .spawn()
        .unwrap();
        let host = sim.player("Target").unwrap().ip;
        let id = PlayerId(sim.player("Target").unwrap().uuid.clone());
        let peer = sim.player("Peer").unwrap().ip;
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        Self {
            sim,
            lan,
            houses: vec![house],
            id,
            host,
            peer,
        }
    }

    fn prepare(&self, at: u32, state: TransportState) {
        soap::remove_all_tracks_from_queue(&self.lan, self.host).unwrap();
        for uri in TRACKS {
            soap::add_uri_to_queue(&self.lan, self.host, uri, "", false).unwrap();
        }
        set_play_mode(&self.lan, self.host, PlayMode::Normal).unwrap();
        control::play_queue_from(&self.lan, &self.houses, &self.id, at).unwrap();
        soap::seek_position(&self.lan, self.host, 70).unwrap();
        match state {
            TransportState::Paused => soap::pause(&self.lan, self.host).unwrap(),
            TransportState::Stopped => soap::stop(&self.lan, self.host).unwrap(),
            TransportState::Playing => {}
            other => panic!("unexpected fixture state: {other:?}"),
        }
    }

    fn step(&self, t: &impl Transport, forward: bool) -> Result<(), CoreError> {
        if forward {
            control::next(t, &self.houses, &self.id)
        } else {
            control::previous(t, &self.houses, &self.id)
        }
    }
}

#[derive(Clone, Copy)]
enum Reply {
    Accepted,
    Lost,
    Malformed,
    Unapplied,
    Refused(u16),
}

#[derive(Clone, Copy, Debug)]
enum Change {
    None,
    AnotherOccurrence,
    OriginalOccurrence,
    Queue,
    Source,
    Mode,
    Members,
    Reboot,
    Position,
    State,
}

struct Replies<'a> {
    rig: &'a Rig,
    forward: bool,
    reply: Reply,
    change: Change,
    sent: Cell<bool>,
    fail_before: bool,
    fail_after: bool,
    missing_boot: bool,
    calls: RefCell<Vec<String>>,
}

impl<'a> Replies<'a> {
    fn new(rig: &'a Rig, forward: bool, reply: Reply) -> Self {
        Self {
            rig,
            forward,
            reply,
            change: Change::None,
            sent: Cell::new(false),
            fail_before: false,
            fail_after: false,
            missing_boot: false,
            calls: RefCell::new(Vec::new()),
        }
    }

    fn lost() -> ProtoError {
        ProtoError::Network {
            target: "simulator".into(),
            detail: "original navigation reply lost".into(),
        }
    }

    fn mutate(&self) {
        let rig = self.rig;
        match self.change {
            Change::None => {}
            Change::AnotherOccurrence => {
                // The URI can be identical; only its queue position differs.
                soap::seek_track(&rig.lan, rig.host, if self.forward { 3 } else { 2 }).unwrap();
            }
            Change::OriginalOccurrence => {
                soap::seek_track(&rig.lan, rig.host, if self.forward { 1 } else { 4 }).unwrap();
            }
            Change::Queue => {
                soap::add_uri_to_queue(
                    &rig.lan,
                    rig.host,
                    "x-file-cifs://example.invalid/owner.flac",
                    "",
                    false,
                )
                .unwrap();
            }
            Change::Source => {
                let source = soap::get_media_info(&rig.lan, rig.host).unwrap().uri;
                soap::set_av_transport_uri(&rig.lan, rig.host, &source, "owner selection metadata")
                    .unwrap();
                soap::seek_track(&rig.lan, rig.host, if self.forward { 2 } else { 3 }).unwrap();
            }
            Change::Mode => set_play_mode(&rig.lan, rig.host, PlayMode::ShuffleNoRepeat).unwrap(),
            Change::Members => soap::join_group(&rig.lan, rig.peer, &rig.id).unwrap(),
            Change::Reboot => rig.sim.reboot("Target", Duration::ZERO).unwrap(),
            Change::Position => soap::seek_position(&rig.lan, rig.host, 60).unwrap(),
            Change::State => soap::pause(&rig.lan, rig.host).unwrap(),
        }
    }

    fn one_step_only(&self) {
        let calls = self.calls.borrow();
        let action = if self.forward { "Next" } else { "Previous" };
        let writes: Vec<_> = calls
            .iter()
            .filter(|name| !name.starts_with("Get") && *name != "Browse")
            .map(String::as_str)
            .collect();
        assert_eq!(writes, [action]);
        assert!(
            calls.len() <= 15,
            "bounded two-entry observation only: {calls:?}"
        );
    }
}

impl Transport for Replies<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let action = header.trim_matches('"').rsplit('#').next().unwrap();
        self.calls.borrow_mut().push(action.into());
        let navigation = action == if self.forward { "Next" } else { "Previous" };
        if !navigation {
            if (!self.sent.get() && self.fail_before) || (self.sent.get() && self.fail_after) {
                return Err(ProtoError::Network {
                    target: "simulator read".into(),
                    detail: "optional navigation observation unavailable".into(),
                });
            }
            let result = self.rig.lan.soap_post(host, path, header, body)?;
            return Ok(if self.missing_boot && action == "GetZoneGroupState" {
                result.replace("BootSeq=", "UnknownBoot=")
            } else {
                result
            });
        }
        assert!(
            !self.sent.replace(true),
            "navigation must never be repeated"
        );
        if matches!(self.reply, Reply::Unapplied) {
            return Err(Self::lost());
        }
        // Even the explicit-refusal fixture moves to the matching occurrence:
        // another controller doing that must not override a definite refusal.
        let result = self.rig.lan.soap_post(host, path, header, body)?;
        fsonos_proto::soap::parse_response(&result, action)?;
        self.mutate();
        match self.reply {
            Reply::Accepted => Ok(result),
            Reply::Malformed => Ok("an incomplete navigation acknowledgement".into()),
            Reply::Refused(code) => Err(ProtoError::SoapFault {
                code,
                reason: "navigation definitely refused".into(),
            }),
            Reply::Lost | Reply::Unapplied => Err(Self::lost()),
        }
    }
}

fn original_network(error: &CoreError) {
    assert!(
        matches!(
            error,
            CoreError::Proto(ProtoError::Network { detail, .. })
                if detail == "original navigation reply lost"
        ),
        "{error}"
    );
}

#[test]
fn accepted_lost_and_malformed_replies_confirm_one_exact_adjacent_occurrence() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        for forward in [true, false] {
            for state in [
                TransportState::Playing,
                TransportState::Paused,
                TransportState::Stopped,
            ] {
                for reply in [Reply::Lost, Reply::Malformed] {
                    // The adjacent middle slots deliberately share a URI.
                    rig.prepare(if forward { 2 } else { 3 }, state);
                    let queue = content::browse_snapshot(&rig.lan, rig.host, QUEUE).unwrap();
                    let t = Replies::new(&rig, forward, reply);
                    rig.step(&t, forward).unwrap();
                    t.one_step_only();
                    assert_eq!(t.calls.borrow().len(), 15);
                    let position = soap::get_position_info(&rig.lan, rig.host).unwrap();
                    assert_eq!(position.track, if forward { 3 } else { 2 });
                    assert_eq!(position.position_secs, Some(0));
                    assert_eq!(position.uri, TRACKS[1]);
                    assert_eq!(
                        soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
                        state
                    );
                    assert_eq!(
                        content::browse_snapshot(&rig.lan, rig.host, QUEUE).unwrap(),
                        queue
                    );
                }
            }
        }
    }
}

#[test]
fn acknowledged_steps_return_without_readback_even_when_optional_reads_fail() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        for forward in [true, false] {
            for fail_before in [false, true] {
                rig.prepare(if forward { 1 } else { 4 }, TransportState::Playing);
                let mut t = Replies::new(&rig, forward, Reply::Accepted);
                t.fail_before = fail_before;
                rig.step(&t, forward).unwrap();
                t.one_step_only();
                assert_eq!(t.calls.borrow().len(), if fail_before { 2 } else { 8 });
                assert_eq!(
                    t.calls.borrow().last().map(String::as_str),
                    Some(if forward { "Next" } else { "Previous" })
                );
                assert_eq!(
                    soap::get_position_info(&rig.lan, rig.host).unwrap().track,
                    if forward { 2 } else { 3 }
                );
            }
        }
    }
}

#[test]
fn unchanged_occurrences_and_failed_evidence_preserve_the_original_lost_reply() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        for forward in [true, false] {
            for (reply, fail_before, fail_after, calls) in [
                (Reply::Unapplied, false, false, 9),
                (Reply::Lost, true, false, 2),
                (Reply::Lost, false, true, 9),
            ] {
                let start = if forward { 1 } else { 4 };
                rig.prepare(start, TransportState::Playing);
                let mut t = Replies::new(&rig, forward, reply);
                t.fail_before = fail_before;
                t.fail_after = fail_after;
                original_network(&rig.step(&t, forward).unwrap_err());
                t.one_step_only();
                assert_eq!(t.calls.borrow().len(), calls);
                assert_eq!(
                    soap::get_position_info(&rig.lan, rig.host).unwrap().track,
                    if matches!(reply, Reply::Unapplied) {
                        start
                    } else if forward {
                        2
                    } else {
                        3
                    }
                );
            }
        }
    }
}

#[test]
fn definite_faults_stand_even_when_another_controller_reaches_the_expected_slot() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        for forward in [true, false] {
            for code in [402, 701, 800] {
                rig.prepare(if forward { 1 } else { 4 }, TransportState::Playing);
                let t = Replies::new(&rig, forward, Reply::Refused(code));
                let error = rig.step(&t, forward).unwrap_err();
                assert!(
                    matches!(
                        &error,
                        CoreError::Proto(ProtoError::SoapFault { code: observed, reason })
                            if *observed == code && reason == "navigation definitely refused"
                    ),
                    "{error}"
                );
                t.one_step_only();
                assert_eq!(t.calls.borrow().len(), 8);
                assert_eq!(
                    soap::get_position_info(&rig.lan, rig.host).unwrap().track,
                    if forward { 2 } else { 3 }
                );
            }
        }
    }
}

#[test]
fn a_changed_occurrence_queue_source_mode_group_boot_clock_or_state_is_not_success() {
    for s2 in [false, true] {
        for forward in [true, false] {
            for change in [
                Change::AnotherOccurrence,
                Change::OriginalOccurrence,
                Change::Queue,
                Change::Source,
                Change::Mode,
                Change::Members,
                Change::Reboot,
                Change::Position,
                Change::State,
            ] {
                let rig = Rig::new(s2);
                rig.prepare(if forward { 1 } else { 4 }, TransportState::Playing);
                let mut t = Replies::new(&rig, forward, Reply::Lost);
                t.change = change;
                original_network(&rig.step(&t, forward).unwrap_err());
                t.one_step_only();
                assert!(t.sent.get(), "{change:?}");
            }
        }
    }
}

#[test]
fn shuffle_repeat_and_missing_boot_identity_keep_navigation_one_shot() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        for forward in [true, false] {
            for mode in PlayMode::ALL {
                if mode == PlayMode::Normal {
                    continue;
                }
                rig.prepare(if forward { 1 } else { 4 }, TransportState::Playing);
                set_play_mode(&rig.lan, rig.host, mode).unwrap();
                let t = Replies::new(&rig, forward, Reply::Lost);
                original_network(&rig.step(&t, forward).unwrap_err());
                t.one_step_only();
                assert_eq!(t.calls.borrow().len(), 3);
                assert_eq!(
                    soap::get_position_info(&rig.lan, rig.host).unwrap().track,
                    if forward { 2 } else { 3 }
                );
            }
            rig.prepare(if forward { 1 } else { 4 }, TransportState::Playing);
            let mut t = Replies::new(&rig, forward, Reply::Lost);
            t.missing_boot = true;
            original_network(&rig.step(&t, forward).unwrap_err());
            t.one_step_only();
            assert_eq!(t.calls.borrow().len(), 5);
            assert_eq!(
                soap::get_position_info(&rig.lan, rig.host).unwrap().track,
                if forward { 2 } else { 3 }
            );
        }
    }
}

#[test]
fn next_near_the_natural_end_cannot_claim_the_automatic_transition() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        rig.prepare(1, TransportState::Playing);
        soap::seek_position(&rig.lan, rig.host, 179).unwrap();
        let t = Replies::new(&rig, true, Reply::Lost);
        original_network(&rig.step(&t, true).unwrap_err());
        t.one_step_only();
        assert_eq!(t.calls.borrow().len(), 9);
        assert_eq!(
            soap::get_position_info(&rig.lan, rig.host).unwrap().track,
            2
        );
    }
}

#[test]
fn queue_boundaries_and_direct_sources_preserve_the_renderer_refusal() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        for forward in [true, false] {
            let start = if forward { 4 } else { 1 };
            rig.prepare(start, TransportState::Playing);
            let t = Replies::new(&rig, forward, Reply::Lost);
            let error = rig.step(&t, forward).unwrap_err();
            assert!(
                matches!(
                    &error,
                    CoreError::Proto(ProtoError::SoapFault { code: 711, .. })
                ),
                "{error}"
            );
            t.one_step_only();
            assert_eq!(t.calls.borrow().len(), if forward { 6 } else { 2 });
            assert_eq!(
                soap::get_position_info(&rig.lan, rig.host).unwrap().track,
                start
            );

            soap::set_av_transport_uri(&rig.lan, rig.host, TRACKS[0], "").unwrap();
            soap::play(&rig.lan, rig.host).unwrap();
            let t = Replies::new(&rig, forward, Reply::Lost);
            let error = rig.step(&t, forward).unwrap_err();
            assert!(
                matches!(
                    &error,
                    CoreError::Proto(ProtoError::SoapFault { code: 701, .. })
                ),
                "{error}"
            );
            t.one_step_only();
            assert_eq!(t.calls.borrow().len(), 2);
            assert_eq!(
                soap::get_media_info(&rig.lan, rig.host).unwrap().uri,
                TRACKS[0]
            );
        }
    }
}
