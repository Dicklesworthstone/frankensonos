//! Source/group evidence over real S1/S2 sockets, without capture hardware.

use super::{restore, verify_measurement};
use fsonos_core::snapshot::{self, ZoneSnapshot};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::cell::Cell;
use std::net::IpAddr;

const OLD: &str = "x-rincon-mp3radio://example.invalid/previous";
const STREAM: &str = "x-rincon-mp3radio://192.0.2.250:3400/stream/measurement.mp3";
const OTHER: &str = "x-rincon-mp3radio://example.invalid/newer";

struct Rig {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    before: ZoneSnapshot,
    host: IpAddr,
    member: PlayerId,
    member_host: IpAddr,
}

impl Rig {
    fn new(s2: bool) -> Self {
        let model = if s2 {
            SimModel::One
        } else {
            SimModel::Play5Gen1
        };
        let players = [
            SimPlayerSpec::new("Alpha", model),
            SimPlayerSpec::new("Beta", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2(players)
        } else {
            builder.s1(players)
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let host = sim.player("Alpha").unwrap().ip;
        let member_host = sim.player("Beta").unwrap().ip;
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let coordinator = resolve_room(std::slice::from_ref(&house), "Alpha")
            .unwrap()
            .player
            .id
            .clone();
        let member = resolve_room(std::slice::from_ref(&house), "Beta")
            .unwrap()
            .player
            .id
            .clone();
        soap::join_group(&lan, member_host, &coordinator).unwrap();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        let houses = vec![house];
        soap::set_av_transport_uri(&lan, host, OLD, "").unwrap();
        soap::play(&lan, host).unwrap();
        let before = snapshot::capture(&lan, &houses, &coordinator, 1_000).unwrap();
        soap::set_av_transport_uri(&lan, host, STREAM, "").unwrap();
        soap::play(&lan, host).unwrap();
        Self {
            sim,
            lan,
            houses,
            before,
            host,
            member,
            member_host,
        }
    }

    fn verify<T: Transport + ?Sized>(&self, transport: &T, settled: bool) -> Result<(), String> {
        verify_measurement(transport, &self.houses, &self.before, STREAM, settled)
    }

    fn readonly_since(&self, first: usize) {
        let log = self.sim.soap_log();
        assert!(
            log[first..]
                .iter()
                .all(|entry| entry.action.starts_with("Get")),
            "{:?}",
            &log[first..]
        );
    }
}

#[derive(Clone, Copy)]
enum Change {
    Alias,
    Source,
    Group,
    Transitioning,
    ReadFailure,
}

struct Observe<'a> {
    rig: &'a Rig,
    change: Change,
    changed: Cell<bool>,
    status_reads: Cell<u32>,
}

impl<'a> Observe<'a> {
    fn new(rig: &'a Rig, change: Change) -> Self {
        Self {
            rig,
            change,
            changed: Cell::new(false),
            status_reads: Cell::new(0),
        }
    }
}

impl Transport for Observe<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let status = action.contains("#GetTransportInfo");
        if status {
            self.status_reads.set(self.status_reads.get() + 1);
            if matches!(self.change, Change::ReadFailure) {
                return Err(ProtoError::Network {
                    target: host.to_string(),
                    detail: "injected measurement read failure".into(),
                });
            }
        }
        let mut response = self.rig.lan.soap_post(host, path, action, body)?;
        if status && matches!(self.change, Change::Transitioning) {
            response = response.replace("PLAYING", "TRANSITIONING");
        }
        if action.contains("#GetMediaInfo") {
            if matches!(self.change, Change::Alias) {
                let http = STREAM.replacen("x-rincon-mp3radio://", "http://", 1);
                response = response.replace(STREAM, &http);
            }
            if !self.changed.replace(true) {
                match self.change {
                    Change::Source => {
                        soap::set_av_transport_uri(&self.rig.lan, host, OTHER, "")?;
                        soap::play(&self.rig.lan, host)?;
                    }
                    Change::Group => {
                        soap::leave_group(&self.rig.lan, self.rig.member_host, &self.rig.member)?;
                    }
                    _ => {}
                }
            }
        }
        Ok(response)
    }
}

