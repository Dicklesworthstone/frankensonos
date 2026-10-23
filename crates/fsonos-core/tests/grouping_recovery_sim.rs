//! Grouping writes over real S1/S2 loopback SOAP, with their replies erased
//! after the simulator acts. Recovery observes topology and never replays.

use fsonos_core::moving::{MoveMethod, move_playback};
use fsonos_core::{CoreError, HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Duration;

struct Fixture {
    sim: SimHandle,
    leader: PlayerId,
    member: PlayerId,
    leader_host: IpAddr,
    member_host: IpAddr,
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
            SimPlayerSpec::pair("Member", model),
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
        let leader = sim.player("Leader").unwrap();
        let member = sim.player("Member").unwrap();
        Self {
            leader: PlayerId(leader.uuid.clone()),
            member: PlayerId(member.uuid.clone()),
            leader_host: leader.ip,
            member_host: member.ip,
            sim,
        }
    }

    fn houses(&self) -> Vec<HouseholdState> {
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&self.sim.lan(), self.leader_host).unwrap());
        vec![house]
    }

    fn queue(&self) {
        let lan = self.sim.lan();
        for number in 1..=3 {
            soap::add_uri_to_queue(
                &lan,
                self.leader_host,
                &format!("x-file-cifs://example.invalid/track-{number}.flac"),
                "",
                false,
            )
            .unwrap();
        }
        soap::play_from_queue(&lan, self.leader_host, &self.leader).unwrap();
        soap::seek_track(&lan, self.leader_host, 2).unwrap();
        soap::seek_position(&lan, self.leader_host, 37).unwrap();
        soap::play(&lan, self.leader_host).unwrap();
    }

    fn assert_queue_preserved(&self) {
        let lan = self.sim.lan();
        let position = soap::get_position_info(&lan, self.member_host).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(37)));
        assert_eq!(position.uri, "x-file-cifs://example.invalid/track-2.flac");
        assert_eq!(
            soap::get_transport_info(&lan, self.member_host)
                .unwrap()
                .state,
            TransportState::Playing
        );
        assert_eq!(
            soap::get_media_info(&lan, self.member_host).unwrap().tracks,
            3
        );
    }

    fn writes_since(&self, since: usize) -> Vec<String> {
        self.sim.soap_log()[since..]
            .iter()
            .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
            .map(|entry| entry.action.clone())
            .collect()
    }
}

struct Lost {
    lan: SimLan,
    action: &'static str,
    malformed: bool,
    writes: Cell<usize>,
    unavailable: Option<IpAddr>,
    all_unavailable: bool,
    missing: Option<String>,
}

impl Lost {
    fn new(f: &Fixture, action: &'static str, malformed: bool) -> Self {
        Self {
            lan: f.sim.lan(),
            action,
            malformed,
            writes: Cell::new(0),
            unavailable: None,
            all_unavailable: false,
            missing: None,
        }
    }

    fn failure() -> ProtoError {
        ProtoError::Network {
            target: "simulator".into(),
            detail: "group write acknowledgement lost".into(),
        }
    }
}

impl Transport for Lost {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if self.writes.get() > 0
            && name == "GetZoneGroupState"
            && (self.all_unavailable || self.unavailable == Some(host))
        {
            return Err(ProtoError::Network {
                target: "simulator".into(),
                detail: "topology unavailable".into(),
            });
        }
        let reply = self.lan.soap_post(host, path, action, body)?;
        if name == self.action {
            self.writes.set(self.writes.get() + 1);
            // A member's self-join really applies before the normal 402 reply.
            // Erasing that reply exercises the uncertain path too.
            return if self.malformed {
                Ok("unreadable acknowledgement".into())
            } else {
                Err(Self::failure())
            };
        }
        if self.writes.get() > 0
            && name == "GetZoneGroupState"
            && let Some(missing) = &self.missing
        {
            return Ok(reply.replace(missing, "RINCON_SYNTHETIC_MISSING"));
        }
        Ok(reply)
    }
}

#[test]
fn joined_and_leaving_stereo_rooms_recover_lost_and_malformed_replies() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        for malformed in [false, true] {
            let houses = f.houses();
            f.sim
                .join_lag("Member", Duration::from_millis(120))
                .unwrap();
            let mut t = Lost::new(&f, "SetAVTransportURI", malformed);
            // The acting player's own topology may be unavailable; a same-
            // household witness still confirms the whole bonded room.
            t.unavailable = Some(f.member_host);
            let before = f.sim.soap_log().len();
            control::join(&t, &houses, &f.member, &f.leader).unwrap();
            assert_eq!(t.writes.get(), 1);
            assert_eq!(f.writes_since(before), ["SetAVTransportURI"]);
            let houses = f.houses();
            let target = resolve_room(&houses, "Member").unwrap();
            assert_eq!(target.room.coordinator, f.leader);
            assert_eq!(target.room.players.len(), 2);
            f.sim.clear_faults("Member").unwrap();
            let mut t = Lost::new(&f, "SetAVTransportURI", malformed);
            t.unavailable = Some(f.member_host);
            let before = f.sim.soap_log().len();
            control::leave(&t, &houses, &f.member).unwrap();
            assert_eq!(t.writes.get(), 1);
            assert_eq!(f.writes_since(before), ["SetAVTransportURI"]);
            let houses = f.houses();
            let target = resolve_room(&houses, "Member").unwrap();
            assert_eq!(target.room.coordinator, f.member);
            assert_eq!(target.room.players.len(), 2);
        }
    }
}

