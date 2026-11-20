//! Room tone follows the physical player across grouping and DHCP changes.
//! Topology and tone observations use real S1/S2 simulator sockets.

use fsonos_core::control::{self, Tone, ToneChange};
use fsonos_core::{CoreError, HouseholdState, actions};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::cell::{Cell, RefCell};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

const WANTED: Tone = Tone {
    bass: -4,
    treble: 6,
    loudness: false,
};
const FLAT: Tone = Tone {
    bass: 0,
    treble: 0,
    loudness: true,
};

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    target: PlayerId,
    host: IpAddr,
    peer: IpAddr,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let players = [
            SimPlayerSpec::new("Target", model),
            SimPlayerSpec::new("Peer", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(players)
        } else {
            builder.s2(players)
        }
        .spawn()
        .unwrap();
        let host = sim.player("Target").unwrap().ip;
        let peer = sim.player("Peer").unwrap().ip;
        let target = PlayerId(sim.player("Target").unwrap().uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
        Self {
            sim,
            houses: vec![house],
            target,
            host,
            peer,
        }
    }

    fn join_peer(&mut self) {
        let peer = PlayerId(self.sim.player("Peer").unwrap().uuid.clone());
        soap::join_group(&self.sim.lan(), self.host, &peer).unwrap();
        self.houses[0].apply_topology(&get_zone_group_state(&self.sim.lan(), self.host).unwrap());
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

fn read(t: &impl Transport, host: IpAddr) -> Tone {
    Tone {
        bass: soap::get_bass(t, host).unwrap(),
        treble: soap::get_treble(t, host).unwrap(),
        loudness: soap::get_loudness(t, host).unwrap(),
    }
}

#[test]
fn stale_tone_addresses_are_refused_before_reading_or_writing_the_peers_fields() {
    for generation in [1, 2] {
        for grouped in [false, true] {
            let mut f = Fixture::new(generation);
            if grouped {
                f.join_peer();
            }
            let lan = f.sim.lan();
            let stale = f.stale();
            let before = f.sim.soap_log().len();
            assert!(matches!(
                control::tone(&lan, &stale, &f.target),
                Err(CoreError::TargetChanged(_))
            ));
            let mut touched = ToneChange::all(WANTED);
            assert!(matches!(
                control::set_tone_tracked(
                    &lan,
                    &stale,
                    &f.target,
                    ToneChange::all(WANTED),
                    &mut touched
                ),
                Err(CoreError::TargetChanged(_))
            ));
            assert_eq!(touched, ToneChange::default());
            assert!(
                f.sim.soap_log()[before..]
                    .iter()
                    .all(|entry| entry.action == "GetZoneGroupState")
            );
            assert_eq!(read(&lan, f.host), FLAT);
            assert_eq!(read(&lan, f.peer), FLAT);
        }
    }
}

#[test]
fn a_group_member_can_read_and_change_its_own_tone_without_changing_the_leader() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation);
        f.join_peer();
        let lan = f.sim.lan();
        assert_eq!(control::tone(&lan, &f.houses, &f.target).unwrap(), FLAT);
        let before = f.sim.soap_log().len();
        let mut touched = ToneChange::default();
        control::set_tone_tracked(
            &lan,
            &f.houses,
            &f.target,
            ToneChange::all(WANTED),
            &mut touched,
        )
        .unwrap();
        assert_eq!(touched, ToneChange::all(WANTED));
        let writes: Vec<_> = f.sim.soap_log()[before..]
            .iter()
            .filter(|entry| entry.action.starts_with("Set"))
            .map(|entry| (entry.room.clone(), entry.action.clone()))
            .collect();
        assert_eq!(
            writes,
            ["SetBass", "SetTreble", "SetLoudness"]
                .map(|action| ("Target".to_string(), action.to_string()))
        );
        assert_eq!(control::tone(&lan, &f.houses, &f.target).unwrap(), WANTED);
        assert_eq!(read(&lan, f.peer), FLAT);
    }
}

struct ReusedAddress {
    sim: RefCell<SimHandle>,
    inner: SimLan,
    swap_after: &'static str,
    lose_reply: Option<&'static str>,
    moved: Cell<bool>,
    actions: RefCell<Vec<String>>,
}

impl ReusedAddress {
    fn new(sim: SimHandle, swap_after: &'static str, lose_reply: Option<&'static str>) -> Self {
        Self {
            inner: sim.lan(),
            sim: RefCell::new(sim),
            swap_after,
            lose_reply,
            moved: Cell::new(false),
            actions: RefCell::new(Vec::new()),
        }
    }

