//! Native line-in works on S1/S2, retains exact physical input identities and
//! never replays writes or adopts a changed destination after preparation.

use fsonos_core::audio_inputs::{self, PreparedLineIn};
use fsonos_core::{CoreError, HouseholdState};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, PlayerId, TransportState};
use std::cell::RefCell;
use std::net::IpAddr;
use std::time::Duration;

const OLD: &str = "x-rincon-mp3radio://example.invalid/destination.mp3";
const INPUT: &str = "x-rincon-mp3radio://example.invalid/input-room.mp3";
const OWNER: &str = "x-rincon-mp3radio://example.invalid/owner.mp3";
const WRITES: [&str; 2] = ["SetAVTransportURI", "Play"];

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    destination: PlayerId,
    host: IpAddr,
    input: PlayerId,
    input_primary: IpAddr,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let input_model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::Play5Gen2
        };
        let rooms = [
            SimPlayerSpec::new("Listening", SimModel::Play1),
            SimPlayerSpec::pair("Turntable", input_model),
            SimPlayerSpec::new("Peer", SimModel::Play1),
        ];
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(rooms)
        } else {
            builder.s2(rooms)
        }
        .spawn()
        .unwrap();
        let destination = sim.player("Listening").unwrap();
        let host = destination.ip;
        let destination_id = PlayerId(destination.uuid.clone());
        let input_primary = sim.player("Turntable").unwrap().ip;
        let input = PlayerId(
            sim.players()
                .iter()
                .find(|p| p.hidden && p.room == "Turntable")
                .unwrap()
                .uuid
                .clone(),
        );
        let lan = sim.lan();
        let mut house = HouseholdState {
            id: Some(HouseholdId(destination.household.clone())),
            ..Default::default()
        };
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        for (address, uri) in [(host, OLD), (input_primary, INPUT)] {
            soap::add_uri_to_queue(&lan, address, uri, "", false).unwrap();
            soap::set_av_transport_uri(&lan, address, uri, "").unwrap();
            soap::play(&lan, address).unwrap();
        }
        Self {
            sim,
            houses: vec![house],
            destination: destination_id,
            host,
            input,
            input_primary,
        }
    }

    fn prepare(&self) -> PreparedLineIn {
        audio_inputs::prepare(
            &self.sim.lan(),
            &self.houses,
            &self.destination,
            &self.input,
        )
        .unwrap()
    }

    fn assert_queues_and_input_preserved(&self) {
        for (host, expected) in [(self.host, OLD), (self.input_primary, INPUT)] {
            let queue = browse_all(&self.sim.lan(), host, QUEUE).unwrap();
            assert_eq!(queue.len(), 1);
            assert_eq!(queue[0].res.as_ref().unwrap().uri, expected);
        }
        assert_eq!(
            soap::get_media_info(&self.sim.lan(), self.input_primary)
                .unwrap()
                .uri,
            INPUT
        );
    }
}

#[derive(Clone, Copy)]
enum Fault {
    None,
    Lost,
    Malformed,
    Before,
    Rejected,
    OwnerAfterAck,
    RegroupAfterAck,
    RebootInputAfterAck,
    ReplacedDestinationAddress,
}

struct Replies<'a> {
    inner: SimLan,
    sim: &'a SimHandle,
    input: &'a PlayerId,
    fault: Fault,
    stage: Option<&'static str>,
    calls: RefCell<Vec<(IpAddr, String)>>,
}

impl<'a> Replies<'a> {
    fn new(f: &'a Fixture, fault: Fault, stage: Option<&'static str>) -> Self {
        Self {
            inner: f.sim.lan(),
            sim: &f.sim,
            input: &f.input,
            fault,
            stage,
            calls: RefCell::new(Vec::new()),
        }
    }

    fn lost(host: IpAddr) -> ProtoError {
        ProtoError::Network {
            target: host.to_string(),
            detail: "injected lost line-in acknowledgement".into(),
        }
    }

    fn writes(&self) -> Vec<String> {
        self.calls
            .borrow()
            .iter()
            .filter(|(_, name)| WRITES.contains(&name.as_str()))
            .map(|(_, name)| name.clone())
            .collect()
    }
}

