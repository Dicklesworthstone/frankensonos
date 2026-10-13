//! Real-socket regressions for completing a queue start after an ambiguous
//! response. Every write is sent once; unconfirmed state blocks later stages.

use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

const TRACKS: [&str; 2] = [
    "x-file-cifs://example.invalid/owner-a.flac",
    "x-file-cifs://example.invalid/owner-b.flac",
];
const OTHER: &str = "x-file-cifs://example.invalid/another-track.flac";
const WRITES: [&str; 3] = ["SetAVTransportURI", "Seek", "Play"];

#[derive(Clone, Copy)]
enum Fault {
    None,
    Lost,
    Malformed,
    Before,
    Rejected,
    Unreadable,
    Takeover,
    StalePosition,
    QueueChanged,
}

struct Replies {
    inner: SimLan,
    fault: Fault,
    stage: Option<&'static str>,
    calls: RefCell<Vec<String>>,
    failed: Cell<bool>,
}

impl Replies {
    fn new(sim: &SimHandle, fault: Fault, stage: Option<&'static str>) -> Self {
        Self {
            inner: sim.lan(),
            fault,
            stage,
            calls: RefCell::new(Vec::new()),
            failed: Cell::new(false),
        }
    }

    fn lost(host: IpAddr) -> ProtoError {
        ProtoError::Network {
            target: host.to_string(),
            detail: "injected lost queue-start reply".into(),
        }
    }

    fn writes(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter(|name| WRITES.contains(&name.as_str()))
            .cloned()
            .collect()
    }
}

impl Transport for Replies {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap_or(action);
        self.calls.borrow_mut().push(name.into());
        if self.failed.get() && matches!(self.fault, Fault::Unreadable) {
            return Err(Self::lost(host));
        }
        let selected = WRITES.contains(&name) && self.stage.is_none_or(|stage| stage == name);
        if selected && matches!(self.fault, Fault::Before) {
            return Err(Self::lost(host));
        }
        if selected && matches!(self.fault, Fault::Rejected) {
            return Err(ProtoError::SoapFault {
                code: 501,
                reason: "injected definite rejection".into(),
            });
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if self.failed.get() && name == "GetPositionInfo" {
            if matches!(self.fault, Fault::StalePosition) {
                assert!(response.contains(TRACKS[1]));
                return Ok(response.replace(TRACKS[1], OTHER));
            }
            if matches!(self.fault, Fault::QueueChanged) {
                soap::add_uri_to_queue(&self.inner, host, OTHER, "", false)?;
            }
        }
        if !selected || matches!(self.fault, Fault::None) {
            return Ok(response);
        }
        self.failed.set(true);
        if matches!(self.fault, Fault::Malformed) {
            return Ok("<incomplete-reply>".into());
        }
        if matches!(self.fault, Fault::Takeover) {
            // An independent controller switches sources after our seek.
            soap::set_av_transport_uri(&self.inner, host, OTHER, "")?;
            soap::play(&self.inner, host)?;
        }
        Err(Self::lost(host))
    }
}

fn fixture(generation: u8) -> (SimHandle, Vec<HouseholdState>, PlayerId, IpAddr) {
    let builder = SimHousehold::builder();
    let builder = if generation == 1 {
        builder.s1([SimPlayerSpec::new("Den", SimModel::Play5Gen1)])
    } else {
        builder.s2([SimPlayerSpec::new("Den", SimModel::One)])
    };
    let sim = builder.spawn().unwrap();
    let player = sim.player("Den").unwrap();
    let host = player.ip;
    let id = PlayerId(player.uuid.clone());
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
    for uri in TRACKS {
        soap::add_uri_to_queue(&sim.lan(), host, uri, "", false).unwrap();
    }
    (sim, vec![house], id, host)
}

