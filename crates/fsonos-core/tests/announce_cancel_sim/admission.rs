//! Admission and read-only sleep continuation at preparation boundaries.

use super::*;
use fsonos_core::snapshot::{Aspect, RestoreReport};

#[derive(Clone, Copy, PartialEq, Eq)]
enum Override {
    None,
    Source,
    Pause,
    Group,
    Volume,
    Mute,
}

fn owner_change(sim: &SimHandle, lan: &SimLan, room: &str, change: Override) {
    let host = sim.player(room).unwrap().ip;
    match change {
        Override::None => {}
        Override::Source => {
            soap::set_av_transport_uri(lan, host, NEW, "").unwrap();
            soap::play(lan, host).unwrap();
        }
        Override::Pause => soap::pause(lan, host).unwrap(),
        Override::Group => soap::join_group(
            lan,
            host,
            &PlayerId(sim.player("Gamma").unwrap().uuid.clone()),
        )
        .unwrap(),
        Override::Volume => soap::set_volume(lan, host, 88).unwrap(),
        Override::Mute => soap::set_mute(lan, host, false).unwrap(),
    }
}

fn setup(s2: bool) -> (SimHandle, SimLan, Vec<HouseholdState>) {
    let sim = simulator(s2);
    let lan = sim.lan();
    for (room, source, level) in [("Alpha", OLD, 40), ("Beta", OTHER, 37)] {
        let host = sim.player(room).unwrap().ip;
        soap::set_av_transport_uri(&lan, host, source, "").unwrap();
        soap::play(&lan, host).unwrap();
        soap::set_volume(&lan, host, level).unwrap();
        soap::set_mute(&lan, host, true).unwrap();
    }
    let households = houses(&sim, &lan);
    (sim, lan, households)
}

fn announce<T: Transport + Sync>(
    transport: &T,
    households: &[HouseholdState],
    control: &AnnouncementControl,
    callbacks: &AnnouncementCallbacks<'_>,
) -> AnnounceReport {
    announcer()
        .announce_controlled(
            transport,
            &[
                resolve_room(households, "Alpha").unwrap(),
                resolve_room(households, "Beta").unwrap(),
            ],
            &clip(),
            25,
            None,
            callbacks,
            control,
        )
        .unwrap()
}

fn row<'a>(report: &'a AnnounceReport, sim: &SimHandle, room: &str) -> &'a RestoreReport {
    &report.households[0]
        .restored
        .iter()
        .find(|(coordinator, _)| coordinator.0 == sim.player(room).unwrap().uuid)
        .unwrap_or_else(|| panic!("missing explicit restoration row for {room}: {report:?}"))
        .1
}

struct Preparing<'a> {
    sim: &'a SimHandle,
    lan: &'a SimLan,
    control: &'a AnnouncementControl,
    caps: &'a AtomicBool,
    partial: bool,
    during_snapshot: bool,
    fail_snapshot: bool,
    owner: Override,
    triggered: AtomicBool,
    after_owner: AtomicUsize,
}

impl Transport for Preparing<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let beta = self.sim.player("Beta").unwrap().ip;
        if self.fail_snapshot
            && self.caps.load(Ordering::Acquire)
            && host == beta
            && action.contains("#GetVolume")
        {
            return Err(ProtoError::SoapFault {
                code: 701,
                reason: "synthetic snapshot refusal".into(),
            });
        }
        let result = self.lan.soap_post(host, path, action, body);
        let at_volume = self.partial && action.contains("#SetVolume");
        let at_snapshot = self.during_snapshot
            && self.caps.load(Ordering::Acquire)
            && host == beta
            && action.contains("#GetMediaInfo");
        if result.is_ok()
            && (at_volume || at_snapshot)
            && !self.triggered.swap(true, Ordering::AcqRel)
        {
            assert!(self.control.cancel());
            owner_change(
                self.sim,
                self.lan,
                if at_volume { "Alpha" } else { "Beta" },
                self.owner,
            );
            self.after_owner
                .store(self.sim.soap_log().len(), Ordering::Release);
        }
        result
    }
}