impl Transport for Replies<'_> {
    fn http_get(&self, url: &str) -> Result<String, ProtoError> {
        let response = self.inner.http_get(url)?;
        let destination = self.sim.player("Listening").unwrap();
        if matches!(self.fault, Fault::ReplacedDestinationAddress)
            && url == fsonos_proto::ssdp::description_url(destination.ip)
        {
            return Ok(response.replace(
                &format!("<UDN>uuid:{}</UDN>", destination.uuid),
                "<UDN>uuid:RINCON_REPLACEMENT</UDN>",
            ));
        }
        Ok(response)
    }

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
        self.calls.borrow_mut().push((host, name.into()));
        let selected = WRITES.contains(&name) && self.stage.is_none_or(|stage| stage == name);
        if selected && matches!(self.fault, Fault::Before) {
            return Err(Self::lost(host));
        }
        if selected && matches!(self.fault, Fault::Rejected) {
            return Err(ProtoError::SoapFault {
                code: 701,
                reason: "injected definite refusal".into(),
            });
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if !selected {
            return Ok(response);
        }
        match self.fault {
            Fault::Lost => return Err(Self::lost(host)),
            Fault::Malformed => return Ok("<incomplete-reply>".into()),
            Fault::OwnerAfterAck => {
                soap::set_av_transport_uri(&self.inner, host, OWNER, "")?;
                soap::play(&self.inner, host)?;
            }
            Fault::RegroupAfterAck => {
                let destination = self.sim.player("Listening").unwrap();
                let peer = self.sim.player("Peer").unwrap();
                soap::join_group(&self.inner, peer.ip, &PlayerId(destination.uuid.clone()))?;
            }
            Fault::RebootInputAfterAck => {
                self.sim.reboot(&self.input.0, Duration::ZERO).unwrap();
            }
            _ => {}
        }
        Ok(response)
    }
}

#[test]
fn both_generations_discover_and_play_the_exact_bonded_input_without_touching_queues() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let catalog = audio_inputs::list(&f.sim.lan(), &f.houses, &f.destination).unwrap();
        assert_eq!(catalog.sources.len(), 2, "only the two physical jacks");
        assert!(catalog.unavailable.is_empty());
        assert!(
            catalog
                .sources
                .iter()
                .any(|source| source.player == f.input)
        );
        assert!(catalog.sources.iter().all(|source| {
            source.name == "Turntable line-in" && source.icon == "AudioComponent"
        }));
        let before = get_zone_group_state(&f.sim.lan(), f.host).unwrap();
        let prepared = f.prepare();
        let replies = Replies::new(&f, Fault::None, None);
        audio_inputs::play_prepared(&replies, &f.houses, &f.destination, &prepared).unwrap();
        assert_eq!(replies.writes(), WRITES);
        assert!(
            replies
                .calls
                .borrow()
                .iter()
                .filter(|(_, name)| WRITES.contains(&name.as_str()))
                .all(|(host, _)| *host == f.host)
        );
        assert_eq!(
            soap::get_media_info(&f.sim.lan(), f.host).unwrap().uri,
            format!("x-rincon-stream:{}", f.input.0)
        );
        assert_eq!(
            soap::get_transport_info(&f.sim.lan(), f.host)
                .unwrap()
                .state,
            TransportState::Playing
        );
        let after = get_zone_group_state(&f.sim.lan(), f.host).unwrap();
        let audience = |state: fsonos_proto::topology::ZoneGroupState| {
            state
                .groups
                .into_iter()
                .map(|group| {
                    (
                        group.coordinator,
                        group
                            .members
                            .into_iter()
                            .map(|member| member.uuid)
                            .collect::<Vec<_>>(),
                    )
                })
                .collect::<Vec<_>>()
        };
        assert_eq!(audience(before), audience(after));
        f.assert_queues_and_input_preserved();
    }
}

#[test]
fn lost_and_malformed_acknowledgements_recover_without_replaying_either_write() {
    for generation in [1, 2] {
        for fault in [Fault::Lost, Fault::Malformed] {
            let f = Fixture::new(generation);
            let prepared = f.prepare();
            let replies = Replies::new(&f, fault, None);
            audio_inputs::play_prepared(&replies, &f.houses, &f.destination, &prepared).unwrap();
            assert_eq!(replies.writes(), WRITES);
            assert_eq!(
                soap::get_transport_info(&f.sim.lan(), f.host)
                    .unwrap()
                    .state,
                TransportState::Playing
            );
            f.assert_queues_and_input_preserved();
        }
    }
}

