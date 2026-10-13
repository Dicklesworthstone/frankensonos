//! Ordered queue starts against real S1/S2 sockets. The transport adds the
//! queue-active requirement some players impose on SetPlayMode, and injects
//! individual failed replies without replacing the simulator's state machine.

use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::RefCell;
use std::net::IpAddr;

const FIRST: &str = "x-file-cifs://example.invalid/first.flac";
const SECOND: &str = "x-file-cifs://example.invalid/second.flac";
const STREAM: &str = "x-rincon-mp3radio://example.invalid/radio.mp3";
const ORDERED: [&str; 4] = ["SetAVTransportURI", "SetPlayMode", "Seek", "Play"];

#[derive(Clone, Copy)]
enum Fault {
    None,
    RejectedMode,
    LostMode,
    MalformedMode,
    LostSource,
    LostPlay,
}

struct StrictMode {
    inner: SimLan,
    queue: String,
    fault: Fault,
    writes: RefCell<Vec<String>>,
}

impl Transport for StrictMode {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action
            .trim_matches('"')
            .rsplit('#')
            .next()
            .unwrap_or(action);
        if ORDERED.contains(&name) {
            self.writes.borrow_mut().push(name.into());
        }
        if name == "SetPlayMode" {
            if soap::get_media_info(&self.inner, host)?.uri != self.queue {
                return Err(ProtoError::SoapFault {
                    code: 712,
                    reason: "queue must be active before setting its play mode".into(),
                });
            }
            if matches!(self.fault, Fault::RejectedMode) {
                return Err(ProtoError::SoapFault {
                    code: 501,
                    reason: "injected mode refusal".into(),
                });
            }
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if name == "SetPlayMode" && matches!(self.fault, Fault::MalformedMode) {
            return Ok("<broken-mode-response>".into());
        }
        if matches!(
            (name, self.fault),
            ("SetPlayMode", Fault::LostMode)
                | ("SetAVTransportURI", Fault::LostSource)
                | ("Play", Fault::LostPlay)
        ) {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: format!("injected lost {name} reply"),
            });
        }
        Ok(response)
    }
}

struct Rig {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    id: PlayerId,
    host: IpAddr,
}

impl Rig {
    fn new(generation: u8, stream: bool) -> Self {
        let builder = SimHousehold::builder();
        let builder = if generation == 1 {
            builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)])
        } else {
            builder.s2([SimPlayerSpec::new("Room", SimModel::One)])
        };
        let sim = builder.spawn().unwrap();
        let player = sim.player("Room").unwrap();
        let host = player.ip;
        let id = PlayerId(player.uuid.clone());
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        for uri in [FIRST, SECOND] {
            soap::add_uri_to_queue(&lan, host, uri, "", false).unwrap();
        }
        if stream {
            soap::set_av_transport_uri(&lan, host, STREAM, "").unwrap();
            soap::play(&lan, host).unwrap();
        }
        Self {
            sim,
            houses: vec![house],
            id,
            host,
        }
    }

    fn transport(&self, fault: Fault) -> StrictMode {
        StrictMode {
            inner: self.sim.lan(),
            queue: format!("x-rincon-queue:{}#0", self.id.0),
            fault,
            writes: RefCell::default(),
        }
    }

    fn start(&self, t: &StrictMode) -> Result<(), CoreError> {
        control::play_queue_from_in_order(t, &self.houses, &self.id, 2)
    }

    fn assert_playing_second(&self) {
        let lan = self.sim.lan();
        let transport = soap::get_transport_info(&lan, self.host).unwrap();
        assert_eq!(transport.state, TransportState::Playing);
        let now = soap::get_position_info(&lan, self.host).unwrap();
        assert_eq!(now.track, 2);
        assert_eq!(now.uri, SECOND);
        let queue = browse_all(&lan, self.host, QUEUE).unwrap();
        let uris: Vec<_> = queue
            .iter()
            .map(|item| item.res.as_ref().unwrap().uri.as_str())
            .collect();
        assert_eq!(uris, [FIRST, SECOND]);
    }
}

#[test]
fn ordered_start_activates_the_queue_before_mode_and_seeks_before_playing() {
    for generation in [1, 2] {
        for stream in [false, true] {
            let rig = Rig::new(generation, stream);
            let t = rig.transport(Fault::None);
            rig.start(&t).unwrap();
            assert_eq!(*t.writes.borrow(), ORDERED);
            rig.assert_playing_second();
        }
    }
}

#[test]
fn ordinary_queue_starts_do_not_change_shuffle_or_repeat() {
    for generation in [1, 2] {
        let rig = Rig::new(generation, true);
        let t = rig.transport(Fault::None);
        control::play_queue_from(&t, &rig.houses, &rig.id, 2).unwrap();
        assert_eq!(*t.writes.borrow(), ["SetAVTransportURI", "Seek", "Play"]);
        rig.assert_playing_second();
    }
}

#[test]
fn a_failed_mode_change_never_advances_to_seek_or_play() {
    for generation in [1, 2] {
        for fault in [Fault::RejectedMode, Fault::LostMode, Fault::MalformedMode] {
            let rig = Rig::new(generation, true);
            let t = rig.transport(fault);
            assert!(rig.start(&t).is_err());
            assert_eq!(*t.writes.borrow(), ["SetAVTransportURI", "SetPlayMode"]);
            let lan = rig.sim.lan();
            let transport = soap::get_transport_info(&lan, rig.host).unwrap();
            assert_eq!(transport.state, TransportState::Stopped);
            assert_eq!(soap::get_position_info(&lan, rig.host).unwrap().track, 1);
            assert_eq!(browse_all(&lan, rig.host, QUEUE).unwrap().len(), 2);
        }
    }
}

#[test]
fn lost_source_and_play_replies_keep_the_mode_stage_and_never_repeat_writes() {
    for generation in [1, 2] {
        for fault in [Fault::LostSource, Fault::LostPlay] {
            let rig = Rig::new(generation, true);
            let t = rig.transport(fault);
            rig.start(&t).unwrap();
            assert_eq!(*t.writes.borrow(), ORDERED);
            rig.assert_playing_second();
        }
    }
}

#[test]
fn an_invalid_ordered_start_changes_nothing() {
    let rig = Rig::new(2, true);
    let t = rig.transport(Fault::None);
    assert!(control::play_queue_from_in_order(&t, &rig.houses, &rig.id, 0).is_err());
    assert!(t.writes.borrow().is_empty());
    let media = soap::get_media_info(&rig.sim.lan(), rig.host).unwrap();
    assert_eq!(media.uri, STREAM);
}
