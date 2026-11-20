//! Grouping binds both physical endpoints before the first write. A stale
//! address, destination or incoherent household cannot move an unrelated room.

use fsonos_core::{CoreError, HouseholdState, control, heal, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::soap::{ZONE_GROUP_TOPOLOGY, args_xml, envelope, parse_response};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::cell::Cell;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    member: PlayerId,
    leader: PlayerId,
    peer: PlayerId,
    member_host: IpAddr,
    leader_host: IpAddr,
    peer_host: IpAddr,
    foreign: PlayerId,
    foreign_host: IpAddr,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let rooms = |generation, selected| {
            let model = if generation == 1 {
                SimModel::Play5Gen1
            } else {
                SimModel::One
            };
            if selected {
                vec![
                    SimPlayerSpec::pair("Member", model),
                    SimPlayerSpec::pair("Leader", model),
                    SimPlayerSpec::new("Peer", model),
                ]
            } else {
                vec![SimPlayerSpec::new("Foreign", model)]
            }
        };
        let sim = SimHousehold::builder()
            .s1(rooms(1, generation == 1))
            .s2(rooms(2, generation == 2))
            .spawn()
            .unwrap();
        let player = |name| {
            let player = sim.player(name).unwrap();
            (PlayerId(player.uuid.clone()), player.ip)
        };
        let (member, member_host) = player("Member");
        let (leader, leader_host) = player("Leader");
        let (peer, peer_host) = player("Peer");
        let (foreign, foreign_host) = player("Foreign");
        let houses = [leader_host, foreign_host]
            .into_iter()
            .map(|host| {
                let mut house = HouseholdState::default();
                house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
                house
            })
            .collect();
        Self {
            sim,
            houses,
            member,
            leader,
            peer,
            member_host,
            leader_host,
            peer_host,
            foreign,
            foreign_host,
        }
    }

    fn stale(&self, player: &PlayerId) -> Vec<HouseholdState> {
        let mut houses = self.houses.clone();
        houses[0]
            .players
            .iter_mut()
            .find(|known| known.id == *player)
            .unwrap()
            .ip = self.peer_host;
        houses
    }

    fn grouped(&mut self) {
        soap::join_group(&self.sim.lan(), self.member_host, &self.leader).unwrap();
        self.houses[0]
            .apply_topology(&get_zone_group_state(&self.sim.lan(), self.leader_host).unwrap());
    }

    fn writes_since(&self, start: usize) -> Vec<(String, String)> {
        self.sim.soap_log()[start..]
            .iter()
            .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
            .map(|entry| (entry.room.clone(), entry.action.clone()))
            .collect()
    }

    fn coordinator(&self, room: &str) -> PlayerId {
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&self.sim.lan(), self.leader_host).unwrap());
        resolve_room(std::slice::from_ref(&house), room)
            .unwrap()
            .room
            .coordinator
            .clone()
    }
}

#[test]
fn stale_join_endpoints_do_not_regroup_the_other_player_at_that_address() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        for player in [&f.member, &f.leader] {
            let before = f.sim.soap_log().len();
            let error =
                control::join(&f.sim.lan(), &f.stale(player), &f.member, &f.leader).unwrap_err();
            assert!(matches!(error, CoreError::TargetChanged(_)), "{error}");
            assert!(f.writes_since(before).is_empty());
            assert_eq!(f.coordinator("Member"), f.member);
            assert_eq!(f.coordinator("Peer"), f.peer);
        }
    }
}

#[test]
fn a_destination_that_joined_another_group_cannot_receive_the_old_join() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        soap::join_group(&f.sim.lan(), f.leader_host, &f.peer).unwrap();
        let before = f.sim.soap_log().len();
        let error = control::join(&f.sim.lan(), &f.houses, &f.member, &f.leader).unwrap_err();
        assert!(matches!(error, CoreError::TargetChanged(_)), "{error}");
        assert!(f.writes_since(before).is_empty());
        assert_eq!(f.coordinator("Member"), f.member);
        assert_eq!(f.coordinator("Leader"), f.peer);
    }
}

#[test]
fn the_core_refuses_cross_household_joins_before_any_speaker_read_or_write() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let before = f.sim.soap_log().len();
        let error = control::join(&f.sim.lan(), &f.houses, &f.member, &f.foreign).unwrap_err();
        assert!(matches!(error, CoreError::TargetChanged(_)), "{error}");
        assert_eq!(f.sim.soap_log().len(), before);
    }
}