#[test]
fn unapplied_requests_stop_and_definite_refusals_are_not_replayed() {
    for fault in [Fault::Before, Fault::Rejected] {
        for (index, stage) in WRITES.iter().enumerate() {
            let f = Fixture::new(2);
            let prepared = f.prepare();
            let replies = Replies::new(&f, fault, Some(stage));
            let error = audio_inputs::play_prepared(&replies, &f.houses, &f.destination, &prepared)
                .unwrap_err();
            assert_eq!(replies.writes(), WRITES[..=index]);
            if matches!(fault, Fault::Rejected) && index == 0 {
                assert!(matches!(
                    error,
                    CoreError::Proto(ProtoError::SoapFault { code: 701, .. })
                ));
            } else {
                assert!(matches!(error, CoreError::Uncertain(_)), "{error}");
            }
            f.assert_queues_and_input_preserved();
        }
    }
}

#[test]
fn changed_sources_groups_and_boot_epochs_block_the_first_write() {
    for change in [
        "owner",
        "membership",
        "input_reboot",
        "destination_reboot",
        "source_id",
    ] {
        let f = Fixture::new(2);
        let mut prepared = f.prepare();
        match change {
            "owner" => soap::set_av_transport_uri(&f.sim.lan(), f.host, OWNER, "").unwrap(),
            "membership" => {
                soap::join_group(
                    &f.sim.lan(),
                    f.sim.player("Peer").unwrap().ip,
                    &f.destination,
                )
                .unwrap();
            }
            "input_reboot" => f.sim.reboot(&f.input.0, Duration::ZERO).unwrap(),
            "destination_reboot" => f.sim.reboot(&f.destination.0, Duration::ZERO).unwrap(),
            "source_id" => prepared.source.player = f.destination.clone(),
            _ => unreachable!(),
        }
        let replies = Replies::new(&f, Fault::None, None);
        let error = audio_inputs::play_prepared(&replies, &f.houses, &f.destination, &prepared)
            .unwrap_err();
        assert!(
            matches!(error, CoreError::LineInChanged(_)),
            "{change}: {error}"
        );
        assert!(replies.writes().is_empty(), "{change}");
    }
}

#[test]
fn successful_acknowledgements_do_not_bypass_the_guard_before_play() {
    for fault in [
        Fault::OwnerAfterAck,
        Fault::RegroupAfterAck,
        Fault::RebootInputAfterAck,
    ] {
        let f = Fixture::new(2);
        let prepared = f.prepare();
        let replies = Replies::new(&f, fault, Some("SetAVTransportURI"));
        let error = audio_inputs::play_prepared(&replies, &f.houses, &f.destination, &prepared)
            .unwrap_err();
        assert!(matches!(error, CoreError::Uncertain(_)));
        assert_eq!(
            replies.writes(),
            WRITES[..1],
            "do not play a changed group or source"
        );
        if matches!(fault, Fault::OwnerAfterAck) {
            assert_eq!(
                soap::get_media_info(&f.sim.lan(), f.host).unwrap().uri,
                OWNER
            );
        }
        f.assert_queues_and_input_preserved();
    }
}

#[test]
fn a_final_owner_source_change_is_reported_uncertain_without_rollback() {
    let f = Fixture::new(2);
    let prepared = f.prepare();
    let replies = Replies::new(&f, Fault::OwnerAfterAck, Some("Play"));
    assert!(matches!(
        audio_inputs::play_prepared(&replies, &f.houses, &f.destination, &prepared),
        Err(CoreError::Uncertain(_))
    ));
    assert_eq!(replies.writes(), WRITES);
    assert_eq!(
        soap::get_media_info(&f.sim.lan(), f.host).unwrap().uri,
        OWNER
    );
    f.assert_queues_and_input_preserved();
}

#[test]
fn a_prepared_input_cannot_be_redirected_to_another_coordinator() {
    let f = Fixture::new(2);
    let prepared = f.prepare();
    let peer = PlayerId(f.sim.player("Peer").unwrap().uuid.clone());
    let replies = Replies::new(&f, Fault::None, None);
    assert!(matches!(
        audio_inputs::play_prepared(&replies, &f.houses, &peer, &prepared),
        Err(CoreError::LineInChanged(_))
    ));
    assert!(replies.writes().is_empty());
}

#[test]
fn a_reused_destination_address_cannot_control_a_different_player() {
    let f = Fixture::new(2);
    let prepared = f.prepare();
    let replies = Replies::new(&f, Fault::ReplacedDestinationAddress, None);
    assert!(matches!(
        audio_inputs::play_prepared(&replies, &f.houses, &f.destination, &prepared),
        Err(CoreError::LineInChanged(_))
    ));
    assert!(replies.writes().is_empty());
    assert_eq!(soap::get_media_info(&f.sim.lan(), f.host).unwrap().uri, OLD);
}
