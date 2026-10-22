//! Group capture follows real topology rather than a cached room list.

use fsonos_core::HouseholdState;
use fsonos_core::snapshot::{self, ZoneSnapshot};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::cell::{Cell, RefCell};
use std::net::IpAddr;

struct Fixture {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
}

impl Fixture {
    fn new(generation: u8, paired: bool) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let lead = if paired {
            SimPlayerSpec::pair("Room", model)
        } else {
            SimPlayerSpec::new("Room", model)
        };
        let players = [
            lead,
            SimPlayerSpec::new("Member", model),
            SimPlayerSpec::new("Other", model),
            SimPlayerSpec::new("Spare", model),
        ];
        let builder = SimHousehold::builder();
        let builder = if generation == 1 {
            builder.s1(players)
        } else {
            builder.s2(players)
        };
        let sim = builder.spawn().unwrap();
        let lan = sim.lan();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, sim.player("Room").unwrap().ip).unwrap());
        Self {
            sim,
            lan,
            houses: vec![house],
        }
    }

    fn id(&self, room: &str) -> PlayerId {
        PlayerId(self.sim.player(room).unwrap().uuid.clone())
    }

    fn host(&self, room: &str) -> IpAddr {
        self.sim.player(room).unwrap().ip
    }

    fn join(&self, room: &str, lead: &str) {
        soap::join_group(&self.lan, self.host(room), &self.id(lead)).unwrap();
    }

    fn leave(&self, room: &str) {
        // The sim models the player's self-join: it leaves even with a 402.
        let _ = soap::join_group(&self.lan, self.host(room), &self.id(room));
    }

    fn capture<T: Transport>(&self, t: &T) -> Result<ZoneSnapshot, fsonos_core::CoreError> {
        snapshot::capture(t, &self.houses, &self.id("Room"), 123)
    }
}

struct After<'a, F> {
    lan: &'a SimLan,
    hook: F,
    reads: RefCell<Vec<(IpAddr, String)>>,
}

impl<'a, F> After<'a, F> {
    fn new(lan: &'a SimLan, hook: F) -> Self {
        Self {
            lan,
            hook,
            reads: RefCell::new(Vec::new()),
        }
    }

    fn count(&self, action: &str) -> usize {
        self.reads
            .borrow()
            .iter()
            .filter(|(_, a)| a == action)
            .count()
    }

    fn assert_read_only(&self) {
        assert!(
            self.reads
                .borrow()
                .iter()
                .all(|(_, a)| a.starts_with("Get") || a == "Browse")
        );
    }
}

impl<F: Fn(IpAddr, &str)> Transport for After<'_, F> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        self.reads.borrow_mut().push((host, name.to_string()));
        let reply = self.lan.soap_post(host, path, action, body);
        (self.hook)(host, name);
        reply
    }
}

#[test]
fn fresh_members_and_their_levels_replace_a_stale_solo_snapshot() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation, false);
        let member = f.id("Member");
        // The caller's discovery cache does not even know this player yet.
        f.houses[0].players.retain(|p| p.id != member);
        f.join("Member", "Room");
        soap::set_volume(&f.lan, f.host("Member"), 42).unwrap();
        soap::set_mute(&f.lan, f.host("Member"), true).unwrap();
        let t = After::new(&f.lan, |_: IpAddr, _: &str| {});
        let snap = f.capture(&t).unwrap();
        assert_eq!(snap.members, [f.id("Room"), member.clone()]);
        let level = snap.levels.iter().find(|l| l.player == member).unwrap();
        assert_eq!((level.volume, level.mute), (42, true));
        assert_eq!(t.count("GetZoneGroupState"), 2);
        t.assert_read_only();
    }
}

#[test]
fn a_join_during_capture_restarts_with_all_member_levels() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        let once = Cell::new(false);
        let t = After::new(&f.lan, |host: IpAddr, action: &str| {
            if host == f.host("Room") && action == "GetVolume" && !once.replace(true) {
                f.join("Member", "Room");
            }
        });
        let snap = f.capture(&t).unwrap();
        assert_eq!(snap.members, [f.id("Room"), f.id("Member")]);
        assert_eq!(snap.levels.len(), 2);
        assert_eq!(t.count("GetZoneGroupState"), 4);
        t.assert_read_only();
    }
}