fn queue(sim: &SimHandle, host: IpAddr) -> Vec<String> {
    browse_all(&sim.lan(), host, QUEUE)
        .unwrap()
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

#[test]
fn lost_and_malformed_replies_at_every_stage_complete_on_both_generations() {
    for generation in [1, 2] {
        for fault in [Fault::Lost, Fault::Malformed] {
            let (sim, houses, id, host) = fixture(generation);
            let replies = Replies::new(&sim, fault, None);
            control::play_queue_from(&replies, &houses, &id, 2).unwrap();
            assert_eq!(replies.writes(), WRITES, "each stage is sent once");
            assert_eq!(queue(&sim, host), TRACKS, "no enqueue, clear or rollback");
            let actual = control::playback(&sim.lan(), &houses, &id).unwrap();
            assert_eq!(actual.transport.state, TransportState::Playing);
            assert_eq!(actual.position.track, 2);
            assert_eq!(actual.position.uri, TRACKS[1]);
        }
    }
}

#[test]
fn successful_replies_keep_the_three_command_path_without_extra_reads() {
    let (sim, houses, id, _) = fixture(2);
    let replies = Replies::new(&sim, Fault::None, None);
    control::play_queue_from(&replies, &houses, &id, 2).unwrap();
    assert_eq!(*replies.calls.borrow(), WRITES);
}

#[test]
fn a_request_that_did_not_land_never_advances_to_a_later_stage() {
    for (index, stage) in WRITES.iter().enumerate() {
        let (sim, houses, id, host) = fixture(2);
        let replies = Replies::new(&sim, Fault::Before, Some(stage));
        let error = control::play_queue_from(&replies, &houses, &id, 2).unwrap_err();
        assert!(matches!(error, CoreError::Proto(ProtoError::Network { .. })));
        assert_eq!(replies.writes(), WRITES[..=index]);
        assert_eq!(queue(&sim, host), TRACKS);
        assert_ne!(
            soap::get_transport_info(&sim.lan(), host).unwrap().state,
            TransportState::Playing
        );
    }
}

#[test]
fn definite_refusals_keep_their_error_without_recovery_or_retry() {
    let (sim, houses, id, _) = fixture(2);
    let replies = Replies::new(&sim, Fault::Rejected, Some("Play"));
    let error = control::play_queue_from(&replies, &houses, &id, 2).unwrap_err();
    assert!(matches!(
        error,
        CoreError::Proto(ProtoError::SoapFault { code: 501, .. })
    ));
    assert_eq!(*replies.calls.borrow(), WRITES, "no recovery reads for a refusal");
}

#[test]
fn failed_readback_keeps_the_original_error_and_sends_no_play() {
    let (sim, houses, id, host) = fixture(2);
    let replies = Replies::new(&sim, Fault::Unreadable, Some("Seek"));
    let error = control::play_queue_from(&replies, &houses, &id, 2).unwrap_err();
    assert!(error.to_string().contains("injected lost queue-start reply"));
    assert_eq!(replies.writes(), WRITES[..2]);
    assert_eq!(soap::get_position_info(&sim.lan(), host).unwrap().track, 2);
    assert_ne!(
        soap::get_transport_info(&sim.lan(), host).unwrap().state,
        TransportState::Playing
    );
}

#[test]
fn an_owner_source_change_is_not_overwritten_during_recovery() {
    let (sim, houses, id, host) = fixture(2);
    let replies = Replies::new(&sim, Fault::Takeover, Some("Seek"));
    assert!(control::play_queue_from(&replies, &houses, &id, 2).is_err());
    assert_eq!(replies.writes(), WRITES[..2], "no subsequent Play or reset");
    assert_eq!(soap::get_media_info(&sim.lan(), host).unwrap().uri, OTHER);
    assert_eq!(
        soap::get_transport_info(&sim.lan(), host).unwrap().state,
        TransportState::Playing
    );
    assert_eq!(queue(&sim, host), TRACKS);
}

#[test]
fn a_matching_track_number_with_different_media_is_not_confirmation() {
    let (sim, houses, id, host) = fixture(2);
    let replies = Replies::new(&sim, Fault::StalePosition, Some("Play"));
    assert!(control::play_queue_from(&replies, &houses, &id, 2).is_err());
    assert_eq!(replies.writes(), WRITES, "no replay or rollback on mismatch");
    assert_eq!(soap::get_position_info(&sim.lan(), host).unwrap().uri, TRACKS[1]);
}

#[test]
fn a_queue_changed_during_readback_cannot_confirm_a_start() {
    let (sim, houses, id, host) = fixture(2);
    let replies = Replies::new(&sim, Fault::QueueChanged, Some("Play"));
    assert!(control::play_queue_from(&replies, &houses, &id, 2).is_err());
    assert_eq!(replies.writes(), WRITES);
    assert_eq!(queue(&sim, host), [TRACKS[0], TRACKS[1], OTHER]);
}

#[test]
fn zero_position_is_rejected_before_changing_the_source() {
    let (sim, houses, id, _) = fixture(2);
    let replies = Replies::new(&sim, Fault::None, None);
    assert!(control::play_queue_from(&replies, &houses, &id, 0).is_err());
    assert!(replies.calls.borrow().is_empty());
}
