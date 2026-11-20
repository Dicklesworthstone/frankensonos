//! Room volume and mute must follow physical identity; group volume must
//! still belong to the same coordinator after taking its relative-level snapshot.

use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::cell::{Cell, RefCell};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};

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

    fn group_peer(&mut self) {
        let lan = self.sim.lan();
        soap::join_group(&lan, self.peer, &self.target).unwrap();
        self.houses[0].apply_topology(&get_zone_group_state(&lan, self.host).unwrap());
    }

    fn join_peer(&mut self) {
        let lan = self.sim.lan();
        let peer = PlayerId(self.sim.player("Peer").unwrap().uuid.clone());
        soap::join_group(&lan, self.host, &peer).unwrap();
        self.houses[0].apply_topology(&get_zone_group_state(&lan, self.host).unwrap());
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
    Room,
    RelativeRoom,
    Group,
    RelativeGroup,
    Mute,
}

const COMMANDS: [Command; 5] = [
    Command::Room,
    Command::RelativeRoom,
    Command::Group,
    Command::RelativeGroup,
    Command::Mute,
];

impl Command {
    fn run(
        self,
        t: &impl Transport,
        houses: &[HouseholdState],
        target: &PlayerId,
    ) -> Result<(), CoreError> {
        match self {
            Self::Room => control::set_volume(t, houses, target, 30).map(drop),
            Self::RelativeRoom => control::adjust_volume(t, houses, target, 5).map(drop),
            Self::Group => control::set_group_volume(t, houses, target, 30).map(drop),
            Self::RelativeGroup => control::adjust_group_volume(t, houses, target, 5).map(drop),
            Self::Mute => control::set_mute(t, houses, target, true),
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Room => "SetVolume",
            Self::RelativeRoom => "SetRelativeVolume",
            Self::Group => "SetGroupVolume",
            Self::RelativeGroup => "SetRelativeGroupVolume",
            Self::Mute => "SetMute",
        }
    }

    fn is_group(self) -> bool {
        matches!(self, Self::Group | Self::RelativeGroup)
    }

    fn level(self) -> u8 {
        match self {
            Self::Room | Self::Group => 30,
            Self::RelativeRoom | Self::RelativeGroup => 25,
            Self::Mute => 20,
        }
    }
}

#[test]
fn a_stale_room_address_cannot_change_its_peers_volume_or_mute() {
    for generation in [1, 2] {
        for grouped in [false, true] {
            let mut f = Fixture::new(generation);
            if grouped {
                f.group_peer();
            }
            let lan = f.sim.lan();
            for command in COMMANDS {
                let before = f.sim.soap_log().len();
                let error = command.run(&lan, &f.stale(), &f.target).unwrap_err();
                assert!(
                    matches!(error, CoreError::TargetChanged(_)),
                    "S{generation} {command:?}: {error}"
                );
                let actions: Vec<_> = f.sim.soap_log()[before..]
                    .iter()
                    .map(|entry| entry.action.clone())
                    .collect();
                assert_eq!(actions, ["GetZoneGroupState"]);
                for host in [f.host, f.peer] {
                    assert_eq!(soap::get_volume(&lan, host).unwrap(), 20);
                    assert!(!soap::get_mute(&lan, host).unwrap());
                }
            }
        }
    }
}

#[test]
fn matching_room_and_group_endpoints_apply_each_change_once() {
    for generation in [1, 2] {
        for grouped in [false, true] {
            for command in COMMANDS {
                let mut f = Fixture::new(generation);
                if grouped {
                    f.group_peer();
                }
                let lan = f.sim.lan();
                let before = f.sim.soap_log().len();
                command.run(&lan, &f.houses, &f.target).unwrap();
                let actions: Vec<_> = f.sim.soap_log()[before..]
                    .iter()
                    .map(|entry| (entry.room.clone(), entry.action.clone()))
                    .collect();
                let expected = if command.is_group() {
                    vec![
                        "GetZoneGroupState",
                        "SnapshotGroupVolume",
                        "GetZoneGroupState",
                        command.action(),
                    ]
                } else {
                    vec!["GetZoneGroupState", command.action()]
                };
                let expected: Vec<_> = expected
                    .into_iter()
                    .map(|action| ("Target".to_string(), action.to_string()))
                    .collect();
                assert_eq!(actions, expected, "S{generation} {command:?}");
                assert_eq!(soap::get_volume(&lan, f.host).unwrap(), command.level());
                assert_eq!(
                    soap::get_mute(&lan, f.host).unwrap(),
                    matches!(command, Command::Mute)
                );
                let peer_level = if grouped && command.is_group() {
                    command.level()
                } else {
                    20
                };
                assert_eq!(soap::get_volume(&lan, f.peer).unwrap(), peer_level);
                assert!(!soap::get_mute(&lan, f.peer).unwrap());
            }
        }
    }
}

