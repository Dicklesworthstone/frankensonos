//! Cancellation owns only its queued turn or still-confirmed announcement clip.

#[path = "announce_cancel_sim/admission.rs"]
mod admission;

use fsonos_core::announce::{
    AnnounceReport, AnnouncementCallbacks, AnnouncementControl, AnnouncementPhase, Announcer, Clip,
    Ending,
};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

const CLIP: &str = "http://192.0.2.250:3400/media/cancellation.wav";
const OLD: &str = "x-rincon-mp3radio://stream.example.invalid/original";
const OTHER: &str = "x-rincon-mp3radio://stream.example.invalid/other";
const NEW: &str = "x-rincon-mp3radio://stream.example.invalid/newer";

fn simulator(s2: bool) -> SimHandle {
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
    let mut household = HouseholdState::default();
    household.apply_topology(&get_zone_group_state(lan, sim.player("Alpha").unwrap().ip).unwrap());
    vec![household]
}

fn clip() -> Clip {
    Clip {
        url: CLIP.into(),
        title: "Synthetic cancellation".into(),
        duration: Duration::from_secs(60),
    }
}

fn announcer() -> Announcer {
    Announcer::with_timing(Duration::from_millis(1), Duration::ZERO)
}

#[derive(Clone, Copy)]
enum CancelAt {
    Volume,
    Source,
    Playing,
}

#[derive(Clone, Copy)]
enum Owner {
    None,
    Source,
    Pause,
    Group,
    SourceAfterStop,
}

#[derive(Clone, Copy)]
enum StopReply {
    Normal,
    Lost,
    Refused,
    Unobservable,
}

struct Cancelling<'a> {
    sim: &'a SimHandle,
    lan: &'a SimLan,
    control: &'a AnnouncementControl,
    at: CancelAt,
    owner: Owner,
    stop_reply: StopReply,
    installed: AtomicBool,
    cancelled: AtomicBool,
    changed: AtomicBool,
    unreadable: AtomicBool,
    stops: AtomicUsize,
    after: AtomicUsize,
}

impl Cancelling<'_> {
    fn owner_change(&self) -> Result<(), ProtoError> {
        let lead = self.sim.player("Alpha").unwrap().ip;
        match self.owner {
            Owner::Source | Owner::SourceAfterStop => {
                soap::set_av_transport_uri(self.lan, lead, NEW, "")?;
                soap::play(self.lan, lead)?;
            }
            Owner::Pause => soap::pause(self.lan, lead)?,
            Owner::Group => soap::join_group(
                self.lan,
                self.sim.player("Beta").unwrap().ip,
                &PlayerId(self.sim.player("Gamma").unwrap().uuid.clone()),
            )?,
            Owner::None => {}
        }
        self.after
            .store(self.sim.soap_log().len(), Ordering::Release);
        Ok(())
    }

    fn request_cancel(&self) {
        if !self.cancelled.swap(true, Ordering::AcqRel) {
            assert!(self.control.cancel(), "the first cancellation was accepted");
        }
    }
}

impl Transport for Cancelling<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if self.unreadable.load(Ordering::Acquire) && action.contains("#Get") {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "synthetic post-cancellation read failure".into(),
            });
        }
        // The first watcher state/source inspection can still look healthy.
        // Change the owner at its subsequent fresh group check before Stop.
        if self.cancelled.load(Ordering::Acquire)
            && action.contains("#GetZoneGroupState")
            && matches!(self.owner, Owner::Source | Owner::Pause | Owner::Group)
            && !self.changed.swap(true, Ordering::AcqRel)
        {
            self.owner_change()?;
        }
        let stop = self.cancelled.load(Ordering::Acquire) && action.contains("#Stop");
        if stop {
            self.stops.fetch_add(1, Ordering::AcqRel);
            if matches!(self.stop_reply, StopReply::Refused) {
                self.after
                    .store(self.sim.soap_log().len(), Ordering::Release);
                return Err(ProtoError::SoapFault {
                    code: 701,
                    reason: "synthetic cancellation Stop refused".into(),
                });
            }
        }
        let result = self.lan.soap_post(host, path, action, body);
        if result.is_ok() {
            if action.contains("#SetAVTransportURI") && body.contains(CLIP) {
                self.installed.store(true, Ordering::Release);
                if matches!(self.at, CancelAt::Source) {
                    self.request_cancel();
                }
            }
            if matches!(self.at, CancelAt::Volume) && action.contains("#SetVolume") {
                self.request_cancel();
            }
            if matches!(self.at, CancelAt::Playing)
                && self.installed.load(Ordering::Acquire)
                && action.contains("#Play\"")
            {
                self.request_cancel();
            }
            if stop {
                self.after
                    .store(self.sim.soap_log().len(), Ordering::Release);
                if matches!(self.owner, Owner::SourceAfterStop)
                    && !self.changed.swap(true, Ordering::AcqRel)
                {
                    self.owner_change()?;
                }
                if matches!(self.stop_reply, StopReply::Unobservable) {
                    self.unreadable.store(true, Ordering::Release);
                }
                if matches!(self.stop_reply, StopReply::Lost | StopReply::Unobservable) {
                    return Err(ProtoError::Network {
                        target: host.to_string(),
                        detail: "synthetic lost cancellation Stop reply".into(),
                    });
                }
            }
        }
        result
    }
}

