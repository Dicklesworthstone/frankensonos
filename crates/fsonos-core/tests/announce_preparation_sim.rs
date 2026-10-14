//! Announcement preparation uses live groups and caps before audible changes.

use fsonos_core::announce::{Announcer, Clip};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::net::IpAddr;
use std::time::Duration;

fn sim(s2: bool) -> SimHandle {
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let players = [
        SimPlayerSpec::new("Alpha", model),
        SimPlayerSpec::new("Beta", model),
        SimPlayerSpec::new("Gamma", model),
    ];
    let builder = SimHousehold::builder();
    if s2 {
        builder.s2(players)
    } else {
        builder.s1(players)
    }
    .spawn()
    .unwrap()
}

fn houses(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    let mut h = HouseholdState::default();
    h.apply_topology(&get_zone_group_state(lan, sim.player("Alpha").unwrap().ip).unwrap());
    vec![h]
}

fn clip() -> Clip {
    Clip {
        url: "http://192.0.2.250:3400/media/preparation.wav".into(),
        title: "Synthetic test".into(),
        duration: Duration::from_millis(1),
    }
}

fn announcer() -> Announcer {
    Announcer::with_timing(Duration::from_millis(1), Duration::ZERO)
}

fn join(lan: &SimLan, h: &[HouseholdState], member: &str, lead: &str) {
    control::join(
        lan,
        h,
        &resolve_room(h, member).unwrap().player.id,
        &resolve_room(h, lead).unwrap().player.id,
    )
    .unwrap();
}

#[test]
fn a_target_returns_to_its_current_group_not_its_stale_resolved_group() {
    for s2 in [false, true] {
        let sim = sim(s2);
        let lan = sim.lan();
        let h = houses(&sim, &lan);
        join(&lan, &h, "Beta", "Alpha");
        let stale = houses(&sim, &lan);
        let target = resolve_room(&stale, "Beta").unwrap();
        // The request has resolved, but the room moves before its turn.
        join(&lan, &stale, "Beta", "Gamma");
        let from = sim.soap_log().len();
        let report = announcer()
            .announce(&lan, &[target], &clip(), 25, &|_| 25)
            .unwrap();
        assert!(report.households[0].outcome.is_ok(), "{report:?}");
        assert!(report.households[0].restore_failed.is_empty(), "{report:?}");
        let h = houses(&sim, &lan);
        assert_eq!(
            resolve_room(&h, "Beta").unwrap().coordinator.id,
            resolve_room(&h, "Gamma").unwrap().player.id
        );
        assert!(
            sim.soap_log()[from..]
                .iter()
                .all(|e| e.room != "Alpha" || e.action.starts_with("Get")),
            "the former group must never be changed"
        );
    }
}

#[test]
fn every_target_is_capped_before_any_regroup_or_unmute() {
    for s2 in [false, true] {
        let sim = sim(s2);
        let lan = sim.lan();
        let h = houses(&sim, &lan);
        for room in ["Alpha", "Beta"] {
            let host = sim.player(room).unwrap().ip;
            soap::set_volume(&lan, host, 95).unwrap();
            soap::set_mute(&lan, host, true).unwrap();
        }
        let from = sim.soap_log().len();
        let report = announcer()
            .announce(
                &lan,
                &[
                    resolve_room(&h, "Alpha").unwrap(),
                    resolve_room(&h, "Beta").unwrap(),
                ],
                &clip(),
                80,
                &|_| 25,
            )
            .unwrap();
        assert!(report.households[0].outcome.is_ok(), "{report:?}");
        let log = sim.soap_log();
        let log = &log[from..];
        let audible = log
            .iter()
            .position(|e| e.action == "SetAVTransportURI" || e.action == "SetMute")
            .unwrap();
        for room in ["Alpha", "Beta"] {
            assert!(log[..audible].iter().any(|e| {
                e.room == room
                    && e.action == "SetVolume"
                    && e.args
                        .iter()
                        .any(|(k, v)| k == "DesiredVolume" && v == "25")
            }));
            let host = sim.player(room).unwrap().ip;
            assert_eq!(soap::get_volume(&lan, host).unwrap(), 95);
            assert!(soap::get_mute(&lan, host).unwrap());
        }
    }
}

struct Failing<'a> {
    lan: &'a SimLan,
    action: &'static str,
}

impl Transport for Failing<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action
            .trim_matches('"')
            .ends_with(&format!("#{}", self.action))
        {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "synthetic preparation failure".into(),
            });
        }
        self.lan.soap_post(host, path, action, body)
    }
}

#[test]
fn unreadable_topology_never_falls_back_to_writing_from_stale_groups() {
    for s2 in [false, true] {
        let sim = sim(s2);
        let lan = sim.lan();
        let h = houses(&sim, &lan);
        let from = sim.soap_log().len();
        let failing = Failing {
            lan: &lan,
            action: "GetZoneGroupState",
        };
        let report = announcer()
            .announce(
                &failing,
                &[resolve_room(&h, "Alpha").unwrap()],
                &clip(),
                25,
                &|_| 25,
            )
            .unwrap();
        assert!(report.households[0].outcome.is_err());
        assert!(report.households[0].levels.is_empty());
        assert!(report.households[0].restored.is_empty());
        assert!(sim.soap_log()[from..].is_empty());
    }
}

#[test]
fn a_failed_volume_preparation_does_not_rewind_or_regroup_the_music() {
    for s2 in [false, true] {
        let sim = sim(s2);
        let lan = sim.lan();
        let h = houses(&sim, &lan);
        let from = sim.soap_log().len();
        let failing = Failing {
            lan: &lan,
            action: "SetVolume",
        };
        let report = announcer()
            .announce(
                &failing,
                &[
                    resolve_room(&h, "Alpha").unwrap(),
                    resolve_room(&h, "Beta").unwrap(),
                ],
                &clip(),
                25,
                &|_| 25,
            )
            .unwrap();
        assert!(report.households[0].outcome.is_err());
        assert!(
            sim.soap_log()[from..]
                .iter()
                .all(|e| e.action.starts_with("Get")),
            "no transport, group or mute writes after a failed volume preparation"
        );
    }
}