#[test]
fn room_changes_accept_group_members_while_group_changes_require_the_coordinator() {
    for generation in [1, 2] {
        for command in COMMANDS {
            let mut f = Fixture::new(generation);
            f.join_peer();
            let lan = f.sim.lan();
            let before = f.sim.soap_log().len();
            let result = command.run(&lan, &f.houses, &f.target);
            if command.is_group() {
                assert!(matches!(result, Err(CoreError::TargetChanged(_))));
                let actions: Vec<_> = f.sim.soap_log()[before..]
                    .iter()
                    .map(|entry| entry.action.clone())
                    .collect();
                assert_eq!(actions, ["GetZoneGroupState"]);
                assert_eq!(soap::get_volume(&lan, f.host).unwrap(), 20);
            } else {
                result.unwrap();
                assert_eq!(soap::get_volume(&lan, f.host).unwrap(), command.level());
                assert_eq!(
                    soap::get_mute(&lan, f.host).unwrap(),
                    matches!(command, Command::Mute)
                );
            }
            assert_eq!(soap::get_volume(&lan, f.peer).unwrap(), 20);
            assert!(!soap::get_mute(&lan, f.peer).unwrap());
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum SnapshotChange {
    AddressReuse,
    Coordinator,
    Unreadable,
}

struct AfterSnapshot {
    sim: RefCell<SimHandle>,
    inner: SimLan,
    change: SnapshotChange,
    changed: Cell<bool>,
    actions: RefCell<Vec<String>>,
}

impl AfterSnapshot {
    fn new(sim: SimHandle, change: SnapshotChange) -> Self {
        Self {
            inner: sim.lan(),
            sim: RefCell::new(sim),
            change,
            changed: Cell::new(false),
            actions: RefCell::new(Vec::new()),
        }
    }
}

impl Transport for AfterSnapshot {
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
        self.actions.borrow_mut().push(name.into());
        if name == "GetZoneGroupState"
            && self.changed.get()
            && matches!(self.change, SnapshotChange::Unreadable)
        {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected unreadable topology after group snapshot".into(),
            });
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if name == "SnapshotGroupVolume" && !self.changed.replace(true) {
            match self.change {
                SnapshotChange::AddressReuse => {
                    let IpAddr::V4(old) = host else {
                        panic!("synthetic players use IPv4");
                    };
                    let mut sim = self.sim.borrow_mut();
                    sim.change_address("Target").unwrap();
                    sim.move_to("Peer", old, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                        .unwrap();
                }
                SnapshotChange::Coordinator => {
                    self.sim.borrow().reelect_coordinator("Target").unwrap();
                }
                SnapshotChange::Unreadable => {}
            }
        }
        Ok(response)
    }
}

#[test]
fn a_group_snapshot_never_authorizes_a_write_to_a_changed_or_unreadable_endpoint() {
    for generation in [1, 2] {
        for command in [Command::Group, Command::RelativeGroup] {
            for change in [
                SnapshotChange::AddressReuse,
                SnapshotChange::Coordinator,
                SnapshotChange::Unreadable,
            ] {
                let mut f = Fixture::new(generation);
                f.group_peer();
                let t = AfterSnapshot::new(f.sim, change);
                let error = command.run(&t, &f.houses, &f.target).unwrap_err();
                assert!(t.changed.get(), "S{generation} {command:?} {change:?}");
                match change {
                    SnapshotChange::Unreadable => {
                        assert!(matches!(
                            error,
                            CoreError::Proto(ProtoError::Network { .. })
                        ));
                    }
                    _ => assert!(matches!(error, CoreError::TargetChanged(_))),
                }
                assert_eq!(
                    *t.actions.borrow(),
                    [
                        "GetZoneGroupState",
                        "SnapshotGroupVolume",
                        "GetZoneGroupState"
                    ]
                );
                // A snapshot has no audible effect and remains safe to redo
                // after address recovery. Neither room received a volume Set.
                for room in ["Target", "Peer"] {
                    let host = t.sim.borrow().player(room).unwrap().ip;
                    assert_eq!(soap::get_volume(&t.inner, host).unwrap(), 20);
                    assert!(!soap::get_mute(&t.inner, host).unwrap());
                }
            }
        }
    }
}

const PLAYER_TOPOLOGY: &str = r#"<ZoneGroupState><ZoneGroups>
    <ZoneGroup Coordinator="RINCON_PRIMARY" ID="group">
        <ZoneGroupMember UUID="RINCON_PRIMARY" Location="http://192.0.2.1:1400/x">
            <Satellite UUID="RINCON_SATELLITE" Location="http://192.0.2.3:1400/x" Invisible="1"/>
        </ZoneGroupMember>
        <ZoneGroupMember UUID="RINCON_MEMBER" Location="http://192.0.2.2:1400/x" Invisible="1"/>
    </ZoneGroup>
    <ZoneGroup Coordinator="RINCON_BRIDGE" ID="bridge">
        <ZoneGroupMember UUID="RINCON_BRIDGE" Location="http://192.0.2.4:1400/x" IsZoneBridge="1"/>
    </ZoneGroup>
</ZoneGroups></ZoneGroupState>"#;

struct PlayerTopology(String);

impl Transport for PlayerTopology {
    fn soap_post(&self, _: IpAddr, _: &str, action: &str, _: &str) -> Result<String, ProtoError> {
        assert!(action.ends_with("#GetZoneGroupState\""), "{action}");
        let topology = self
            .0
            .replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;");
        Ok(format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
             <u:GetZoneGroupStateResponse xmlns:u=\"urn:x\"><ZoneGroupState>{topology}\
             </ZoneGroupState></u:GetZoneGroupStateResponse></s:Body></s:Envelope>"
        ))
    }
}

#[test]
fn player_identity_accepts_bonded_players_but_rejects_bridges_and_other_addresses() {
    let t = PlayerTopology(PLAYER_TOPOLOGY.into());
    for (player, address) in [
        ("RINCON_PRIMARY", "192.0.2.1"),
        ("RINCON_MEMBER", "192.0.2.2"),
        ("RINCON_SATELLITE", "192.0.2.3"),
    ] {
        control::require_player(&t, address.parse().unwrap(), &PlayerId(player.into())).unwrap();
    }
    for (player, address) in [
        ("RINCON_MEMBER", "192.0.2.1"),
        ("RINCON_SATELLITE", "192.0.2.2"),
        ("RINCON_MISSING", "192.0.2.1"),
        ("RINCON_BRIDGE", "192.0.2.4"),
    ] {
        assert!(matches!(
            control::require_player(&t, address.parse().unwrap(), &PlayerId(player.into())),
            Err(CoreError::TargetChanged(_))
        ));
    }
}

#[test]
fn player_identity_requires_coherent_topology_even_when_the_requested_player_matches() {
    for topology in [
        PLAYER_TOPOLOGY.replace("192.0.2.3", "192.0.2.2"),
        PLAYER_TOPOLOGY.replace("RINCON_SATELLITE", "RINCON_MEMBER"),
        PLAYER_TOPOLOGY.replace(
            "Coordinator=\"RINCON_PRIMARY\"",
            "Coordinator=\"RINCON_MISSING\"",
        ),
    ] {
        assert!(matches!(
            control::require_player(
                &PlayerTopology(topology),
                "192.0.2.1".parse().unwrap(),
                &PlayerId("RINCON_PRIMARY".into())
            ),
            Err(CoreError::Proto(ProtoError::Malformed(_)))
        ));
    }
}
