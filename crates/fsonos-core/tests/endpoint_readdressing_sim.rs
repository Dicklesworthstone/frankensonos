//! A DHCP repair needs the contacted player's own unambiguous identity.
//! Only topology/description replies are altered; commands and observations
//! use real S1/S2 simulator sockets.

use fsonos_core::{CoreError, HouseholdState, control, heal};
use fsonos_proto::control as soap;
use fsonos_proto::soap::{
    AV_TRANSPORT, ZONE_GROUP_TOPOLOGY, args_xml, envelope, parse_response, soap_action_header,
};
use fsonos_proto::ssdp::{Advert, SONOS_ST, description_url};
use fsonos_proto::topology::parse_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::{Cell, RefCell};
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;

#[path = "endpoint_readdressing_sim/wrapped_commands.rs"]
mod wrapped_commands;

const ORIGINAL: &str = "x-rincon-mp3radio://example.invalid/original";
const WANTED: &str = "x-rincon-mp3radio://example.invalid/requested";
const TRACKS: [&str; 2] = [
    "x-file-cifs://example.invalid/first.flac",
    "x-file-cifs://example.invalid/second.flac",
];

fn document(lan: &SimLan, host: IpAddr) -> String {
    let response = lan
        .soap_post(
            host,
            ZONE_GROUP_TOPOLOGY.control_path,
            &soap_action_header(&ZONE_GROUP_TOPOLOGY, "GetZoneGroupState"),
            &envelope(&ZONE_GROUP_TOPOLOGY, "GetZoneGroupState", ""),
        )
        .unwrap();
    parse_response(&response, "GetZoneGroupState")
        .unwrap()
        .require("ZoneGroupState")
        .unwrap()
        .to_owned()
}

fn topology_reply(document: &str) -> String {
    envelope(
        &ZONE_GROUP_TOPOLOGY,
        "GetZoneGroupStateResponse",
        &args_xml(&[("ZoneGroupState", document)]),
    )
}

fn writes(sim: &SimHandle, marker: usize) -> Vec<(String, String)> {
    sim.soap_log()[marker..]
        .iter()
        .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
        .map(|entry| (entry.room.clone(), entry.action.clone()))
        .collect()
}

struct Fixture {
    sim: SimHandle,
    remembered: Vec<HouseholdState>,
    target: PlayerId,
    peer_id: PlayerId,
    original_host: IpAddr,
    host: IpAddr,
    peer: IpAddr,
    stale: String,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1([
                SimPlayerSpec::new("Target", SimModel::Play5Gen1),
                SimPlayerSpec::new("Peer", SimModel::Play5Gen1),
            ])
        } else {
            builder.s2([
                SimPlayerSpec::new("Target", SimModel::One),
                SimPlayerSpec::new("Peer", SimModel::Play5Gen2),
            ])
        }
        .spawn()
        .unwrap();
        let original_host = sim.player("Target").unwrap().ip;
        let peer = sim.player("Peer").unwrap().ip;
        let target = PlayerId(sim.player("Target").unwrap().uuid.clone());
        let peer_id = PlayerId(sim.player("Peer").unwrap().uuid.clone());
        let lan = sim.lan();
        for address in [original_host, peer] {
            for track in TRACKS {
                soap::add_uri_to_queue(&lan, address, track, "", false).unwrap();
            }
            soap::set_av_transport_uri(&lan, address, ORIGINAL, "").unwrap();
            soap::play(&lan, address).unwrap();
            soap::set_volume(&lan, address, 31).unwrap();
        }
        let stale = document(&lan, original_host);
        let mut house = HouseholdState::default();
        house.apply_topology(&parse_zone_group_state(&stale).unwrap());
        let mut sim = sim;
        let host = sim.change_address("Target").unwrap();
        Self {
            sim,
            remembered: vec![house],
            target,
            peer_id,
            original_host,
            host,
            peer,
            stale,
        }
    }

    fn repaired(&self) -> Vec<HouseholdState> {
        let mut houses = self.remembered.clone();
        houses[0]
            .players
            .iter_mut()
            .find(|player| player.id == self.target)
            .unwrap()
            .ip = self.host;
        houses
    }

    fn evidence(&self) -> Evidence {
        Evidence {
            lan: self.sim.lan(),
            host: self.host,
            topology: self.stale.clone(),
            after_description: None,
            fault: DescriptionFault::None,
            descriptions: RefCell::new(Vec::new()),
            described: Cell::new(false),
            adverts: self
                .sim
                .players()
                .iter()
                .map(|player| Advert {
                    location: description_url(player.ip),
                    st: SONOS_ST.into(),
                    usn: None,
                    household: Some(player.household.clone()),
                    boot_seq: None,
                })
                .collect(),
        }
    }
}