struct ResultOf {
    sim: SimHandle,
    report: AnnounceReport,
    control: AnnouncementControl,
    stops: usize,
    after: usize,
    from: usize,
}

fn run(s2: bool, at: CancelAt, owner: Owner, stop_reply: StopReply) -> ResultOf {
    let sim = simulator(s2);
    let lan = sim.lan();
    for (room, source, level) in [("Alpha", OLD, 40), ("Beta", OTHER, 37)] {
        let host = sim.player(room).unwrap().ip;
        soap::set_av_transport_uri(&lan, host, source, "").unwrap();
        soap::play(&lan, host).unwrap();
        soap::set_volume(&lan, host, level).unwrap();
        soap::set_mute(&lan, host, true).unwrap();
    }
    let lead = sim.player("Alpha").unwrap().ip;
    for uri in [
        "http://example.invalid/one.mp3",
        "http://example.invalid/two.mp3",
    ] {
        soap::add_uri_to_queue(&lan, lead, uri, "", false).unwrap();
    }
    let h = houses(&sim, &lan);
    let control = AnnouncementControl::default();
    let transport = Cancelling {
        sim: &sim,
        lan: &lan,
        control: &control,
        at,
        owner,
        stop_reply,
        installed: AtomicBool::new(false),
        cancelled: AtomicBool::new(false),
        changed: AtomicBool::new(false),
        unreadable: AtomicBool::new(false),
        stops: AtomicUsize::new(0),
        after: AtomicUsize::new(0),
    };
    let completed = AtomicUsize::new(0);
    let from = sim.soap_log().len();
    let report = announcer()
        .announce_controlled(
            &transport,
            &[
                resolve_room(&h, "Alpha").unwrap(),
                resolve_room(&h, "Beta").unwrap(),
            ],
            &clip(),
            25,
            None,
            &AnnouncementCallbacks {
                authorize: &|| Ok(()),
                cap: &|_| 25,
                completed: &|_, _| {
                    assert_eq!(control.progress()[0].phase, AnnouncementPhase::Restoring);
                    completed.fetch_add(1, Ordering::Relaxed);
                },
            },
            &control,
        )
        .unwrap();
    assert_eq!(completed.load(Ordering::Relaxed), 1);
    assert!(control.is_cancelled());
    assert_eq!(control.progress()[0].phase, AnnouncementPhase::Complete);
    assert!(
        !control.cancel(),
        "a completed request cannot be cancelled again"
    );
    let stops = transport.stops.load(Ordering::Acquire);
    let after = transport.after.load(Ordering::Acquire);
    ResultOf {
        sim,
        report,
        control,
        stops,
        after,
        from,
    }
}

fn restored(result: &ResultOf) {
    let report = &result.report.households[0];
    assert_eq!(report.outcome, Ok(Ending::Cancelled), "{report:?}");
    assert!(report.restore_failed.is_empty(), "{report:?}");
    assert!(
        report.restored.iter().all(|(_, r)| r.skipped.is_empty()),
        "{report:?}"
    );
    let lan = result.sim.lan();
    for (room, source, level) in [("Alpha", OLD, 40), ("Beta", OTHER, 37)] {
        let host = result.sim.player(room).unwrap().ip;
        assert_eq!(soap::get_media_info(&lan, host).unwrap().uri, source);
        assert_eq!(
            soap::get_transport_info(&lan, host).unwrap().state,
            TransportState::Playing
        );
        assert_eq!(soap::get_volume(&lan, host).unwrap(), level);
        assert!(soap::get_mute(&lan, host).unwrap());
    }
    let h = houses(&result.sim, &lan);
    for room in ["Alpha", "Beta"] {
        let target = resolve_room(&h, room).unwrap();
        assert_eq!(target.coordinator.id, target.player.id);
    }
    let queue = browse_snapshot(&lan, result.sim.player("Alpha").unwrap().ip, QUEUE).unwrap();
    assert_eq!(queue.objects.len(), 2);
    assert!(
        result.sim.soap_log()[result.from..].iter().all(|call| {
            !matches!(
                call.action.as_str(),
                "AddURIToQueue" | "RemoveAllTracksFromQueue" | "RemoveTrackRangeFromQueue"
            )
        }),
        "announcement cancellation must not reconstruct or clear the retained queue"
    );
}