#[test]
fn a_room_that_leaves_is_not_saved_as_a_member_of_the_old_group() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        f.join("Member", "Room");
        let once = Cell::new(false);
        let t = After::new(&f.lan, |host: IpAddr, action: &str| {
            if host == f.host("Member") && action == "GetMute" && !once.replace(true) {
                f.leave("Member");
            }
        });
        let snap = f.capture(&t).unwrap();
        assert_eq!(snap.members, [f.id("Room")]);
        assert_eq!(snap.levels.len(), 1);
        assert_eq!(t.count("GetZoneGroupState"), 4);
        t.assert_read_only();
    }
}

#[test]
fn a_former_coordinator_is_not_silently_retargeted() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        f.join("Room", "Other");
        let t = After::new(&f.lan, |_: IpAddr, _: &str| {});
        assert!(f.capture(&t).is_err());
        assert_eq!(t.count("GetZoneGroupState"), 1);
        assert_eq!(t.count("GetMediaInfo"), 0);
        t.assert_read_only();
    }
}

#[test]
fn another_groups_changes_do_not_restart_the_requested_capture() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        let once = Cell::new(false);
        let t = After::new(&f.lan, |_: IpAddr, action: &str| {
            if action == "GetVolume" && !once.replace(true) {
                f.join("Spare", "Other");
            }
        });
        let snap = f.capture(&t).unwrap();
        assert_eq!(snap.members, [f.id("Room")]);
        assert_eq!(t.count("GetZoneGroupState"), 2);
        assert_eq!(t.count("GetVolume"), 1);
        t.assert_read_only();
    }
}

#[test]
fn bonded_players_stay_one_logical_room_with_one_level_read() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, true);
        f.join("Member", "Room");
        let t = After::new(&f.lan, |_: IpAddr, _: &str| {});
        let snap = f.capture(&t).unwrap();
        assert_eq!(snap.members, [f.id("Room"), f.id("Member")]);
        assert_eq!(snap.levels.len(), 2);
        assert_eq!(t.count("GetVolume"), 2);
        let secondary = f
            .sim
            .players()
            .iter()
            .find(|p| p.room == "Room" && p.uuid != f.id("Room").0)
            .unwrap();
        assert!(
            !t.reads
                .borrow()
                .iter()
                .any(|(host, a)| { *host == secondary.ip && (a == "GetVolume" || a == "GetMute") })
        );
        t.assert_read_only();
    }
}

#[test]
fn unavailable_topology_never_falls_back_to_cached_membership() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        f.sim.upnp_fault("Room", "GetZoneGroupState", 701).unwrap();
        let t = After::new(&f.lan, |_: IpAddr, _: &str| {});
        assert!(f.capture(&t).is_err());
        assert_eq!(t.count("GetZoneGroupState"), 1);
        assert_eq!(t.count("GetVolume"), 0);
        t.assert_read_only();
    }
}

#[test]
fn continually_changing_groups_exhaust_only_three_read_attempts() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        let joined = Cell::new(false);
        let t = After::new(&f.lan, |host: IpAddr, action: &str| {
            if host == f.host("Room") && action == "GetVolume" {
                if joined.get() {
                    f.leave("Member");
                } else {
                    f.join("Member", "Room");
                }
                joined.set(!joined.get());
            }
        });
        assert!(f.capture(&t).is_err());
        assert_eq!(t.count("GetZoneGroupState"), 6);
        t.assert_read_only();
    }
}

#[test]
fn a_missing_media_count_uses_the_verified_queue_instead() {
    struct NoMediaCount<'a>(&'a SimLan);
    impl Transport for NoMediaCount<'_> {
        fn soap_post(
            &self,
            host: IpAddr,
            path: &str,
            action: &str,
            body: &str,
        ) -> Result<String, ProtoError> {
            let reply = self.0.soap_post(host, path, action, body)?;
            if action.ends_with("#GetMediaInfo\"") {
                assert!(reply.contains("<NrTracks>1</NrTracks>"));
                Ok(reply.replace("<NrTracks>1</NrTracks>", ""))
            } else {
                Ok(reply)
            }
        }
    }
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        soap::add_uri_to_queue(
            &f.lan,
            f.host("Room"),
            "x-file-cifs://example.invalid/track.flac",
            "",
            false,
        )
        .unwrap();
        soap::play_from_queue(&f.lan, f.host("Room"), &f.id("Room")).unwrap();
        soap::play(&f.lan, f.host("Room")).unwrap();
        let snap = f.capture(&NoMediaCount(&f.lan)).unwrap();
        assert!(matches!(
            snap.source,
            snapshot::SnapshotSource::Queue { track: 1, .. }
        ));
    }
}
