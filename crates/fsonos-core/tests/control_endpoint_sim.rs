//! A household-wide topology is not proof of who owns the addressed socket.
//! Exercise stale cached addresses and actual address reuse over S1/S2 sockets.

use fsonos_core::{CoreError, HouseholdState, control, heal};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

const OLD: &str = "x-rincon-mp3radio://example.invalid/original.mp3";
const NEW: &str = "x-rincon-mp3radio://example.invalid/requested.mp3";
const TRACKS: [&str; 2] = [
    "x-file-cifs://example.invalid/first.flac",
    "x-file-cifs://example.invalid/second.flac",
];

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    target: PlayerId,
    host: IpAddr,
    peer: IpAddr,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let builder = SimHousehold::builder();
        let builder = if generation == 1 {
            builder.s1([
                SimPlayerSpec::new("Target", SimModel::Play5Gen1),
                SimPlayerSpec::new("Peer", SimModel::Play5Gen1),
            ])
        } else {
            builder.s2([
                SimPlayerSpec::new("Target", SimModel::One),
                SimPlayerSpec::new("Peer", SimModel::One),
            ])
        };
        let sim = builder.spawn().unwrap();
        let host = sim.player("Target").unwrap().ip;
        let peer = sim.player("Peer").unwrap().ip;
        let target = PlayerId(sim.player("Target").unwrap().uuid.clone());
        let lan = sim.lan();
        for address in [host, peer] {
            for uri in TRACKS {
                soap::add_uri_to_queue(&lan, address, uri, "", false).unwrap();
            }
            soap::set_av_transport_uri(&lan, address, OLD, "").unwrap();
            soap::play(&lan, address).unwrap();
        }
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        Self {
            sim,
            houses: vec![house],
            target,
            host,
            peer,
        }
    }

    fn stale(&self) -> Vec<HouseholdState> {
        let mut houses = self.houses.clone();
        houses[0]
            .players
            .iter_mut()
            .find(|player| player.id == self.target)
            .unwrap()
            .ip = self.peer;
        houses
    }
}

#[derive(Clone, Copy, Debug)]
enum Command {
    Source,
    Queue,
    OrderedQueue,
    Resume,
    Pause,
    LoadSource,
    LoadQueue,
    LoadOrderedQueue,
}

const COMMANDS: [Command; 8] = [
    Command::Source,
    Command::Queue,
    Command::OrderedQueue,
    Command::Resume,
    Command::Pause,
    Command::LoadSource,
    Command::LoadQueue,
    Command::LoadOrderedQueue,
];

impl Command {
    fn run(
        self,
        transport: &impl Transport,
        houses: &[HouseholdState],
        target: &PlayerId,
    ) -> Result<(), CoreError> {
        match self {
            Self::Source => control::play_uri(transport, houses, target, NEW, ""),
            Self::Queue => control::play_queue_from(transport, houses, target, 2),
            Self::OrderedQueue => control::play_queue_from_in_order(transport, houses, target, 2),
            Self::Resume => control::resume(transport, houses, target),
            Self::Pause => control::pause_confirmed(transport, houses, target),
            Self::LoadSource => control::load_uri(transport, houses, target, NEW, ""),
            Self::LoadQueue => control::load_queue_from(transport, houses, target, 2),
            Self::LoadOrderedQueue => {
                control::load_queue_from_in_order(transport, houses, target, 2)
            }
        }
    }

    fn plays(self) -> bool {
        matches!(
            self,
            Self::Source | Self::Queue | Self::OrderedQueue | Self::Resume
        )
    }
}

fn is_write(action: &str) -> bool {
    !action.starts_with("Get") && action != "Browse"
}