#[derive(Clone, Copy, Debug)]
enum DescriptionFault {
    None,
    Unavailable,
    Malformed,
    DuplicateDevice,
    DuplicateUdn,
    NotRenderer,
}

struct Evidence {
    lan: SimLan,
    host: IpAddr,
    topology: String,
    after_description: Option<String>,
    fault: DescriptionFault,
    descriptions: RefCell<Vec<String>>,
    described: Cell<bool>,
    adverts: Vec<Advert>,
}

impl Transport for Evidence {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.lan.soap_post(host, path, action, body)?;
        if host == self.host && action.ends_with("#GetZoneGroupState\"") {
            let document = if self.described.get() {
                self.after_description.as_ref().unwrap_or(&self.topology)
            } else {
                &self.topology
            };
            return Ok(topology_reply(document));
        }
        Ok(response)
    }

    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        self.descriptions.borrow_mut().push(url.to_owned());
        if url != description_url(self.host) {
            return self.lan.http_get(url);
        }
        self.described.set(true);
        if matches!(self.fault, DescriptionFault::Unavailable) {
            return Err(ProtoError::Network {
                target: url.into(),
                detail: "description unavailable".into(),
            });
        }
        let mut body = self.lan.http_get(url)?;
        match self.fault {
            DescriptionFault::Malformed => body = "<root>".into(),
            DescriptionFault::DuplicateDevice => {
                let start = body.find("<device>").unwrap();
                let end = body.rfind("</device>").unwrap() + "</device>".len();
                let extra = body[start..end].to_owned();
                body.insert_str(body.rfind("</root>").unwrap(), &extra);
            }
            DescriptionFault::DuplicateUdn => {
                let end = body.find("</UDN>").unwrap() + "</UDN>".len();
                body.insert_str(end, "<UDN>uuid:ConflictingRoot</UDN>");
            }
            DescriptionFault::NotRenderer => {
                body = body.replace(AV_TRANSPORT.service_type, "urn:example:service:Other:1");
            }
            DescriptionFault::None | DescriptionFault::Unavailable => {}
        }
        Ok(body)
    }

    fn ssdp_search(&self, _: u8, _: Duration) -> Result<Vec<Advert>, ProtoError> {
        Ok(self.adverts.clone())
    }
}

#[derive(Clone, Copy, Debug)]
enum Command {
    Volume,
    Source,
    Queue,
}

impl Command {
    fn run(self, fixture: &Fixture, transport: &Evidence) -> Result<(), CoreError> {
        let houses = fixture.repaired();
        match self {
            Self::Volume => control::set_volume(transport, &houses, &fixture.target, 19).map(drop),
            Self::Source => control::play_uri(transport, &houses, &fixture.target, WANTED, ""),
            Self::Queue => {
                control::play_queue_from_in_order(transport, &houses, &fixture.target, 2)
            }
        }
    }
}

#[test]
fn a_fresh_description_finishes_dhcp_recovery_without_writing_to_the_old_address() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let t = f.evidence();
        let mut houses = f.remembered.clone();
        let marker = f.sim.soap_log().len();
        let (_, recovered) = heal::readdressing(&t, &[], &mut houses, &f.target, |households| {
            control::set_volume(&t, households, &f.target, 19)
        })
        .unwrap();
        assert_eq!(
            recovered,
            heal::Recovery::Readdressed {
                player: f.target.clone(),
                from: f.original_host,
                to: f.host,
            }
        );
        assert_eq!(
            writes(&f.sim, marker),
            [("Target".into(), "SetVolume".into())]
        );
        assert_eq!(soap::get_volume(&f.sim.lan(), f.host).unwrap(), 19);
        assert_eq!(soap::get_volume(&f.sim.lan(), f.peer).unwrap(), 31);
    }
}