#[test]
fn cancellation_before_admission_is_read_only_and_never_calls_boundary_hooks() {
    for s2 in [false, true] {
        let sim = simulator(s2);
        let lan = sim.lan();
        let h = houses(&sim, &lan);
        let control = AnnouncementControl::default();
        assert!(control.cancel());
        let from = sim.soap_log().len();
        let report = announcer()
            .announce_controlled(
                &lan,
                &[resolve_room(&h, "Alpha").unwrap()],
                &clip(),
                25,
                None,
                &AnnouncementCallbacks {
                    authorize: &|| panic!("cancelled queued work never reaches admission"),
                    cap: &|_| panic!("cancelled queued work must not suspend a fade"),
                    completed: &|_, _| panic!("there is no effect to restore"),
                },
                &control,
            )
            .unwrap();
        assert_eq!(sim.soap_log().len(), from);
        assert_eq!(report.households[0].outcome, Ok(Ending::Cancelled));
        assert!(report.households[0].restored.is_empty());
        assert_eq!(control.progress()[0].phase, AnnouncementPhase::Complete);
        assert!(
            announcer()
                .announce_controlled(
                    &lan,
                    &[resolve_room(&h, "Alpha").unwrap()],
                    &clip(),
                    25,
                    None,
                    &AnnouncementCallbacks {
                        authorize: &|| Ok(()),
                        cap: &|_| 25,
                        completed: &|_, _| {}
                    },
                    &control,
                )
                .is_err(),
            "a control token cannot admit a second announcement"
        );
        assert_eq!(sim.soap_log().len(), from);
    }
}

#[test]
fn an_active_cancel_stops_once_and_restores_sources_grouping_levels_and_queues() {
    for s2 in [false, true] {
        let result = run(s2, CancelAt::Playing, Owner::None, StopReply::Normal);
        restored(&result);
        assert_eq!(result.stops, 1);
    }
}

#[test]
fn preparation_cancel_restores_partial_levels_without_playing_the_clip() {
    for s2 in [false, true] {
        let result = run(s2, CancelAt::Volume, Owner::None, StopReply::Normal);
        restored(&result);
        assert_eq!(result.stops, 0);
        assert!(result.sim.soap_log()[result.from..].iter().all(|call| {
            call.action != "SetAVTransportURI" || call.args.iter().all(|(_, value)| value != CLIP)
        }));
    }
}

#[test]
fn a_cancel_during_source_install_never_sends_the_clips_play() {
    for s2 in [false, true] {
        let result = run(s2, CancelAt::Source, Owner::None, StopReply::Normal);
        restored(&result);
        assert_eq!(result.stops, 0);
        let log = result.sim.soap_log();
        let selected = log
            .iter()
            .position(|call| {
                call.action == "SetAVTransportURI"
                    && call.args.iter().any(|(_, value)| value == CLIP)
            })
            .unwrap();
        let restored_source = log
            .iter()
            .enumerate()
            .skip(selected + 1)
            .find(|(_, call)| {
                call.action == "SetAVTransportURI"
                    && call
                        .args
                        .iter()
                        .any(|(_, value)| value == OLD || value == OTHER)
            })
            .map(|(index, _)| index)
            .unwrap();
        assert!(
            log[selected + 1..restored_source]
                .iter()
                .all(|call| call.action != "Play")
        );
    }
}

#[test]
fn new_sources_pauses_and_groups_win_over_a_cancel_before_stop() {
    for s2 in [false, true] {
        for owner in [Owner::Source, Owner::Pause, Owner::Group] {
            let result = run(s2, CancelAt::Playing, owner, StopReply::Normal);
            let report = &result.report.households[0];
            assert!(report.outcome.is_err(), "{report:?}");
            assert_eq!(result.stops, 0);
            assert!(result.after > result.from, "the owner intervention ran");
            assert!(
                result.sim.soap_log()[result.after..]
                    .iter()
                    .all(|call| call.action.starts_with("Get"))
            );
            assert!(report.restored.iter().any(|(_, r)| !r.skipped.is_empty()));
        }
    }
}

#[test]
fn a_lost_stop_reply_is_confirmed_without_repeating_stop() {
    for s2 in [false, true] {
        let result = run(s2, CancelAt::Playing, Owner::None, StopReply::Lost);
        restored(&result);
        assert_eq!(result.stops, 1);
    }
}

#[test]
fn refused_or_unobservable_stops_never_restart_or_replay_automatically() {
    for s2 in [false, true] {
        for reply in [StopReply::Refused, StopReply::Unobservable] {
            let result = run(s2, CancelAt::Playing, Owner::None, reply);
            assert!(result.report.households[0].outcome.is_err());
            assert_eq!(result.stops, 1);
            assert!(
                result.sim.soap_log()[result.after..]
                    .iter()
                    .all(|call| call.action.starts_with("Get"))
            );
            assert!(
                result.report.households[0]
                    .restored
                    .iter()
                    .any(|(_, r)| !r.skipped.is_empty())
            );
        }
    }
}

#[test]
fn a_new_source_during_cancel_stop_is_not_overwritten_by_restore() {
    for s2 in [false, true] {
        let result = run(
            s2,
            CancelAt::Playing,
            Owner::SourceAfterStop,
            StopReply::Normal,
        );
        assert!(result.report.households[0].outcome.is_err());
        assert_eq!(result.stops, 1);
        assert!(
            result.sim.soap_log()[result.after..]
                .iter()
                .all(|call| call.action.starts_with("Get"))
        );
        assert_eq!(
            soap::get_media_info(&result.sim.lan(), result.sim.player("Alpha").unwrap().ip)
                .unwrap()
                .uri,
            NEW
        );
        assert!(result.control.is_cancelled());
    }
}