fn queue(lan: &SimLan, host: IpAddr) -> Vec<String> {
    browse_all(lan, host, QUEUE)
        .unwrap()
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

#[test]
fn stale_addresses_refuse_all_start_and_load_writes_even_with_the_target_in_topology() {
    for generation in [1, 2] {
        for grouped in [false, true] {
            for command in COMMANDS {
                let mut f = Fixture::new(generation);
                let lan = f.sim.lan();
                if grouped {
                    soap::join_group(&lan, f.peer, &f.target).unwrap();
                    f.houses[0].apply_topology(&get_zone_group_state(&lan, f.host).unwrap());
                }
                let before = f.sim.soap_log().len();
                let error = command.run(&lan, &f.stale(), &f.target).unwrap_err();
                assert!(
                    matches!(error, CoreError::TargetChanged(_)),
                    "S{generation} {command:?}: {error}"
                );
                assert!(
                    f.sim.soap_log()[before..]
                        .iter()
                        .all(|entry| entry.action == "GetZoneGroupState"),
                    "S{generation} {command:?}: no write may reach the stale endpoint"
                );
                assert_eq!(soap::get_media_info(&lan, f.host).unwrap().uri, OLD);
                assert_eq!(queue(&lan, f.host), TRACKS);
                if !grouped {
                    assert_eq!(soap::get_media_info(&lan, f.peer).unwrap().uri, OLD);
                    assert_eq!(queue(&lan, f.peer), TRACKS);
                }
            }
        }
    }
}

#[test]
fn matching_endpoints_still_start_pause_and_load_in_both_generations() {
    for generation in [1, 2] {
        for command in COMMANDS {
            let f = Fixture::new(generation);
            let lan = f.sim.lan();
            let before = f.sim.soap_log().len();
            command.run(&lan, &f.houses, &f.target).unwrap();
            assert!(
                f.sim.soap_log()[before..]
                    .iter()
                    .filter(|entry| is_write(&entry.action))
                    .all(|entry| entry.room == "Target")
            );
            let state = soap::get_transport_info(&lan, f.host).unwrap().state;
            assert_eq!(state == TransportState::Playing, command.plays());
            let expected = match command {
                Command::Source | Command::LoadSource => NEW.to_string(),
                Command::Resume | Command::Pause => OLD.to_string(),
                _ => format!("x-rincon-queue:{}#0", f.target.0),
            };
            assert_eq!(soap::get_media_info(&lan, f.host).unwrap().uri, expected);
            assert_eq!(soap::get_media_info(&lan, f.peer).unwrap().uri, OLD);
            assert_eq!(queue(&lan, f.host), TRACKS);
        }
    }
}

#[test]
fn a_local_refusal_can_readdress_the_same_player_without_writing_to_its_peer() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let lan = f.sim.lan();
        let mut stale = f.stale();
        let before = f.sim.soap_log().len();
        let (_, recovery) =
            heal::readdressing(&lan, &[f.host, f.peer], &mut stale, &f.target, |houses| {
                control::play_uri(&lan, houses, &f.target, NEW, "")
            })
            .unwrap();
        assert_eq!(
            recovery,
            heal::Recovery::Readdressed {
                player: f.target.clone(),
                from: f.peer,
                to: f.host,
            }
        );
        let writes: Vec<_> = f.sim.soap_log()[before..]
            .iter()
            .filter(|entry| is_write(&entry.action))
            .map(|entry| (entry.room.clone(), entry.action.clone()))
            .collect();
        assert_eq!(
            writes,
            [
                ("Target".into(), "SetAVTransportURI".into()),
                ("Target".into(), "Play".into())
            ]
        );
        assert_eq!(soap::get_media_info(&lan, f.peer).unwrap().uri, OLD);
    }
}

#[test]
fn a_local_refusal_follows_the_original_rooms_new_coordinator_once() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation);
        let lan = f.sim.lan();
        let peer = PlayerId(f.sim.player("Peer").unwrap().uuid.clone());
        soap::join_group(&lan, f.host, &peer).unwrap();
        let before = f.sim.soap_log().len();
        let (_, recovery) =
            heal::on_coordinator(&lan, &mut f.houses, &f.target, |houses, coordinator| {
                control::play_uri(&lan, houses, coordinator, NEW, "")
            })
            .unwrap();
        assert_eq!(
            recovery,
            heal::Recovery::CoordinatorMoved {
                from: f.target.clone(),
                to: peer.clone(),
            }
        );
        let writes: Vec<_> = f.sim.soap_log()[before..]
            .iter()
            .filter(|entry| is_write(&entry.action))
            .map(|entry| (entry.room.clone(), entry.action.clone()))
            .collect();
        assert_eq!(
            writes,
            [
                ("Peer".into(), "SetAVTransportURI".into()),
                ("Peer".into(), "Play".into())
            ]
        );
        assert_eq!(f.houses[0].coordinator_of(&f.target), Some(&peer));
    }
}

/// Accept a real write, then let another real player acquire the old address
/// before returning a lost reply. The old target still appears in topology.
struct ReusedAddress {
    sim: RefCell<SimHandle>,
    inner: SimLan,
    stage: &'static str,
    acknowledge: bool,
    moved: Cell<bool>,
    writes: RefCell<Vec<String>>,
}

impl ReusedAddress {
    fn new(sim: SimHandle, stage: &'static str) -> Self {
        Self {
            inner: sim.lan(),
            sim: RefCell::new(sim),
            stage,
            acknowledge: false,
            moved: Cell::new(false),
            writes: RefCell::new(Vec::new()),
        }
    }

    fn acknowledged(mut self) -> Self {
        self.acknowledge = true;
        self
    }
}

