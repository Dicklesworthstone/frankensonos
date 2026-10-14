//! Lost Play replies while the renderer is transitioning. State is read
//! through real simulator sockets; only the transient reply is injected.
//! Neither queue nor direct-source recovery may replay a playback write.

use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

const FIRST: &str = "x-file-cifs://example.invalid/first.flac";
const SECOND: &str = "x-file-cifs://example.invalid/second.flac";
const STREAM: &str = "x-rincon-mp3radio://example.invalid/stream.mp3";
const OWNER: &str = "x-rincon-mp3radio://example.invalid/owner.mp3";

#[derive(Clone, Copy)]
enum DuringTransition {
    Settle,
    SourceChanged,
    QueueEdited,
    Paused,
    ReadFailed,
}

struct Transition {
    inner: SimLan,
    mode: DuringTransition,
    played: Cell<bool>,
    status_reads: Cell<u32>,
    writes: RefCell<Vec<String>>,
}

impl Transition {
    fn new(sim: &SimHandle, mode: DuringTransition) -> Self {
        Self {
            inner: sim.lan(),
            mode,
            played: Cell::new(false),
            status_reads: Cell::new(0),
            writes: RefCell::new(Vec::new()),
        }
    }
}

impl Transport for Transition {
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
        if !name.starts_with("Get") && name != "Browse" {
            self.writes.borrow_mut().push(name.into());
        }
        if self.played.get()
            && self.status_reads.get() > 0
            && matches!(self.mode, DuringTransition::ReadFailed)
        {
            return Err(ProtoError::Malformed("transition read failed".into()));
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if name == "Play" {
            assert!(!self.played.replace(true), "Play was replayed");
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected lost Play reply".into(),
            });
        }
        if name != "GetTransportInfo" || !self.played.get() {
            return Ok(response);
        }
        let read = self.status_reads.get() + 1;
        self.status_reads.set(read);
        if read == 1 {
            match self.mode {
                DuringTransition::SourceChanged => {
                    soap::set_av_transport_uri(&self.inner, host, OWNER, "")?;
                    soap::play(&self.inner, host)?;
                }
                DuringTransition::QueueEdited => {
                    soap::add_uri_to_queue(&self.inner, host, OWNER, "", false)?;
                }
                DuringTransition::Paused => soap::pause(&self.inner, host)?,
                _ => {}
            }
        }
        if read == 1 || (read == 2 && matches!(self.mode, DuringTransition::Settle)) {
            assert!(response.contains(">PLAYING<"));
            return Ok(response.replacen(">PLAYING<", ">TRANSITIONING<", 1));
        }
        Ok(response)
    }
}

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    id: PlayerId,
    host: IpAddr,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let builder = SimHousehold::builder();
        let builder = if generation == 1 {
            builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)])
        } else {
            builder.s2([SimPlayerSpec::new("Room", SimModel::One)])
        };
        let sim = builder.spawn().unwrap();
        let player = sim.player("Room").unwrap();
        let id = PlayerId(player.uuid.clone());
        let host = player.ip;
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
        for uri in [FIRST, SECOND] {
            soap::add_uri_to_queue(&sim.lan(), host, uri, "", false).unwrap();
        }
        Self {
            sim,
            houses: vec![house],
            id,
            host,
        }
    }

    fn start(&self, transport: &Transition, queue: bool) -> Result<(), CoreError> {
        if queue {
            control::play_queue_from(transport, &self.houses, &self.id, 2)
        } else {
            control::play_uri(transport, &self.houses, &self.id, STREAM, "")
        }
    }

    fn queue(&self) -> Vec<String> {
        browse_all(&self.sim.lan(), self.host, QUEUE)
            .unwrap()
            .into_iter()
            .map(|item| item.res.unwrap().uri)
            .collect()
    }

    fn state(&self) -> TransportState {
        soap::get_transport_info(&self.sim.lan(), self.host)
            .unwrap()
            .state
    }
}

fn assert_writes(transport: &Transition, queue: bool) {
    let expected: &[&str] = if queue {
        &["SetAVTransportURI", "Seek", "Play"]
    } else {
        &["SetAVTransportURI", "Play"]
    };
    assert_eq!(transport.writes.borrow().as_slice(), expected);
}

#[test]
fn transitioning_direct_and_queue_starts_settle_without_another_play_on_s1_and_s2() {
    for generation in [1, 2] {
        for queue in [false, true] {
            let f = Fixture::new(generation);
            let transport = Transition::new(&f.sim, DuringTransition::Settle);
            f.start(&transport, queue).unwrap();
            assert_writes(&transport, queue);
            assert_eq!(transport.status_reads.get(), 3);
            assert_eq!(f.state(), TransportState::Playing);
            let position = soap::get_position_info(&f.sim.lan(), f.host).unwrap();
            assert_eq!(position.uri, if queue { SECOND } else { STREAM });
            assert_eq!(f.queue(), [FIRST, SECOND]);
        }
    }
}

#[test]
fn an_owner_source_change_during_transition_is_not_overwritten_or_called_success() {
    for generation in [1, 2] {
        for queue in [false, true] {
            let f = Fixture::new(generation);
            let transport = Transition::new(&f.sim, DuringTransition::SourceChanged);
            let error = f.start(&transport, queue).unwrap_err().to_string();
            assert!(error.contains("injected lost Play reply"));
            assert_writes(&transport, queue);
            assert_eq!(transport.status_reads.get(), 1);
            let media = soap::get_media_info(&f.sim.lan(), f.host).unwrap();
            assert_eq!(media.uri, OWNER);
            assert_eq!(f.state(), TransportState::Playing);
            assert_eq!(f.queue(), [FIRST, SECOND]);
        }
    }
}

#[test]
fn a_queue_edit_during_transition_cannot_replace_the_original_revision_proof() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let transport = Transition::new(&f.sim, DuringTransition::QueueEdited);
        let error = f.start(&transport, true).unwrap_err().to_string();
        assert!(error.contains("injected lost Play reply"));
        assert_writes(&transport, true);
        assert_eq!(transport.status_reads.get(), 1);
        assert_eq!(f.queue(), [FIRST, SECOND, OWNER]);
        assert_eq!(f.state(), TransportState::Playing);
    }
}

#[test]
fn an_owner_pause_during_transition_is_not_resumed_by_recovery() {
    for generation in [1, 2] {
        for queue in [false, true] {
            let f = Fixture::new(generation);
            let transport = Transition::new(&f.sim, DuringTransition::Paused);
            assert!(f.start(&transport, queue).is_err());
            assert_writes(&transport, queue);
            assert_eq!(transport.status_reads.get(), 2);
            assert_eq!(f.state(), TransportState::Paused);
        }
    }
}

#[test]
fn failed_transition_reads_stop_recovery_and_preserve_the_original_play_error() {
    for queue in [false, true] {
        let f = Fixture::new(2);
        let transport = Transition::new(&f.sim, DuringTransition::ReadFailed);
        let error = f.start(&transport, queue).unwrap_err().to_string();
        assert!(error.contains("injected lost Play reply"));
        assert!(!error.contains("transition read failed"));
        assert_writes(&transport, queue);
        assert_eq!(transport.status_reads.get(), 1);
        assert_eq!(f.state(), TransportState::Playing);
    }
}