    fn writes(&self) -> Vec<String> {
        self.actions
            .borrow()
            .iter()
            .filter(|action| action.starts_with("Set"))
            .cloned()
            .collect()
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
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        self.actions.borrow_mut().push(name.into());
        let response = self.inner.soap_post(host, path, action, body)?;
        if name == self.swap_after && !self.moved.replace(true) {
            let IpAddr::V4(old) = host else {
                panic!("synthetic players use IPv4");
            };
            let mut sim = self.sim.borrow_mut();
            sim.change_address("Target").unwrap();
            sim.move_to("Peer", old, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .unwrap();
        }
        if self.lose_reply == Some(name) {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "tone reply lost during address reuse".into(),
            });
        }
        Ok(response)
    }
}

#[test]
fn endpoint_reuse_between_tone_fields_preserves_only_the_original_touched_scope() {
    for generation in [1, 2] {
        for stage in ["SetBass", "SetTreble"] {
            let f = Fixture::new(generation);
            let transport = ReusedAddress::new(f.sim, stage, None);
            let mut touched = ToneChange::default();
            let error = control::set_tone_tracked(
                &transport,
                &f.houses,
                &f.target,
                ToneChange::all(WANTED),
                &mut touched,
            )
            .unwrap_err();
            assert!(transport.moved.get());
            assert!(matches!(error, CoreError::TargetChanged(_)), "{error}");
            assert_eq!(
                touched,
                ToneChange {
                    bass: Some(WANTED.bass),
                    treble: (stage == "SetTreble").then_some(WANTED.treble),
                    loudness: None,
                }
            );
            assert_eq!(
                transport.writes(),
                if stage == "SetBass" {
                    vec!["SetBass"]
                } else {
                    vec!["SetBass", "SetTreble"]
                }
            );
            let sim = transport.sim.borrow();
            assert_eq!(read(&transport.inner, sim.player("Peer").unwrap().ip), FLAT);
            assert_eq!(
                read(&transport.inner, sim.player("Target").unwrap().ip),
                Tone {
                    bass: WANTED.bass,
                    treble: if stage == "SetTreble" {
                        WANTED.treble
                    } else {
                        0
                    },
                    loudness: true,
                }
            );
        }
    }
}

#[test]
fn a_lost_tone_reply_never_uses_matching_values_from_a_replacement_player() {
    for generation in [1, 2] {
        for swap_after in ["SetBass", "GetBass"] {
            let f = Fixture::new(generation);
            // Matching values on the replacement must not establish ownership
            // of either the uncertain write or the fields still to be sent.
            soap::set_bass(&f.sim.lan(), f.peer, WANTED.bass).unwrap();
            let transport = ReusedAddress::new(f.sim, swap_after, Some("SetBass"));
            let mut touched = ToneChange::default();
            let error = control::set_tone_tracked(
                &transport,
                &f.houses,
                &f.target,
                ToneChange::all(WANTED),
                &mut touched,
            )
            .unwrap_err();
            assert!(transport.moved.get());
            assert!(
                matches!(error, CoreError::Proto(ProtoError::Network { ref detail, .. })
                if detail == "tone reply lost during address reuse"),
                "{error}"
            );
            assert_eq!(transport.writes(), ["SetBass"]);
            assert_eq!(
                touched,
                ToneChange {
                    bass: Some(WANTED.bass),
                    ..ToneChange::default()
                }
            );
            assert_eq!(
                transport
                    .actions
                    .borrow()
                    .iter()
                    .filter(|action| *action == "GetBass")
                    .count(),
                usize::from(swap_after == "GetBass")
            );
            assert_eq!(
                read(
                    &transport.inner,
                    transport.sim.borrow().player("Peer").unwrap().ip
                ),
                Tone {
                    bass: WANTED.bass,
                    ..FLAT
                }
            );
        }
    }
}

#[test]
fn a_tone_snapshot_cannot_mix_fields_from_two_physical_players() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        soap::set_bass(&f.sim.lan(), f.host, WANTED.bass).unwrap();
        soap::set_treble(&f.sim.lan(), f.peer, WANTED.treble).unwrap();
        let transport = ReusedAddress::new(f.sim, "GetBass", None);
        assert!(matches!(
            control::tone(&transport, &f.houses, &f.target),
            Err(CoreError::TargetChanged(_))
        ));
        assert!(transport.moved.get());
        assert!(transport.writes().is_empty());
    }
}

#[test]
fn undo_capture_keeps_reassigned_or_mid_read_tone_evidence_unknown() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let before = f.sim.soap_log().len();
        assert_eq!(
            actions::capture_tone_after(
                &f.sim.lan(),
                &f.stale(),
                &f.target,
                ToneChange::all(WANTED)
            ),
            ToneChange::default()
        );
        assert!(
            f.sim.soap_log()[before..]
                .iter()
                .all(|entry| entry.action == "GetZoneGroupState")
        );
        let transport = ReusedAddress::new(f.sim, "GetBass", None);
        assert_eq!(
            actions::capture_tone_after(&transport, &f.houses, &f.target, ToneChange::all(WANTED)),
            ToneChange::default()
        );
        assert!(transport.moved.get());
        assert!(transport.writes().is_empty());
    }
}