#[test]
fn direct_and_ordered_queue_starts_keep_the_repaired_binding_through_readback() {
    for generation in [1, 2] {
        for command in [Command::Source, Command::Queue] {
            let f = Fixture::new(generation);
            let t = f.evidence();
            let marker = f.sim.soap_log().len();
            command.run(&f, &t).unwrap();
            let expected = match command {
                Command::Source => vec!["SetAVTransportURI", "Play"],
                Command::Queue => vec!["SetAVTransportURI", "SetPlayMode", "Seek", "Play"],
                Command::Volume => unreachable!(),
            };
            assert_eq!(
                writes(&f.sim, marker),
                expected
                    .into_iter()
                    .map(|action| ("Target".into(), action.into()))
                    .collect::<Vec<_>>()
            );
            let lan = f.sim.lan();
            assert_eq!(
                soap::get_transport_info(&lan, f.host).unwrap().state,
                TransportState::Playing
            );
            assert_eq!(soap::get_media_info(&lan, f.peer).unwrap().uri, ORIGINAL);
        }
    }
}

#[test]
fn fallback_is_bounded_fresh_and_keeps_destination_receipts_stable_as_location_catches_up() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let t = f.evidence();
        for _ in 0..2 {
            control::require_player(&t, f.host, &f.target).unwrap();
        }
        assert_eq!(
            *t.descriptions.borrow(),
            [description_url(f.host), description_url(f.host)]
        );
        let mut catching_up = f.evidence();
        catching_up.after_description = Some(document(&f.sim.lan(), f.host));
        let repaired = control::SourceDestination::read(&catching_up, f.host, &f.target).unwrap();
        let fresh = control::SourceDestination::read(&f.sim.lan(), f.host, &f.target).unwrap();
        assert_eq!(repaired, fresh);
        assert_eq!(
            *catching_up.descriptions.borrow(),
            [description_url(f.host)]
        );
        control::require_coordinator(&catching_up, f.host, &f.target).unwrap();
        assert_eq!(
            catching_up.descriptions.borrow().len(),
            1,
            "matching topology requires no additional description"
        );
    }
}

#[test]
fn unavailable_ambiguous_or_nonrenderer_descriptions_authorize_no_writes() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        for fault in [
            DescriptionFault::Unavailable,
            DescriptionFault::Malformed,
            DescriptionFault::DuplicateDevice,
            DescriptionFault::DuplicateUdn,
            DescriptionFault::NotRenderer,
        ] {
            for command in [Command::Volume, Command::Source, Command::Queue] {
                let mut t = f.evidence();
                t.fault = fault;
                let marker = f.sim.soap_log().len();
                assert!(
                    command.run(&f, &t).is_err(),
                    "S{generation} {fault:?} {command:?}"
                );
                assert!(writes(&f.sim, marker).is_empty());
                assert_eq!(*t.descriptions.borrow(), [description_url(f.host)]);
            }
        }
    }
}

