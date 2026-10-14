//! New owner actions during an announcement are not overwritten by cleanup.

use fsonos_core::announce::{Announcer, Clip, HouseholdAnnouncement};
use fsonos_core::snapshot::Aspect;
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec, SoapLogEntry};
use fsonos_types::TransportState;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const CLIP: &str = "http://192.0.2.250:3400/media/ownership.wav";
const OLD: &str = "x-rincon-mp3radio://stream.example.invalid/original";
const NEW: &str = "x-rincon-mp3radio://stream.example.invalid/newer";

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

#[derive(Clone, Copy)]
enum Change {
    Source,
    DeadlineSource,
    CleanupSource,
    Pause,
    Group,
    Volume,
    Mute,
    ReadFailure,
    RestoreFailure,
}

struct Changing<'a> {
    sim: &'a SimHandle,
    lan: &'a SimLan,
    change: Change,
    installed: AtomicBool,
    changed: AtomicBool,
    stopped: AtomicBool,
    after: AtomicUsize,
}

impl Transport for Changing<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let intervene = match self.change {
            Change::DeadlineSource => action.contains("#GetZoneGroupState"),
            Change::CleanupSource => {
                self.stopped.load(Ordering::Acquire) && action.contains("#GetZoneGroupState")
            }
            _ => action.contains("#GetTransportInfo"),
        };
        if self.installed.load(Ordering::Acquire)
            && intervene
            && !self.changed.swap(true, Ordering::AcqRel)
        {
            let lead = self.sim.player("Alpha").unwrap().ip;
            match self.change {
                Change::Source | Change::DeadlineSource | Change::CleanupSource => {
                    soap::set_av_transport_uri(self.lan, lead, NEW, "")?;
                    soap::play(self.lan, lead)?;
                }
                Change::Pause => soap::pause(self.lan, lead)?,
                Change::Group => {
                    let h = houses(self.sim, self.lan);
                    let gamma = &resolve_room(&h, "Gamma").unwrap().player.id;
                    soap::join_group(self.lan, self.sim.player("Beta").unwrap().ip, gamma)?;
                }
                Change::Volume => soap::set_volume(self.lan, lead, 55)?,
                Change::Mute => soap::set_mute(self.lan, lead, true)?,
                Change::ReadFailure | Change::RestoreFailure => {}
            }
            self.after
                .store(self.sim.soap_log().len(), Ordering::Release);
        }
        if self.changed.load(Ordering::Acquire) {
            if matches!(self.change, Change::ReadFailure) && action.contains("#GetMediaInfo") {
                return Err(ProtoError::Network {
                    target: host.to_string(),
                    detail: "synthetic identity read failure".into(),
                });
            }
            if matches!(self.change, Change::RestoreFailure)
                && action.contains("#SetAVTransportURI")
                && body.contains(OLD)
            {
                self.after
                    .store(self.sim.soap_log().len(), Ordering::Release);
                return Err(ProtoError::SoapFault {
                    code: 701,
                    reason: "synthetic source restore refusal".into(),
                });
            }
        }
        let result = self.lan.soap_post(host, path, action, body);
        if result.is_ok() && action.contains("#SetAVTransportURI") && body.contains(CLIP) {
            self.installed.store(true, Ordering::Release);
        }
        if result.is_ok() && action.contains("#Stop") {
            self.stopped.store(true, Ordering::Release);
        }
        result
    }
}

