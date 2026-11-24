//! Whole-room leave completion over real S1/S2 loopback SOAP. A wire overlay
//! reproduces S1's handoff with the hidden stereo half left with the heir;
//! no simulator model changes or real speakers are needed.

use fsonos_core::grouping;
use fsonos_core::{CoreError, HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::collections::HashSet;
use std::net::IpAddr;

#[path = "control/grouping_fixture.rs"]
mod grouping_fixture;

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    leader: PlayerId,
    hidden: PlayerId,
    heir: PlayerId,
    leader_host: IpAddr,
    hidden_host: IpAddr,
    heir_host: IpAddr,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let specs = [
            SimPlayerSpec::pair("Leader", model),
            SimPlayerSpec::new("Heir", model),
            SimPlayerSpec::new("Witness", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(specs)
        } else {
            builder.s2(specs)
        }
        .spawn()
        .unwrap();
        let primary = sim.player("Leader").unwrap();
        let secondary = sim.players().iter().find(|player| player.hidden).unwrap();
        let other = sim.player("Heir").unwrap();
        let leader = PlayerId(primary.uuid.clone());
        let hidden = PlayerId(secondary.uuid.clone());
        let heir = PlayerId(other.uuid.clone());
        let (leader_host, hidden_host, heir_host) = (primary.ip, secondary.ip, other.ip);
        let lan = sim.lan();
        for room in ["Heir", "Witness"] {
            soap::join_group(&lan, sim.player(room).unwrap().ip, &leader).unwrap();
        }
        for number in 1..=3 {
            soap::add_uri_to_queue(
                &lan,
                leader_host,
                &format!("x-file-cifs://example.invalid/handoff-{number}.flac"),
                "",
                false,
            )
            .unwrap();
        }
        soap::play_from_queue(&lan, leader_host, &leader).unwrap();
        soap::seek_track(&lan, leader_host, 2).unwrap();
        soap::seek_position(&lan, leader_host, 37).unwrap();
        soap::play(&lan, leader_host).unwrap();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, leader_host).unwrap());
        Self {
            sim,
            houses: vec![house],
            leader,
            hidden,
            heir,
            leader_host,
            hidden_host,
            heir_host,
        }
    }

    fn queue_preserved(&self) {
        let lan = self.sim.lan();
        let position = soap::get_position_info(&lan, self.heir_host).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(37)));
        assert_eq!(position.uri, "x-file-cifs://example.invalid/handoff-2.flac");
        assert_eq!(
            soap::get_transport_info(&lan, self.heir_host).unwrap().state,
            TransportState::Playing
        );
        assert_eq!(soap::get_media_info(&lan, self.heir_host).unwrap().tracks, 3);
    }

    fn whole_room(&self, transport: &impl Transport) -> bool {
        let topology = get_zone_group_state(transport, self.heir_host).unwrap();
        let expected: HashSet<_> = [&self.leader, &self.hidden].into_iter().cloned().collect();
        topology.groups.iter().any(|group| {
            group.coordinator == self.leader
                && group.members.iter().map(|m| m.uuid.clone()).collect::<HashSet<_>>() == expected
        })
    }

    fn writes_since(&self, since: usize) -> Vec<(String, String)> {
        self.sim.soap_log()[since..]
            .iter()
            .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
            .map(|entry| (entry.player.clone(), entry.action.clone()))
            .collect()
    }

    fn expected_writes(&self) -> [(String, String); 2] {
        [
            (self.leader.0.clone(), "DelegateGroupCoordinationTo".into()),
            (self.hidden.0.clone(), "SetAVTransportURI".into()),
        ]
    }
}

#[derive(Debug, Clone, Copy)]
enum Rejoin {
    Applied,
    Lost,
    Malformed,
    Refused,
    Ignored,
    Unreadable,
}

struct SplitPair<'a> {
    fixture: &'a Fixture,
    lan: SimLan,
    rejoin: Rejoin,
    lose_handoff: bool,
    stale_self: bool,
    handed_over: Cell<bool>,
    reunited: Cell<bool>,
    rejoins: Cell<usize>,
    initial: RefCell<Option<String>>,
}

impl<'a> SplitPair<'a> {
    fn new(fixture: &'a Fixture, rejoin: Rejoin) -> Self {
        Self {
            fixture,
            lan: fixture.sim.lan(),
            rejoin,
            lose_handoff: false,
            stale_self: false,
            handed_over: Cell::new(false),
            reunited: Cell::new(false),
            rejoins: Cell::new(0),
            initial: RefCell::new(None),
        }
    }

    fn lost(detail: &str) -> ProtoError {
        ProtoError::Network {
            target: "synthetic handoff".into(),
            detail: detail.into(),
        }
    }
}

