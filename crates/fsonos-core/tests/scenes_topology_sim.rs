//! Scene regrouping uses coherent, household-bound topology before dependent
//! writes. Faults alter only SOAP replies; the speakers run on real sockets.

use fsonos_core::fade::Fader;
use fsonos_core::scenes::{self, ApplyOptions, Scene, SceneGroup, SceneOp, SceneSource};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::soap::{ZONE_GROUP_TOPOLOGY, args_xml, envelope, parse_response};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::PlayerId;
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Duration;

const WANTED: &str = "x-rincon-mp3radio://example.invalid/scene-radio";

#[derive(Clone, Copy, Debug)]
enum Fault {
    None,
    DuplicateCoordinator,
    ForeignIdentity,
    ForeignAddress,
    ForeignVanished,
    WrongResponderAddress,
    BadFirstWitness,
    AddedSatellite,
    MissingBond,
}

struct Fixture {
    sim: SimHandle,
    houses: Vec<HouseholdState>,
    main: PlayerId,
    main_host: IpAddr,
    joining: PlayerId,
    joining_host: IpAddr,
    hidden: PlayerId,
    other: PlayerId,
    other_host: IpAddr,
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
                    SimPlayerSpec::pair("Main", model),
                    SimPlayerSpec::pair("Joining", model),
                ]
            } else {
                vec![SimPlayerSpec::new("Other", model)]
            }
        };
        let sim = SimHousehold::builder()
            .s1(rooms(1, generation == 1))
            .s2(rooms(2, generation == 2))
            .spawn()
            .unwrap();
        let main_host = sim.player("Main").unwrap().ip;
        let joining_host = sim.player("Joining").unwrap().ip;
        let other_host = sim.player("Other").unwrap().ip;
        let houses: Vec<_> = [main_host, other_host]
            .into_iter()
            .map(|host| {
                let mut house = HouseholdState::default();
                house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
                house
            })
            .collect();
        let main_room = resolve_room(&houses, "Main").unwrap();
        let main = main_room.player.id.clone();
        let hidden = main_room
            .room
            .players
            .iter()
            .find(|id| **id != main)
            .unwrap()
            .clone();
        let joining = resolve_room(&houses, "Joining").unwrap().player.id.clone();
        let other = resolve_room(&houses, "Other").unwrap().player.id.clone();
        Self {
            sim,
            houses,
            main,
            main_host,
            joining,
            joining_host,
            hidden,
            other,
            other_host,
        }
    }

    fn apply(&self, fault: Fault) -> scenes::SceneReport {
        let source = || SceneSource::Uri {
            uri: WANTED.into(),
            metadata: String::new(),
        };
        let scene = Scene {
            name: "Regroup".into(),
            groups: vec![
                SceneGroup {
                    coordinator: "Main".into(),
                    members: vec!["Joining".into()],
                    source: source(),
                    playing: Some(true),
                },
                SceneGroup {
                    coordinator: "Other".into(),
                    members: Vec::new(),
                    source: source(),
                    playing: Some(true),
                },
            ],
            volumes: [
                ("Main".into(), 17),
                ("Joining".into(), 18),
                ("Other".into(), 19),
            ]
            .into(),
            mutes: [("Main".into(), true)].into(),
            writer: None,
        };
        let plan = scenes::plan_apply(&scene, &self.houses, &[]).unwrap();
        let t = Replies {
            inner: self.sim.lan(),
            fixture: self,
            fault,
            joined: Cell::new(false),
        };
        let fader = Fader::with_timing(Duration::from_millis(1), Duration::from_millis(1));
        scenes::apply(
            &t,
            &self.houses,
            &plan,
            &ApplyOptions {
                fade: Some((&fader, Duration::from_millis(1))),
                ..ApplyOptions::default()
            },
        )
    }

    fn protected(&self, start: usize, report: &scenes::SceneReport) {
        assert!(!report.is_complete(), "{report:#?}");
        assert!(report.failed.iter().any(|(op, _)| {
            matches!(op, SceneOp::Uri { coordinator, .. } if *coordinator == self.main)
        }));
        let writes: Vec<_> = self.sim.soap_log()[start..]
            .iter()
            .filter(|entry| {
                entry.player != self.other.0
                    && !entry.action.starts_with("Get")
                    && entry.action != "Browse"
            })
            .map(|entry| (entry.player.clone(), entry.action.clone()))
            .collect();
        assert_eq!(
            writes,
            [(self.joining.0.clone(), "SetAVTransportURI".into())],
            "only the original join may run in the unconfirmed household"
        );
        let lan = self.sim.lan();
        assert_eq!(
            soap::get_media_info(&lan, self.other_host).unwrap().uri,
            WANTED
        );
        assert_eq!(soap::get_volume(&lan, self.other_host).unwrap(), 19);
        assert!(report.done.iter().any(|op| {
            matches!(op, SceneOp::Uri { coordinator, .. } if *coordinator == self.other)
        }));
    }
}