#[test]
fn denied_household_admission_never_interrupts_fades_or_mutates_speakers() {
    for s2 in [false, true] {
        let (sim, lan, households) = setup(s2);
        let control = AnnouncementControl::default();
        let finished = AtomicUsize::new(0);
        let from = sim.soap_log().len();
        let report = announce(
            &lan,
            &households,
            &control,
            &AnnouncementCallbacks {
                clip_url: None,
                authorize: &|| Err("announcement grant was revoked".into()),
                cap: &|_| panic!("refused work must not interrupt a sleep fade"),
                completed: &|_, _| {
                    finished.fetch_add(1, Ordering::Relaxed);
                },
            },
        );
        assert_eq!(
            report.households[0].outcome,
            Err("announcement grant was revoked".into())
        );
        assert!(report.households[0].levels.is_empty());
        assert!(report.households[0].restored.is_empty());
        assert_eq!(finished.load(Ordering::Relaxed), 1);
        assert!(
            sim.soap_log()[from..]
                .iter()
                .all(|call| call.action.starts_with("Get"))
        );
    }
}

#[test]
fn post_snapshot_cancellation_or_revocation_explicitly_verifies_both_untouched_zones() {
    for s2 in [false, true] {
        for cancelled in [false, true] {
            let (sim, lan, households) = setup(s2);
            let control = AnnouncementControl::default();
            let authorizations = AtomicUsize::new(0);
            let caps = AtomicUsize::new(0);
            let completed = AtomicUsize::new(0);
            let from = sim.soap_log().len();
            let report = announce(
                &lan,
                &households,
                &control,
                &AnnouncementCallbacks {
                    clip_url: None,
                    authorize: &|| {
                        if authorizations.fetch_add(1, Ordering::Relaxed) == 0 {
                            return Ok(());
                        }
                        if cancelled {
                            assert!(control.cancel());
                            Ok(())
                        } else {
                            Err("grant revoked while taking snapshots".into())
                        }
                    },
                    cap: &|_| {
                        caps.fetch_add(1, Ordering::Relaxed);
                        25
                    },
                    completed: &|_, report| {
                        assert_eq!(report.restored.len(), 2, "{report:?}");
                        assert!(
                            report
                                .restored
                                .iter()
                                .all(|(_, row)| row.skipped.is_empty())
                        );
                        completed.fetch_add(1, Ordering::Relaxed);
                    },
                },
            );
            assert_eq!(authorizations.load(Ordering::Relaxed), 2);
            assert_eq!(caps.load(Ordering::Relaxed), 2);
            assert_eq!(completed.load(Ordering::Relaxed), 1);
            assert_eq!(report.households[0].outcome.is_ok(), cancelled);
            if cancelled {
                assert_eq!(report.households[0].outcome, Ok(Ending::Cancelled));
            }
            for room in ["Alpha", "Beta"] {
                assert!(
                    row(&report, &sim, room).restored.is_empty(),
                    "untouched confirmation makes no writes"
                );
                assert!(row(&report, &sim, room).skipped.is_empty());
            }
            assert!(
                sim.soap_log()[from..]
                    .iter()
                    .all(|call| call.action.starts_with("Get"))
            );
        }
    }
}

#[test]
fn cancelling_after_the_first_zones_volume_preserves_both_sleep_continuations() {
    for s2 in [false, true] {
        let (sim, lan, households) = setup(s2);
        let control = AnnouncementControl::default();
        let caps = AtomicBool::new(false);
        let transport = Preparing {
            sim: &sim,
            lan: &lan,
            control: &control,
            caps: &caps,
            partial: true,
            during_snapshot: false,
            fail_snapshot: false,
            owner: Override::None,
            triggered: AtomicBool::new(false),
            after_owner: AtomicUsize::new(0),
        };
        let from = sim.soap_log().len();
        let report = announce(
            &transport,
            &households,
            &control,
            &AnnouncementCallbacks {
                clip_url: None,
                authorize: &|| Ok(()),
                cap: &|_| {
                    caps.store(true, Ordering::Release);
                    25
                },
                completed: &|_, _| {},
            },
        );
        assert_eq!(report.households[0].outcome, Ok(Ending::Cancelled));
        assert!(report.households[0].restore_failed.is_empty());
        assert_eq!(row(&report, &sim, "Alpha").restored, [Aspect::Volume]);
        assert!(row(&report, &sim, "Alpha").skipped.is_empty());
        assert!(row(&report, &sim, "Beta").restored.is_empty());
        assert!(row(&report, &sim, "Beta").skipped.is_empty());
        assert_eq!(
            soap::get_volume(&lan, sim.player("Alpha").unwrap().ip).unwrap(),
            40
        );
        assert_eq!(
            soap::get_volume(&lan, sim.player("Beta").unwrap().ip).unwrap(),
            37
        );
        assert!(sim.soap_log()[from..].iter().all(|call| {
            call.action.starts_with("Get") || (call.action == "SetVolume" && call.room == "Alpha")
        }));
    }
}