#[test]
fn a_stale_leave_endpoint_cannot_adopt_another_players_topology_and_write_elsewhere() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation);
        f.grouped();
        for player in [&f.member, &f.leader] {
            let before = f.sim.soap_log().len();
            let error = control::leave(&f.sim.lan(), &f.stale(player), player).unwrap_err();
            assert!(matches!(error, CoreError::TargetChanged(_)), "{error}");
            assert!(f.writes_since(before).is_empty());
            assert_eq!(f.coordinator("Member"), f.leader);
        }
    }
}

#[test]
fn address_recovery_joins_and_leaves_the_original_room_once_after_real_address_reuse() {
    for generation in [1, 2] {
        for leaving in [false, true] {
            let mut f = Fixture::new(generation);
            if leaving {
                f.grouped();
            }
            let IpAddr::V4(old) = f.member_host else {
                panic!("the simulator uses IPv4");
            };
            let to = f.sim.change_address("Member").unwrap();
            f.sim
                .move_to("Peer", old, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .unwrap();
            let lan = f.sim.lan();
            let mut houses = f.houses.clone();
            let before = f.sim.soap_log().len();
            let (_, recovery) = heal::readdressing(
                &lan,
                &[to, f.leader_host],
                &mut houses,
                &f.member,
                |households| {
                    if leaving {
                        control::leave(&lan, households, &f.member)
                    } else {
                        control::join(&lan, households, &f.member, &f.leader)
                    }
                },
            )
            .unwrap();
            assert_eq!(
                recovery,
                heal::Recovery::Readdressed {
                    player: f.member.clone(),
                    from: f.member_host,
                    to,
                }
            );
            assert_eq!(
                f.writes_since(before),
                [("Member".into(), "SetAVTransportURI".into())]
            );
            assert_eq!(
                f.coordinator("Member"),
                if leaving {
                    f.member.clone()
                } else {
                    f.leader.clone()
                }
            );
            assert_eq!(f.coordinator("Peer"), f.peer);
            assert!(houses[1].player(&f.foreign).is_some());
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum Fault {
    DuplicateCoordinator,
    DuplicateAddress,
    MissingCoordinator,
    ForeignIdentity,
    ForeignAddress,
    WrongResponder,
}

struct Replies<'a> {
    inner: SimLan,
    fixture: &'a Fixture,
    fault: Fault,
    after_write: bool,
    writes: Cell<usize>,
}

impl Replies<'_> {
    fn lost() -> ProtoError {
        ProtoError::Network {
            target: "synthetic grouping endpoint".into(),
            detail: "grouping acknowledgement was lost".into(),
        }
    }

    fn alter(&self, document: &mut String, host: IpAddr) {
        let f = self.fixture;
        let extra = match self.fault {
            Fault::DuplicateCoordinator => Some((f.leader.0.as_str(), "192.0.2.250".into())),
            Fault::ForeignIdentity => Some((f.foreign.0.as_str(), "192.0.2.250".into())),
            Fault::ForeignAddress => Some(("synthetic-foreign", f.foreign_host.to_string())),
            _ => None,
        };
        if let Some((id, address)) = extra {
            let group = format!(
                "<ZoneGroup Coordinator=\"{id}\"><ZoneGroupMember UUID=\"{id}\" \
                 Location=\"http://{address}:1400/x\" ZoneName=\"Synthetic\"/></ZoneGroup>"
            );
            *document = document.replace("</ZoneGroups>", &format!("{group}</ZoneGroups>"));
        }
        match self.fault {
            Fault::DuplicateAddress => {
                *document = document.replace(
                    &format!("http://{}:", f.peer_host),
                    &format!("http://{}:", f.leader_host),
                );
            }
            Fault::MissingCoordinator => {
                *document = document.replace(
                    &format!("Coordinator=\"{}\"", f.leader.0),
                    "Coordinator=\"synthetic-absent\"",
                );
            }
            Fault::WrongResponder => {
                *document = document.replace(&format!("http://{host}:"), "http://192.0.2.250:");
            }
            _ => {}
        }
    }
}

impl Transport for Replies<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        let reply = self.inner.soap_post(host, path, action, body)?;
        if !name.starts_with("Get") && name != "Browse" {
            self.writes.set(self.writes.get() + 1);
            if self.after_write {
                return Err(Self::lost());
            }
        }
        if name != "GetZoneGroupState" || (self.after_write && self.writes.get() == 0) {
            return Ok(reply);
        }
        let response = parse_response(&reply, name)?;
        let mut document = response.require("ZoneGroupState")?.to_owned();
        self.alter(&mut document, host);
        Ok(envelope(
            &ZONE_GROUP_TOPOLOGY,
            "GetZoneGroupStateResponse",
            &args_xml(&[("ZoneGroupState", &document)]),
        ))
    }
}