#[test]
fn current_group_and_exact_radio_alias_are_accepted_by_read_only_checks() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let first = rig.sim.soap_log().len();
        rig.verify(&rig.lan, false).unwrap();
        rig.verify(&rig.lan, true).unwrap();
        rig.verify(&Observe::new(&rig, Change::Alias), true)
            .unwrap();
        rig.readonly_since(first);
        // Same host/path with different query data is not this stream.
        let other = format!("{STREAM}?other=1");
        soap::set_av_transport_uri(&rig.lan, rig.host, &other, "").unwrap();
        soap::play(&rig.lan, rig.host).unwrap();
        let first = rig.sim.soap_log().len();
        assert!(rig.verify(&rig.lan, true).is_err());
        rig.readonly_since(first);
    }
}

#[test]
fn a_paused_or_stopped_stream_is_not_restarted_to_obtain_measurement() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        soap::pause(&rig.lan, rig.host).unwrap();
        let first = rig.sim.soap_log().len();
        assert!(rig.verify(&rig.lan, false).is_err());
        assert!(rig.verify(&rig.lan, true).is_err());
        rig.readonly_since(first);
        soap::stop(&rig.lan, rig.host).unwrap();
        let first = rig.sim.soap_log().len();
        assert!(rig.verify(&rig.lan, false).is_err());
        rig.readonly_since(first);
    }
}

#[test]
fn source_and_group_changes_during_observation_invalidate_the_window() {
    for s2 in [false, true] {
        for change in [Change::Source, Change::Group] {
            let rig = Rig::new(s2);
            let observer = Observe::new(&rig, change);
            assert!(rig.verify(&observer, true).is_err());
            assert!(
                observer.changed.get(),
                "independent owner action was injected"
            );
            let first = rig.sim.soap_log().len();
            let report = restore(&rig.lan, &rig.houses, &rig.before, STREAM, true).unwrap();
            assert!(!report.skipped.is_empty());
            rig.readonly_since(first);
            if matches!(change, Change::Source) {
                assert_eq!(soap::get_media_info(&rig.lan, rig.host).unwrap().uri, OTHER);
            }
        }
    }
}

#[test]
fn buffering_is_allowed_before_capture_but_not_as_evidence_afterward() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let observer = Observe::new(&rig, Change::Transitioning);
        let first = rig.sim.soap_log().len();
        rig.verify(&observer, false).unwrap();
        assert!(rig.verify(&observer, true).is_err());
        assert_eq!(observer.status_reads.get(), 2);
        rig.readonly_since(first);
    }
}

#[test]
fn unavailable_transport_evidence_is_not_retried_or_reported_as_playing() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let observer = Observe::new(&rig, Change::ReadFailure);
        let first = rig.sim.soap_log().len();
        assert!(
            rig.verify(&observer, true)
                .unwrap_err()
                .contains("injected measurement")
        );
        assert_eq!(observer.status_reads.get(), 1);
        rig.readonly_since(first);
    }
}

#[test]
fn successful_measurement_checks_leave_the_existing_restoration_path_usable() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let first = rig.sim.soap_log().len();
        for _ in 0..3 {
            rig.verify(&rig.lan, false).unwrap();
            rig.verify(&rig.lan, true).unwrap();
        }
        rig.readonly_since(first);
        let report = restore(&rig.lan, &rig.houses, &rig.before, STREAM, true).unwrap();
        assert!(report.skipped.is_empty(), "{report:?}");
        assert_eq!(soap::get_media_info(&rig.lan, rig.host).unwrap().uri, OLD);
        assert_eq!(
            soap::get_transport_info(&rig.lan, rig.host).unwrap().state,
            TransportState::Playing
        );
    }
}