#[test]
fn external_changes_never_gain_an_old_sleep_receipt_during_cancelled_preparation() {
    for s2 in [false, true] {
        for partial in [false, true] {
            for change in [
                Override::Source,
                Override::Pause,
                Override::Group,
                Override::Volume,
                Override::Mute,
            ] {
                let (sim, lan, households) = setup(s2);
                let control = AnnouncementControl::default();
                let caps = AtomicBool::new(false);
                let authorizations = AtomicUsize::new(0);
                let transport = Preparing {
                    sim: &sim,
                    lan: &lan,
                    control: &control,
                    caps: &caps,
                    partial,
                    during_snapshot: false,
                    fail_snapshot: false,
                    owner: change,
                    triggered: AtomicBool::new(false),
                    after_owner: AtomicUsize::new(0),
                };
                let report = announce(
                    &transport,
                    &households,
                    &control,
                    &AnnouncementCallbacks {
                        clip_url: None,
                        authorize: &|| {
                            if authorizations.fetch_add(1, Ordering::Relaxed) == 1 && !partial {
                                owner_change(&sim, &lan, "Beta", change);
                                transport
                                    .after_owner
                                    .store(sim.soap_log().len(), Ordering::Release);
                                assert!(control.cancel());
                            }
                            Ok(())
                        },
                        cap: &|_| {
                            caps.store(true, Ordering::Release);
                            25
                        },
                        completed: &|_, _| {},
                    },
                );
                let room = if partial { "Alpha" } else { "Beta" };
                assert!(!row(&report, &sim, room).skipped.is_empty(), "{report:?}");
                let after = transport.after_owner.load(Ordering::Acquire);
                assert!(after > 0);
                assert!(
                    sim.soap_log()[after..].iter().all(|call| {
                        call.action.starts_with("Get") || call.action == "SetVolume"
                    }),
                    "cleanup must not change a newer source, transport or grouping"
                );
                let host = sim.player(room).unwrap().ip;
                match change {
                    Override::Source => {
                        assert_eq!(soap::get_media_info(&lan, host).unwrap().uri, NEW)
                    }
                    Override::Pause => assert_eq!(
                        soap::get_transport_info(&lan, host).unwrap().state,
                        TransportState::Paused
                    ),
                    Override::Volume => assert_eq!(soap::get_volume(&lan, host).unwrap(), 88),
                    Override::Mute => assert!(!soap::get_mute(&lan, host).unwrap()),
                    _ => {}
                }
            }
        }
    }
}

#[test]
fn an_external_source_during_snapshot_cannot_become_the_old_sleeps_baseline() {
    for s2 in [false, true] {
        let (sim, lan, households) = setup(s2);
        let control = AnnouncementControl::default();
        let caps = AtomicBool::new(false);
        let transport = Preparing {
            sim: &sim,
            lan: &lan,
            control: &control,
            caps: &caps,
            partial: false,
            during_snapshot: true,
            fail_snapshot: false,
            owner: Override::Source,
            triggered: AtomicBool::new(false),
            after_owner: AtomicUsize::new(0),
        };
        let report = announce(
            &transport,
            &households,
            &control,
            &AnnouncementCallbacks {
                clip_url: None,
                authorize: &|| Ok(()),
                cap: &|_| {
                    caps.store(true, Ordering::Release);
                    25
                },
                completed: &|_, _| {},
            },
        );
        assert!(report.households[0].outcome.is_err());
        assert!(!row(&report, &sim, "Beta").skipped.is_empty());
        assert!(row(&report, &sim, "Alpha").skipped.is_empty());
        assert_eq!(
            soap::get_media_info(&lan, sim.player("Beta").unwrap().ip)
                .unwrap()
                .uri,
            NEW
        );
        assert!(
            sim.soap_log()[transport.after_owner.load(Ordering::Acquire)..]
                .iter()
                .all(|call| call.action.starts_with("Get"))
        );
    }
}