impl Transport for SplitPair<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name == "GetZoneGroupState"
            && self.rejoins.get() > 0
            && matches!(self.rejoin, Rejoin::Unreadable)
        {
            return Err(Self::lost("post-reunion topology unavailable"));
        }
        let reply = self.lan.soap_post(host, path, action, body)?;
        if name == "DelegateGroupCoordinationTo" {
            self.handed_over.set(true);
            if self.lose_handoff {
                return Err(Self::lost("handoff reply lost"));
            }
        }
        if name == "SetAVTransportURI" && host == self.fixture.hidden_host {
            self.rejoins.set(self.rejoins.get() + 1);
            match self.rejoin {
                Rejoin::Applied | Rejoin::Unreadable => self.reunited.set(true),
                Rejoin::Lost => {
                    self.reunited.set(true);
                    return Err(Self::lost("bonded rejoin reply lost"));
                }
                Rejoin::Malformed => {
                    self.reunited.set(true);
                    return Ok("unreadable bonded rejoin reply".into());
                }
                Rejoin::Refused => {
                    return Err(ProtoError::SoapFault {
                        code: 800,
                        reason: "bonded rejoin refused".into(),
                    });
                }
                Rejoin::Ignored => {}
            }
        }
        if name != "GetZoneGroupState" {
            return Ok(reply);
        }
        if !self.handed_over.get() {
            *self.initial.borrow_mut() = Some(reply.clone());
            return Ok(reply);
        }
        if self.stale_self && host == self.fixture.leader_host {
            return Ok(self.initial.borrow().as_ref().unwrap().clone());
        }
        if !self.reunited.get() {
            return Ok(grouping_fixture::relocate(
                &reply,
                &self.fixture.hidden,
                &self.fixture.heir,
            ));
        }
        Ok(reply)
    }
}

#[test]
fn acknowledged_and_lost_handoffs_reunite_the_whole_room_once() {
    for generation in [1, 2] {
        for lose_handoff in [false, true] {
            for rejoin in [Rejoin::Applied, Rejoin::Lost, Rejoin::Malformed] {
                let f = Fixture::new(generation);
                let mut transport = SplitPair::new(&f, rejoin);
                transport.lose_handoff = lose_handoff;
                let before = f.sim.soap_log().len();
                control::leave(&transport, &f.houses, &f.leader).unwrap();
                assert!(f.whole_room(&transport), "S{generation} {rejoin:?}");
                assert_eq!(transport.rejoins.get(), 1);
                assert_eq!(f.writes_since(before), f.expected_writes());
                f.queue_preserved();
            }
        }
    }
}

#[test]
fn a_stale_self_view_cannot_hide_a_missing_stereo_half() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let mut transport = SplitPair::new(&f, Rejoin::Applied);
        transport.stale_self = true;
        let before = f.sim.soap_log().len();
        control::leave(&transport, &f.houses, &f.leader).unwrap();
        assert!(f.whole_room(&transport));
        assert_eq!(f.writes_since(before), f.expected_writes());
        f.queue_preserved();
    }
}

#[test]
fn refused_or_unobserved_reunions_report_partial_failure() {
    for generation in [1, 2] {
        for rejoin in [Rejoin::Refused, Rejoin::Ignored, Rejoin::Unreadable] {
            let f = Fixture::new(generation);
            let transport = SplitPair::new(&f, rejoin);
            let target = resolve_room(&f.houses, "Leader").unwrap();
            let before = f.sim.soap_log().len();
            let result = grouping::ungroup(&transport, &f.houses, &[target]);
            assert!(result.moved.is_empty(), "S{generation} {rejoin:?}");
            assert!(!result.is_complete());
            assert_eq!(result.failed.len(), 1);
            assert!(
                matches!(&result.failed[0].1, CoreError::Uncertain(message)
                    if message.contains("bonded players") && message.contains("inspect")),
                "{:?}",
                result.failed
            );
            assert_eq!(transport.rejoins.get(), 1);
            assert_eq!(f.writes_since(before), f.expected_writes());
            if !matches!(rejoin, Rejoin::Unreadable) {
                assert!(!f.whole_room(&transport));
            }
            f.queue_preserved();
        }
    }
}

#[test]
fn an_acknowledged_but_unapplied_handoff_is_not_replayed() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        f.sim
            .ignore_action("Leader", "DelegateGroupCoordinationTo")
            .unwrap();
        let before = f.sim.soap_log().len();
        let result = control::leave(&f.sim.lan(), &f.houses, &f.leader);
        assert!(
            matches!(result, Err(CoreError::Uncertain(ref message))
                if message.contains("handoff")),
            "{result:?}"
        );
        assert_eq!(
            f.writes_since(before),
            [(f.leader.0.clone(), "DelegateGroupCoordinationTo".into())]
        );
    }
}