impl Transport for ReusedAddress {
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
        if is_write(name) {
            self.writes.borrow_mut().push(name.into());
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if name == self.stage && !self.moved.replace(true) {
            let IpAddr::V4(old) = host else {
                panic!("synthetic players use IPv4");
            };
            let mut sim = self.sim.borrow_mut();
            sim.change_address("Target").unwrap();
            sim.move_to("Peer", old, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .unwrap();
            if !self.acknowledge {
                return Err(ProtoError::Network {
                    target: host.to_string(),
                    detail: "injected lost reply after address reuse".into(),
                });
            }
        }
        Ok(response)
    }
}

#[test]
fn reused_endpoints_cannot_confirm_lost_source_or_play_replies_with_matching_state() {
    for generation in [1, 2] {
        for stage in ["SetAVTransportURI", "Play"] {
            let f = Fixture::new(generation);
            let lan = f.sim.lan();
            soap::set_av_transport_uri(&lan, f.peer, NEW, "").unwrap();
            soap::play(&lan, f.peer).unwrap();
            let t = ReusedAddress::new(f.sim, stage);
            let error = control::play_uri(&t, &f.houses, &f.target, NEW, "").unwrap_err();
            assert!(t.moved.get());
            assert!(
                error
                    .to_string()
                    .contains("injected lost reply after address reuse")
            );
            if stage == "Play" {
                assert!(matches!(error, CoreError::Uncertain(_)));
                assert_eq!(*t.writes.borrow(), ["SetAVTransportURI", "Play"]);
            } else {
                assert!(matches!(
                    error,
                    CoreError::Proto(ProtoError::Network { .. })
                ));
                assert_eq!(*t.writes.borrow(), ["SetAVTransportURI"]);
            }
        }
    }
}

#[test]
fn a_foreign_endpoint_cannot_advance_a_lost_queue_selection_to_mode_seek_or_play() {
    for generation in [1, 2] {
        for ordered in [false, true] {
            let f = Fixture::new(generation);
            let own_queue = format!("x-rincon-queue:{}#0", f.target.0);
            soap::set_av_transport_uri(&f.sim.lan(), f.peer, &own_queue, "").unwrap();
            let t = ReusedAddress::new(f.sim, "SetAVTransportURI");
            let command = if ordered {
                Command::OrderedQueue
            } else {
                Command::Queue
            };
            let error = command.run(&t, &f.houses, &f.target).unwrap_err();
            assert!(t.moved.get());
            assert!(matches!(
                error,
                CoreError::Proto(ProtoError::Network { .. })
            ));
            assert_eq!(*t.writes.borrow(), ["SetAVTransportURI"]);
        }
    }
}

#[test]
fn a_foreign_playing_endpoint_cannot_confirm_resume() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        soap::pause(&f.sim.lan(), f.host).unwrap();
        let t = ReusedAddress::new(f.sim, "Play");
        let error = control::resume(&t, &f.houses, &f.target).unwrap_err();
        assert!(t.moved.get());
        assert!(matches!(
            error,
            CoreError::Proto(ProtoError::Network { .. })
        ));
        assert_eq!(*t.writes.borrow(), ["Play"]);
    }
}

#[test]
fn an_acknowledged_pause_losing_ownership_is_uncertain_and_cannot_authorize_retry() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let t = ReusedAddress::new(f.sim, "Pause").acknowledged();
        let error = control::pause_confirmed(&t, &f.houses, &f.target).unwrap_err();
        assert!(t.moved.get());
        assert!(matches!(error, CoreError::Uncertain(_)));
        assert_eq!(*t.writes.borrow(), ["Pause"]);
    }
}

#[derive(Clone, Copy, Debug)]
enum TransportCommand {
    Pause,
    Stop,
    Next,
    Previous,
    AbsoluteSeek,
    RelativeSeek,
}

const TRANSPORT_COMMANDS: [TransportCommand; 6] = [
    TransportCommand::Pause,
    TransportCommand::Stop,
    TransportCommand::Next,
    TransportCommand::Previous,
    TransportCommand::AbsoluteSeek,
    TransportCommand::RelativeSeek,
];

impl TransportCommand {
    fn run(
        self,
        transport: &impl Transport,
        houses: &[HouseholdState],
        target: &PlayerId,
    ) -> Result<(), CoreError> {
        match self {
            Self::Pause => control::pause(transport, houses, target),
            Self::Stop => control::stop(transport, houses, target),
            Self::Next => control::next(transport, houses, target),
            Self::Previous => control::previous(transport, houses, target),
            Self::AbsoluteSeek => {
                control::seek(transport, houses, target, control::Seek::At(60)).map(|_| ())
            }
            Self::RelativeSeek => {
                control::seek(transport, houses, target, control::Seek::By(15)).map(|_| ())
            }
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Pause => "Pause",
            Self::Stop => "Stop",
            Self::Next => "Next",
            Self::Previous => "Previous",
            Self::AbsoluteSeek | Self::RelativeSeek => "Seek",
        }
    }