#[test]
fn incoherent_or_foreign_topology_cannot_choose_a_join_or_leave_endpoint() {
    for generation in [1, 2] {
        for fault in [
            Fault::DuplicateCoordinator,
            Fault::DuplicateAddress,
            Fault::MissingCoordinator,
            Fault::ForeignIdentity,
            Fault::ForeignAddress,
            Fault::WrongResponder,
        ] {
            let f = Fixture::new(generation);
            let transport = Replies {
                inner: f.sim.lan(),
                fixture: &f,
                fault,
                after_write: false,
                writes: Cell::new(0),
            };
            let before = f.sim.soap_log().len();
            for result in [
                control::join(&transport, &f.houses, &f.member, &f.leader),
                control::leave(&transport, &f.houses, &f.leader),
            ] {
                assert!(result.is_err(), "S{generation} {fault:?}: {result:?}");
            }
            assert_eq!(transport.writes.get(), 0);
            assert!(f.writes_since(before).is_empty());
        }
    }
}

#[test]
fn invalid_or_reassigned_witnesses_cannot_turn_a_lost_join_reply_into_success() {
    for generation in [1, 2] {
        for fault in [Fault::DuplicateAddress, Fault::WrongResponder] {
            let f = Fixture::new(generation);
            let transport = Replies {
                inner: f.sim.lan(),
                fixture: &f,
                fault,
                after_write: true,
                writes: Cell::new(0),
            };
            let before = f.sim.soap_log().len();
            let error = control::join(&transport, &f.houses, &f.member, &f.leader).unwrap_err();
            assert!(
                matches!(error, CoreError::Proto(ProtoError::Network { ref detail, .. })
                if detail == "grouping acknowledgement was lost"),
                "{error}"
            );
            assert_eq!(transport.writes.get(), 1);
            assert_eq!(
                f.writes_since(before),
                [("Member".into(), "SetAVTransportURI".into())]
            );
            assert_eq!(f.coordinator("Member"), f.leader);
        }
    }
}

#[test]
fn coherent_paired_rooms_still_join_leave_and_hand_off_their_queue() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let lan = f.sim.lan();
        let source = "x-file-cifs://example.invalid/grouping-track.flac";
        soap::add_uri_to_queue(&lan, f.leader_host, source, "", false).unwrap();
        soap::play_from_queue(&lan, f.leader_host, &f.leader).unwrap();
        soap::seek_position(&lan, f.leader_host, 23).unwrap();
        soap::play(&lan, f.leader_host).unwrap();
        let before = f.sim.soap_log().len();
        control::join(&lan, &f.houses, &f.member, &f.leader).unwrap();
        assert_eq!(f.coordinator("Member"), f.leader);
        // Leave reads fresh topology even though this planning snapshot still
        // shows the two rooms alone; the member inherits the playing queue.
        control::leave(&lan, &f.houses, &f.leader).unwrap();
        assert_eq!(f.coordinator("Leader"), f.leader);
        assert_eq!(f.coordinator("Member"), f.member);
        assert_eq!(
            f.writes_since(before),
            [
                ("Member".into(), "SetAVTransportURI".into()),
                ("Leader".into(), "DelegateGroupCoordinationTo".into())
            ]
        );
        let position = soap::get_position_info(&lan, f.member_host).unwrap();
        assert_eq!(position.uri, source);
        assert_eq!(position.position_secs, Some(23));
        assert_eq!(
            soap::get_transport_info(&lan, f.member_host).unwrap().state,
            fsonos_types::TransportState::Playing
        );
        let mut current = HouseholdState::default();
        current.apply_topology(&get_zone_group_state(&lan, f.leader_host).unwrap());
        for room in ["Member", "Leader"] {
            assert_eq!(
                resolve_room(std::slice::from_ref(&current), room)
                    .unwrap()
                    .room
                    .players
                    .len(),
                2
            );
        }
    }
}