#[test]
fn a_reassigned_endpoint_cannot_borrow_the_original_players_stale_topology() {
    for generation in [1, 2] {
        let mut f = Fixture::new(generation);
        let IpAddr::V4(reused) = f.host else {
            unreachable!();
        };
        f.sim.change_address("Target").unwrap();
        f.sim
            .move_to("Peer", reused, SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .unwrap();
        let t = f.evidence();
        for command in [Command::Volume, Command::Source, Command::Queue] {
            let marker = f.sim.soap_log().len();
            assert!(matches!(
                command.run(&f, &t),
                Err(CoreError::TargetChanged(_))
            ));
            assert!(writes(&f.sim, marker).is_empty());
        }
        assert_eq!(
            soap::get_media_info(&f.sim.lan(), f.host).unwrap().uri,
            ORIGINAL
        );
    }
}

fn set_member_attribute(document: &str, player: &PlayerId, name: &str, value: &str) -> String {
    let mut document = document.to_owned();
    let start = document
        .find(&format!("<ZoneGroupMember UUID=\"{}\"", player.0))
        .unwrap();
    let end = start + document[start..].find('>').unwrap();
    let header = &document[start..end];
    let prefix = format!("{name}=\"");
    if let Some(at) = header.find(&prefix) {
        let at = start + at + prefix.len();
        let end = at + document[at..].find('"').unwrap();
        document.replace_range(at..end, value);
    } else {
        let at = if document.as_bytes()[end - 1] == b'/' {
            end - 1
        } else {
            end
        };
        document.insert_str(at, &format!(" {name}=\"{value}\""));
    }
    document
}

fn satellite(document: &str, player: &PlayerId, child: &str) -> String {
    let mut document = document.to_owned();
    let start = document
        .find(&format!("<ZoneGroupMember UUID=\"{}\"", player.0))
        .unwrap();
    let end = start + document[start..].find("/>").unwrap();
    document.replace_range(end..end + 2, &format!(">{child}</ZoneGroupMember>"));
    document
}

#[test]
fn conflicting_or_hidden_member_evidence_never_reaches_description_repair() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let claimed =
            set_member_attribute(&f.stale, &f.peer_id, "Location", &description_url(f.host));
        let duplicate_address = set_member_attribute(
            &f.stale,
            &f.peer_id,
            "Location",
            &description_url(f.original_host),
        );
        let duplicate_id = satellite(
            &f.stale,
            &f.peer_id,
            &format!(
                "<Satellite UUID=\"{}\" Location=\"http://192.0.2.250:1400/x\"/>",
                f.target.0
            ),
        );
        let satellite_claim = satellite(
            &f.stale,
            &f.peer_id,
            &format!(
                "<Satellite UUID=\"AddressOwner\" Location=\"{}\"/>",
                description_url(f.host),
            ),
        );
        let bridge_claim = f.stale.replace(
            "</ZoneGroups>",
            &format!(
                "<ZoneGroup Coordinator=\"BridgeOwner\"><ZoneGroupMember UUID=\"BridgeOwner\" \
                 Location=\"{}\" IsZoneBridge=\"1\"/></ZoneGroup></ZoneGroups>",
                description_url(f.host),
            ),
        );
        let bridge = set_member_attribute(&f.stale, &f.target, "IsZoneBridge", "1");
        let missing = {
            let start = f
                .stale
                .find(&format!("<ZoneGroupMember UUID=\"{}\"", f.target.0))
                .unwrap();
            let begin = f.stale[..start].rfind("<ZoneGroup ").unwrap();
            let end = start + f.stale[start..].find("</ZoneGroup>").unwrap() + "</ZoneGroup>".len();
            let mut result = f.stale.clone();
            result.replace_range(begin..end, "");
            result
        };
        let nested = format!(
            "<Satellite UUID=\"OuterSatellite\" Location=\"http://192.0.2.250:1400/x\">\
             <Satellite UUID=\"{}\" Location=\"{}\"/></Satellite>",
            f.target.0,
            description_url(f.host),
        );
        let live = document(&f.sim.lan(), f.host);
        for topology in [
            claimed,
            duplicate_address,
            duplicate_id,
            satellite_claim,
            bridge_claim,
            bridge,
            missing,
            satellite(&f.stale, &f.peer_id, &nested),
            satellite(&live, &f.peer_id, &nested),
        ] {
            for command in [Command::Volume, Command::Source, Command::Queue] {
                let mut t = f.evidence();
                t.topology = topology.clone();
                let marker = f.sim.soap_log().len();
                assert!(command.run(&f, &t).is_err(), "S{generation} {command:?}");
                assert!(writes(&f.sim, marker).is_empty());
                assert!(t.descriptions.borrow().is_empty());
            }
        }
    }
}

#[test]
fn fresh_identity_cannot_authorize_a_group_or_boot_change_during_its_read() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let boot = set_member_attribute(&f.stale, &f.target, "BootSeq", "999999");
        let address = set_member_attribute(
            &f.stale,
            &f.peer_id,
            "Location",
            "http://192.0.2.249:1400/x",
        );
        let moved_again =
            set_member_attribute(&f.stale, &f.target, "Location", "http://192.0.2.249:1400/x");
        soap::join_group(&f.sim.lan(), f.peer, &f.target).unwrap();
        let group = document(&f.sim.lan(), f.host);
        f.sim.reelect_coordinator("Target").unwrap();
        let coordinator = document(&f.sim.lan(), f.host);
        for after in [boot, address, moved_again, group, coordinator] {
            for command in [Command::Volume, Command::Source, Command::Queue] {
                let mut t = f.evidence();
                t.after_description = Some(after.clone());
                let marker = f.sim.soap_log().len();
                assert!(command.run(&f, &t).is_err(), "S{generation} {command:?}");
                assert!(writes(&f.sim, marker).is_empty());
            }
        }
    }
}

#[test]
fn a_repaired_member_address_does_not_promote_it_to_group_coordinator() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        soap::join_group(&f.sim.lan(), f.host, &f.peer_id).unwrap();
        let grouped = document(&f.sim.lan(), f.host);
        let mut t = f.evidence();
        t.topology = set_member_attribute(
            &grouped,
            &f.target,
            "Location",
            &description_url(f.original_host),
        );
        control::require_player(&t, f.host, &f.target).unwrap();
        for command in [Command::Source, Command::Queue] {
            let marker = f.sim.soap_log().len();
            assert!(matches!(
                command.run(&f, &t),
                Err(CoreError::TargetChanged(_))
            ));
            assert!(writes(&f.sim, marker).is_empty());
        }
    }
}