    fn prepare(self, fixture: &Fixture) {
        let lan = fixture.sim.lan();
        let peer = PlayerId(fixture.sim.player("Peer").unwrap().uuid.clone());
        for (id, host) in [(&fixture.target, fixture.host), (&peer, fixture.peer)] {
            let selected = if matches!(self, Self::Previous) { 2 } else { 1 };
            control::play_queue_from(&lan, &fixture.houses, id, selected).unwrap();
            soap::seek_position(&lan, host, 30).unwrap();
            if !matches!(self, Self::Pause) {
                soap::pause(&lan, host).unwrap();
            }
        }
    }
}

#[test]
fn ordinary_transport_controls_never_mutate_a_reused_cached_address() {
    for generation in [1, 2] {
        for command in TRANSPORT_COMMANDS {
            let f = Fixture::new(generation);
            command.prepare(&f);
            let before = f.sim.soap_log().len();
            let error = command
                .run(&f.sim.lan(), &f.stale(), &f.target)
                .unwrap_err();
            assert!(
                matches!(error, CoreError::TargetChanged(_)),
                "S{generation} {command:?}: {error}"
            );
            assert!(
                f.sim.soap_log()[before..]
                    .iter()
                    .all(|entry| !is_write(&entry.action)),
                "S{generation} {command:?}: a stale address must receive no mutation"
            );
        }
    }
}

#[test]
fn ordinary_transport_controls_still_change_the_verified_group_once() {
    for generation in [1, 2] {
        for command in TRANSPORT_COMMANDS {
            let f = Fixture::new(generation);
            command.prepare(&f);
            let lan = f.sim.lan();
            let peer = soap::get_position_info(&lan, f.peer).unwrap();
            let before = f.sim.soap_log().len();
            command.run(&lan, &f.houses, &f.target).unwrap();
            let writes: Vec<_> = f.sim.soap_log()[before..]
                .iter()
                .filter(|entry| is_write(&entry.action))
                .map(|entry| (entry.room.clone(), entry.action.clone()))
                .collect();
            assert_eq!(
                writes,
                [("Target".to_string(), command.action().to_string())]
            );
            assert_eq!(soap::get_position_info(&lan, f.peer).unwrap(), peer);
            let state = soap::get_transport_info(&lan, f.host).unwrap().state;
            assert_eq!(
                state,
                if matches!(command, TransportCommand::Stop) {
                    TransportState::Stopped
                } else {
                    TransportState::Paused
                }
            );
            let position = soap::get_position_info(&lan, f.host).unwrap();
            match command {
                TransportCommand::Next => assert_eq!(position.track, 2),
                TransportCommand::Previous => assert_eq!(position.track, 1),
                TransportCommand::AbsoluteSeek => assert_eq!(position.position_secs, Some(60)),
                TransportCommand::RelativeSeek => assert_eq!(position.position_secs, Some(45)),
                TransportCommand::Pause | TransportCommand::Stop => {}
            }
        }
    }
}

#[test]
fn navigation_and_seek_recheck_ownership_after_preparatory_reads() {
    for generation in [1, 2] {
        for command in [
            TransportCommand::Next,
            TransportCommand::Previous,
            TransportCommand::AbsoluteSeek,
            TransportCommand::RelativeSeek,
        ] {
            let f = Fixture::new(generation);
            command.prepare(&f);
            let t = ReusedAddress::new(f.sim, "GetPositionInfo").acknowledged();
            let error = command.run(&t, &f.houses, &f.target).unwrap_err();
            assert!(t.moved.get(), "S{generation} {command:?}");
            assert!(
                matches!(error, CoreError::TargetChanged(_)),
                "S{generation} {command:?}: {error}"
            );
            assert!(
                t.writes.borrow().is_empty(),
                "the optional observation cannot authorize a later foreign endpoint"
            );
        }
    }
}

#[test]
fn ordinary_pause_and_stop_do_not_confirm_silence_on_a_foreign_endpoint() {
    for generation in [1, 2] {
        for command in [TransportCommand::Pause, TransportCommand::Stop] {
            let f = Fixture::new(generation);
            let lan = f.sim.lan();
            match command {
                TransportCommand::Pause => soap::pause(&lan, f.peer).unwrap(),
                TransportCommand::Stop => soap::stop(&lan, f.peer).unwrap(),
                _ => unreachable!(),
            }
            let t = ReusedAddress::new(f.sim, command.action());
            let error = command.run(&t, &f.houses, &f.target).unwrap_err();
            assert!(t.moved.get());
            assert!(matches!(
                error,
                CoreError::Proto(ProtoError::Network { detail, .. })
                    if detail == "injected lost reply after address reuse"
            ));
            assert_eq!(*t.writes.borrow(), [command.action()]);
        }
    }
}