#[test]
fn a_later_snapshot_failure_still_verifies_the_already_captured_zone() {
    for s2 in [false, true] {
        let (sim, lan, households) = setup(s2);
        let control = AnnouncementControl::default();
        let caps = AtomicBool::new(false);
        let transport = Preparing {
            sim: &sim,
            lan: &lan,
            control: &control,
            caps: &caps,
            partial: false,
            during_snapshot: false,
            fail_snapshot: true,
            owner: Override::None,
            triggered: AtomicBool::new(false),
            after_owner: AtomicUsize::new(0),
        };
        let from = sim.soap_log().len();
        let report = announce(
            &transport,
            &households,
            &control,
            &AnnouncementCallbacks {
                clip_url: None,
                authorize: &|| Ok(()),
                cap: &|_| {
                    caps.store(true, Ordering::Release);
                    25
                },
                completed: &|_, _| {},
            },
        );
        assert!(report.households[0].outcome.is_err());
        assert_eq!(report.households[0].restored.len(), 1);
        assert!(row(&report, &sim, "Alpha").skipped.is_empty());
        assert!(
            sim.soap_log()[from..]
                .iter()
                .all(|call| call.action.starts_with("Get"))
        );
    }
}

#[test]
fn a_queue_replacement_during_source_admission_never_interrupts_the_old_sleep() {
    struct Replacing<'a> {
        sim: &'a SimHandle,
        lan: &'a SimLan,
        changed: AtomicBool,
        after: AtomicUsize,
    }
    impl Transport for Replacing<'_> {
        fn soap_post(
            &self,
            host: IpAddr,
            path: &str,
            action: &str,
            body: &str,
        ) -> Result<String, ProtoError> {
            let result = self.lan.soap_post(host, path, action, body);
            if result.is_ok()
                && host == self.sim.player("Alpha").unwrap().ip
                && action.contains("#GetPositionInfo")
                && !self.changed.swap(true, Ordering::AcqRel)
            {
                soap::remove_all_tracks_from_queue(self.lan, host)?;
                soap::add_uri_to_queue(
                    self.lan,
                    host,
                    "http://example.invalid/replaced.mp3",
                    "",
                    false,
                )?;
                soap::seek_track(self.lan, host, 1)?;
                soap::play(self.lan, host)?;
                self.after
                    .store(self.sim.soap_log().len(), Ordering::Release);
            }
            result
        }
    }
    for s2 in [false, true] {
        let (sim, lan, households) = setup(s2);
        let player = sim.player("Alpha").unwrap();
        soap::add_uri_to_queue(
            &lan,
            player.ip,
            "http://example.invalid/original.mp3",
            "",
            false,
        )
        .unwrap();
        soap::set_av_transport_uri(
            &lan,
            player.ip,
            &format!("x-rincon-queue:{}#0", player.uuid),
            "",
        )
        .unwrap();
        soap::play(&lan, player.ip).unwrap();
        let transport = Replacing {
            sim: &sim,
            lan: &lan,
            changed: AtomicBool::new(false),
            after: AtomicUsize::new(0),
        };
        let report = announce(
            &transport,
            &households,
            &AnnouncementControl::default(),
            &AnnouncementCallbacks {
                clip_url: None,
                authorize: &|| Ok(()),
                cap: &|_| {
                    panic!("a torn queue/source observation must fail before suspending sleep")
                },
                completed: &|_, _| {},
            },
        );
        assert!(report.households[0].outcome.is_err());
        assert!(report.households[0].levels.is_empty());
        assert!(report.households[0].restored.is_empty());
        assert_eq!(
            soap::get_position_info(&lan, player.ip).unwrap().uri,
            "http://example.invalid/replaced.mp3"
        );
        assert!(
            sim.soap_log()[transport.after.load(Ordering::Acquire)..]
                .iter()
                .all(|call| call.action.starts_with("Get") || call.action == "Browse")
        );
    }
}