struct Replies<'a> {
    inner: SimLan,
    fixture: &'a Fixture,
    fault: Fault,
    joined: Cell<bool>,
}

fn member(id: &str, host: IpAddr) -> String {
    format!(
        "<ZoneGroupMember UUID=\"{id}\" Location=\"http://{host}:1400/xml/device_description.xml\" \
         ZoneName=\"Synthetic\"/>"
    )
}

fn group(id: &str, host: IpAddr) -> String {
    format!(
        "<ZoneGroup Coordinator=\"{id}\">{}</ZoneGroup>",
        member(id, host)
    )
}

fn member_range(document: &str, id: &PlayerId) -> std::ops::Range<usize> {
    let start = document
        .find(&format!("<ZoneGroupMember UUID=\"{}\"", id.0))
        .unwrap();
    let end = start + document[start..].find("/>").unwrap() + 2;
    start..end
}

impl Replies<'_> {
    fn alter(&self, document: &mut String, host: IpAddr) {
        let f = self.fixture;
        let spare = "192.0.2.250".parse().unwrap();
        let extra = match self.fault {
            Fault::DuplicateCoordinator => Some(group(&f.main.0, spare)),
            Fault::ForeignIdentity => Some(group(&f.other.0, spare)),
            Fault::ForeignAddress => Some(group("synthetic-foreign", f.other_host)),
            _ => None,
        };
        if let Some(extra) = extra {
            *document = document.replace("</ZoneGroups>", &format!("{extra}</ZoneGroups>"));
        }
        match self.fault {
            Fault::ForeignVanished => {
                *document = document.replace(
                    "<VanishedDevices>",
                    &format!("<VanishedDevices><Device UUID=\"{}\"/>", f.other.0),
                );
            }
            Fault::WrongResponderAddress | Fault::BadFirstWitness => {
                if matches!(self.fault, Fault::WrongResponderAddress) || host == f.main_host {
                    *document = document.replace(&format!("http://{host}:"), "http://192.0.2.250:");
                }
            }
            Fault::AddedSatellite => {
                let range = member_range(document, &f.main);
                document.insert_str(range.end - 2, " HTSatChanMapSet=\"synthetic-sub:SW\"");
                let range = member_range(document, &f.main);
                document.replace_range(
                    range.end - 2..range.end,
                    "><Satellite UUID=\"synthetic-sub\" Invisible=\"1\" \
                     Location=\"http://192.0.2.250:1400/x\"/></ZoneGroupMember>",
                );
            }
            Fault::MissingBond => {
                document.replace_range(member_range(document, &f.hidden), "");
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
        if name == "SetAVTransportURI" && host == self.fixture.joining_host {
            self.joined.set(true);
        }
        if !self.joined.get() || name != "GetZoneGroupState" || host == self.fixture.other_host {
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
fn incoherent_or_foreign_confirmation_blocks_dependent_writes_on_both_generations() {
    for generation in [1, 2] {
        for fault in [
            Fault::DuplicateCoordinator,
            Fault::ForeignIdentity,
            Fault::ForeignAddress,
            Fault::ForeignVanished,
            Fault::WrongResponderAddress,
        ] {
            let f = Fixture::new(generation);
            let start = f.sim.soap_log().len();
            let report = f.apply(fault);
            f.protected(start, &report);
        }
    }
}

#[test]
fn changed_bonded_players_cannot_hide_behind_unchanged_room_names() {
    for generation in [1, 2] {
        for fault in [Fault::AddedSatellite, Fault::MissingBond] {
            let f = Fixture::new(generation);
            let start = f.sim.soap_log().len();
            let report = f.apply(fault);
            f.protected(start, &report);
        }
    }
}

#[test]
fn coherent_paired_regrouping_continues_after_one_bad_witness() {
    for generation in [1, 2] {
        for fault in [Fault::None, Fault::BadFirstWitness] {
            let f = Fixture::new(generation);
            let report = f.apply(fault);
            assert!(report.is_complete(), "{fault:?}: {report:#?}");
            let lan = f.sim.lan();
            let topology = get_zone_group_state(&lan, f.main_host).unwrap();
            let group = topology
                .groups
                .iter()
                .find(|group| group.coordinator == f.main)
                .unwrap();
            assert_eq!(group.members.len(), 4, "both stereo pairs remain whole");
            assert_eq!(soap::get_media_info(&lan, f.main_host).unwrap().uri, WANTED);
            assert_eq!(soap::get_volume(&lan, f.main_host).unwrap(), 17);
            assert_eq!(soap::get_volume(&lan, f.joining_host).unwrap(), 18);
            assert_eq!(
                soap::get_media_info(&lan, f.other_host).unwrap().uri,
                WANTED
            );
        }
    }
}