fn run(s2: bool, change: Change) {
    let sim = sim(s2);
    let lan = sim.lan();
    let host = sim.player("Alpha").unwrap().ip;
    soap::set_av_transport_uri(&lan, host, OLD, "").unwrap();
    soap::play(&lan, host).unwrap();
    soap::set_volume(&lan, host, 40).unwrap();
    let h = houses(&sim, &lan);
    let transport = Changing {
        sim: &sim,
        lan: &lan,
        change,
        installed: AtomicBool::new(false),
        changed: AtomicBool::new(false),
        stopped: AtomicBool::new(false),
        after: AtomicUsize::new(0),
    };
    let mut targets = vec![resolve_room(&h, "Alpha").unwrap()];
    if matches!(change, Change::Group) {
        targets.push(resolve_room(&h, "Beta").unwrap());
    }
    let clip = Clip {
        url: CLIP.into(),
        title: "Synthetic ownership test".into(),
        duration: Duration::from_millis(1),
    };
    let report = Announcer::with_timing(Duration::from_millis(1), Duration::ZERO)
        .announce(&transport, &targets, &clip, 25, &|_| 25)
        .unwrap();
    let report = &report.households[0];
    assert!(
        transport.changed.load(Ordering::Acquire),
        "the test intervention ran"
    );
    assert!(
        !report.is_clean(),
        "interventions must not report a clean restore"
    );
    let log = sim.soap_log();
    let after = &log[transport.after.load(Ordering::Acquire)..];
    match change {
        Change::Source
        | Change::DeadlineSource
        | Change::CleanupSource
        | Change::Pause
        | Change::Group
        | Change::ReadFailure => {
            if !matches!(change, Change::CleanupSource) {
                assert!(report.outcome.is_err(), "{report:?}");
            }
            assert!(
                after.iter().all(|e| e.action.starts_with("Get")),
                "{after:?}"
            );
            assert!(report.restored.iter().any(|(_, r)| !r.skipped.is_empty()));
            match change {
                Change::Source | Change::DeadlineSource | Change::CleanupSource => {
                    assert_eq!(soap::get_media_info(&lan, host).unwrap().uri, NEW);
                }
                Change::Pause => assert_eq!(
                    soap::get_transport_info(&lan, host).unwrap().state,
                    TransportState::Paused
                ),
                Change::Group => {
                    let now = houses(&sim, &lan);
                    assert_eq!(
                        resolve_room(&now, "Beta").unwrap().coordinator.id,
                        resolve_room(&now, "Gamma").unwrap().player.id
                    );
                }
                _ => {}
            }
        }
        Change::Volume | Change::Mute => setting_kept(change, report, &lan, host),
        Change::RestoreFailure => restore_refused(report, after, &lan, host),
    }
}

/// The owner changed only a volume or mute: the music plays on and the
/// announcement leaves that setting as the owner set it.
fn setting_kept(change: Change, report: &HouseholdAnnouncement, lan: &SimLan, host: IpAddr) {
    assert!(report.outcome.is_ok(), "{report:?}");
    assert_eq!(soap::get_media_info(lan, host).unwrap().uri, OLD);
    assert_eq!(
        soap::get_transport_info(lan, host).unwrap().state,
        TransportState::Playing
    );
    let aspect = if matches!(change, Change::Volume) {
        assert_eq!(soap::get_volume(lan, host).unwrap(), 55);
        Aspect::Volume
    } else {
        assert!(soap::get_mute(lan, host).unwrap());
        Aspect::Mute
    };
    assert!(skipped(report, aspect));
}

/// The source restore was refused: nothing plays or seeks afterwards, and
/// the source and transport are reported as not restored.
fn restore_refused(
    report: &HouseholdAnnouncement,
    after: &[SoapLogEntry],
    lan: &SimLan,
    host: IpAddr,
) {
    assert!(
        after
            .iter()
            .all(|e| e.action != "Play" && e.action != "Seek"),
        "{after:?}"
    );
    assert_eq!(
        soap::get_transport_info(lan, host).unwrap().state,
        TransportState::Stopped
    );
    assert!(skipped(report, Aspect::Source));
    assert!(skipped(report, Aspect::Transport));
}

/// Whether a restore in `report` skipped `aspect`.
fn skipped(report: &HouseholdAnnouncement, aspect: Aspect) -> bool {
    report
        .restored
        .iter()
        .any(|(_, r)| r.skipped.iter().any(|(a, _)| *a == aspect))
}

#[test]
fn newer_music_is_neither_stopped_nor_replaced() {
    for s2 in [false, true] {
        run(s2, Change::Source);
    }
}

#[test]
fn a_manual_pause_does_not_restart_the_previous_music() {
    for s2 in [false, true] {
        run(s2, Change::Pause);
    }
}

#[test]
fn regrouping_during_the_clip_is_not_undone() {
    for s2 in [false, true] {
        run(s2, Change::Group);
    }
}

#[test]
fn a_newer_volume_survives_restoration() {
    for s2 in [false, true] {
        run(s2, Change::Volume);
    }
}

#[test]
fn a_newer_mute_survives_restoration() {
    for s2 in [false, true] {
        run(s2, Change::Mute);
    }
}

#[test]
fn unreadable_source_identity_does_not_authorize_cleanup() {
    for s2 in [false, true] {
        run(s2, Change::ReadFailure);
    }
}

#[test]
fn a_failed_source_restore_does_not_play_the_clip_again() {
    for s2 in [false, true] {
        run(s2, Change::RestoreFailure);
    }
}

#[test]
fn a_source_change_during_the_deadline_check_prevents_stop() {
    for s2 in [false, true] {
        run(s2, Change::DeadlineSource);
    }
}

#[test]
fn a_source_change_after_the_clip_stops_prevents_restoration() {
    for s2 in [false, true] {
        run(s2, Change::CleanupSource);
    }
}