#[test]
fn a_cleanup_retry_rechecks_preparation_ownership_before_another_write() {
    struct Settling<'a> {
        sim: &'a SimHandle,
        lan: &'a SimLan,
        control: &'a AnnouncementControl,
        prepared: AtomicBool,
        players: usize,
        topology_reads: AtomicUsize,
        refusals: AtomicUsize,
        cleanup_attempts: AtomicUsize,
        after_owner: AtomicUsize,
    }
    impl Transport for Settling<'_> {
        fn soap_post(
            &self,
            host: IpAddr,
            path: &str,
            action: &str,
            body: &str,
        ) -> Result<String, ProtoError> {
            let volume =
                action.contains("#SetVolume") && host == self.sim.player("Alpha").unwrap().ip;
            if self.prepared.load(Ordering::Acquire) {
                if volume {
                    self.cleanup_attempts.fetch_add(1, Ordering::AcqRel);
                }
                if action.contains("#GetZoneGroupState") {
                    let read = self.topology_reads.fetch_add(1, Ordering::AcqRel);
                    // The initial source_current proof brackets source reads
                    // with two topology reads. Fail the next refreshed() call
                    // on every fallback player, so restore() itself returns
                    // 402 and actually exercises its second restore attempt.
                    if (2..2 + self.players).contains(&read) {
                        if read == 2 {
                            owner_change(self.sim, self.lan, "Alpha", Override::Source);
                            self.after_owner
                                .store(self.sim.soap_log().len(), Ordering::Release);
                        }
                        self.refusals.fetch_add(1, Ordering::AcqRel);
                        return Err(ProtoError::SoapFault {
                            code: 402,
                            reason: "synthetic cleanup topology settling refusal".into(),
                        });
                    }
                }
            }
            let result = self.lan.soap_post(host, path, action, body);
            if result.is_ok() && volume && !self.prepared.swap(true, Ordering::AcqRel) {
                assert!(self.control.cancel());
            }
            result
        }
    }
    for s2 in [false, true] {
        let (sim, lan, households) = setup(s2);
        let control = AnnouncementControl::default();
        let transport = Settling {
            sim: &sim,
            lan: &lan,
            control: &control,
            prepared: AtomicBool::new(false),
            players: households[0].players.len(),
            topology_reads: AtomicUsize::new(0),
            refusals: AtomicUsize::new(0),
            cleanup_attempts: AtomicUsize::new(0),
            after_owner: AtomicUsize::new(0),
        };
        let report = announce(
            &transport,
            &households,
            &control,
            &AnnouncementCallbacks {
                clip_url: None,
                authorize: &|| Ok(()),
                cap: &|_| 25,
                completed: &|_, _| {},
            },
        );
        assert_eq!(
            transport.refusals.load(Ordering::Acquire),
            transport.players
        );
        assert!(
            transport.topology_reads.load(Ordering::Acquire) > 2 + transport.players,
            "the initial cleanup topology refusal was followed by the retry"
        );
        assert_eq!(
            transport.cleanup_attempts.load(Ordering::Acquire),
            0,
            "the retried cleanup must recheck source ownership before SetVolume"
        );
        assert!(!row(&report, &sim, "Alpha").skipped.is_empty());
        assert!(row(&report, &sim, "Beta").skipped.is_empty());
        let after = transport.after_owner.load(Ordering::Acquire);
        assert!(after > 0);
        assert!(
            sim.soap_log()[after..]
                .iter()
                .all(|call| call.action.starts_with("Get"))
        );
        let host = sim.player("Alpha").unwrap().ip;
        assert_eq!(soap::get_media_info(&lan, host).unwrap().uri, NEW);
        assert_eq!(soap::get_volume(&lan, host).unwrap(), 25);
    }
}