#[test]
fn leading_room_handoff_recovers_without_replaying_or_losing_the_queue() {
    for generation in [1, 2] {
        for malformed in [false, true] {
            let f = Fixture::new(generation);
            soap::join_group(&f.sim.lan(), f.member_host, &f.leader).unwrap();
            soap::join_group(&f.sim.lan(), f.sim.player("Witness").unwrap().ip, &f.leader).unwrap();
            f.queue();
            let mut t = Lost::new(&f, "DelegateGroupCoordinationTo", malformed);
            t.unavailable = Some(f.leader_host);
            let before = f.sim.soap_log().len();
            control::leave(&t, &f.houses(), &f.leader).unwrap();
            assert_eq!(t.writes.get(), 1);
            assert_eq!(f.writes_since(before), ["DelegateGroupCoordinationTo"]);
            let houses = f.houses();
            assert_eq!(
                resolve_room(&houses, "Leader").unwrap().room.coordinator,
                f.leader
            );
            assert_eq!(
                resolve_room(&houses, "Witness").unwrap().room.coordinator,
                f.member
            );
            assert_eq!(
                resolve_room(&houses, "Member").unwrap().room.players.len(),
                2
            );
            f.assert_queue_preserved();
        }
    }
}

#[test]
fn move_completes_after_its_join_reply_is_lost() {
    for generation in [1, 2] {
        for malformed in [false, true] {
            let f = Fixture::new(generation);
            f.queue();
            let houses = f.houses();
            let t = Lost::new(&f, "SetAVTransportURI", malformed);
            let before = f.sim.soap_log().len();
            let result = move_playback(
                &t,
                &houses,
                &resolve_room(&houses, "Leader").unwrap(),
                &resolve_room(&houses, "Member").unwrap(),
            )
            .unwrap();
            assert_eq!(result.method, MoveMethod::Delegated);
            assert_eq!(result.new_coordinator, f.member);
            assert_eq!(t.writes.get(), 1);
            assert_eq!(
                f.writes_since(before),
                ["SetAVTransportURI", "DelegateGroupCoordinationTo"]
            );
            f.assert_queue_preserved();
        }
    }
}

#[test]
fn unavailable_topology_keeps_the_original_uncertain_error_and_never_repeats() {
    for action in ["SetAVTransportURI", "DelegateGroupCoordinationTo"] {
        let f = Fixture::new(1);
        if action == "DelegateGroupCoordinationTo" {
            soap::join_group(&f.sim.lan(), f.member_host, &f.leader).unwrap();
        }
        let mut t = Lost::new(&f, action, false);
        t.all_unavailable = true;
        let before = f.sim.soap_log().len();
        let result = if action == "DelegateGroupCoordinationTo" {
            control::leave(&t, &f.houses(), &f.leader)
        } else {
            control::join(&t, &f.houses(), &f.member, &f.leader)
        };
        assert!(
            matches!(result, Err(CoreError::Proto(ProtoError::Network { ref detail, .. }))
            if detail == "group write acknowledgement lost")
        );
        assert_eq!(t.writes.get(), 1);
        assert_eq!(f.writes_since(before), [action]);
    }
}

#[test]
fn a_missing_bonded_player_cannot_confirm_a_join() {
    let f = Fixture::new(2);
    let mut t = Lost::new(&f, "SetAVTransportURI", false);
    t.missing = Some(
        f.sim
            .players()
            .iter()
            .find(|p| p.hidden)
            .unwrap()
            .uuid
            .clone(),
    );
    let before = f.sim.soap_log().len();
    assert!(control::join(&t, &f.houses(), &f.member, &f.leader).is_err());
    assert_eq!(t.writes.get(), 1);
    assert_eq!(f.writes_since(before), ["SetAVTransportURI"]);
}

#[test]
fn an_unapplied_join_remains_uncertain_without_replaying_it() {
    let f = Fixture::new(1);
    f.sim.ignore_action("Member", "SetAVTransportURI").unwrap();
    let t = Lost::new(&f, "SetAVTransportURI", false);
    let before = f.sim.soap_log().len();
    let result = control::join(&t, &f.houses(), &f.member, &f.leader);
    assert!(
        matches!(result, Err(CoreError::Proto(ProtoError::Network { ref detail, .. }))
        if detail == "group write acknowledgement lost")
    );
    assert_eq!(t.writes.get(), 1);
    assert_eq!(f.writes_since(before), ["SetAVTransportURI"]);
    assert_eq!(
        resolve_room(&f.houses(), "Member")
            .unwrap()
            .room
            .coordinator,
        f.member
    );
}

#[test]
fn a_definite_join_refusal_is_not_replaced_with_topology_success() {
    let f = Fixture::new(2);
    soap::join_group(&f.sim.lan(), f.member_host, &f.leader).unwrap();
    f.sim
        .upnp_fault("Member", "SetAVTransportURI", 800)
        .unwrap();
    let before = f.sim.soap_log().len();
    let result = control::join(&f.sim.lan(), &f.houses(), &f.member, &f.leader);
    assert!(matches!(
        result,
        Err(CoreError::Proto(ProtoError::SoapFault { code: 800, .. }))
    ));
    assert_eq!(f.writes_since(before), ["SetAVTransportURI"]);
}
