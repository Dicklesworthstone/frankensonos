//! File publication and restart use the same checkpoints as the daemon worker.

use super::super::fades::Checkpoint;
use super::*;
use fsonos_core::control;
use fsonos_core::sleep::{SleepCheckpoint, SleepOutcome};
use fsonos_proto::control as soap;
use fsonos_types::TransportState;
use std::panic::{AssertUnwindSafe, catch_unwind};

struct Crash<'a> {
    inner: &'a dyn Transport,
    action: &'static str,
    remaining: AtomicUsize,
}

impl Transport for Crash<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.inner.soap_post(host, path, header, body)?;
        assert!(
            !(header.contains(self.action) && self.remaining.fetch_sub(1, Ordering::SeqCst) == 1),
            "injected process stop after an accepted speaker mutation"
        );
        Ok(reply)
    }
}

fn playing(h: &House) {
    let lan = h.sim.lan();
    let households = h.rooms.lock().unwrap().clone();
    let coordinator = h.id("RecoverA");
    control::queue_spotify_tracks(
        &lan,
        &households,
        &coordinator,
        &[("spotify:track:0SimSavedSleep000001", "Night")],
    )
    .unwrap()
    .unwrap();
    control::play_queue_from(&lan, &households, &coordinator, 1).unwrap();
    control::join(&lan, &households, &h.id("RecoverB"), &coordinator).unwrap();
    h.rooms.lock().unwrap()[0]
        .apply_topology(&get_zone_group_state(&lan, h.ip("RecoverA")).unwrap());
    soap::set_volume(&lan, h.ip("RecoverA"), 30).unwrap();
    soap::set_volume(&lan, h.ip("RecoverB"), 20).unwrap();
}

fn crash(h: &House, action: &'static str, writes: usize) -> SavedTimers {
    playing(h);
    let surface = h.surface();
    set(&surface, "RecoverA", "1s", false);
    let sleep = sleep(&surface);
    let timer = sleep.timers.get(&h.id("RecoverA")).unwrap();
    let ticket = sleep.timers.due_runs(timer.ends, 1).remove(0);
    let households = h.rooms.lock().unwrap().clone();
    assert!(
        catch_unwind(AssertUnwindSafe(|| {
            let checkpoint = Checkpoint::new(sleep);
            sleep
                .timers
                .run_due_with_checkpoint(
                    &Crash {
                        inner: &*surface.transport,
                        action,
                        remaining: AtomicUsize::new(writes),
                    },
                    &households,
                    &sleep.fader,
                    &ticket,
                    &checkpoint,
                )
                .unwrap();
        }))
        .is_err()
    );
    let saved = read(h);
    assert_eq!(saved.fades.len(), 1);
    assert_eq!(saved.timers.len(), 1);
    saved
}

fn read(h: &House) -> SavedTimers {
    serde_json::from_slice(&std::fs::read(h.scratch.path()).unwrap()).unwrap()
}

fn levels(h: &House) -> (u8, u8) {
    (
        soap::get_volume(&h.sim.lan(), h.ip("RecoverA")).unwrap(),
        soap::get_volume(&h.sim.lan(), h.ip("RecoverB")).unwrap(),
    )
}

#[test]
fn a_real_saved_journal_resumes_the_original_baseline_after_restart() {
    for s2 in [false, true] {
        let h = House::new(s2);
        let saved = crash(&h, "#SetVolume", 2);
        assert_eq!(levels(&h), (0, 0));
        let encoded = serde_json::to_value(&saved.fades[0]).unwrap();
        let originals: Vec<_> = encoded["levels"]
            .as_array()
            .unwrap()
            .iter()
            .map(|level| level["original"].as_u64().unwrap())
            .collect();
        assert!(originals.contains(&30) && originals.contains(&20));
        assert!(
            encoded["levels"]
                .as_array()
                .unwrap()
                .iter()
                .all(|level| level["pending"] == 0)
        );

        let restarted = h.surface();
        assert_eq!(restarted.adopt_sleep_timers(), 1);
        let sleep = sleep(&restarted);
        let timer = sleep.timers.get(&h.id("RecoverA")).unwrap();
        assert_eq!(timer.ends, saved.timers[0].ends);
        let ticket = sleep.timers.due_runs(timer.ends, 1).remove(0);
        let checkpoint = Checkpoint::new(sleep);
        assert_eq!(
            sleep
                .timers
                .run_due_with_checkpoint(
                    &*restarted.transport,
                    &h.rooms.lock().unwrap(),
                    &sleep.fader,
                    &ticket,
                    &checkpoint,
                )
                .unwrap(),
            SleepOutcome::Paused
        );
        assert_eq!(levels(&h), (30, 20));
        assert!(read(&h).fades.is_empty());
        assert!(read(&h).timers.is_empty());
        assert_eq!(h.surface().adopt_sleep_timers(), 0);
        assert_eq!(
            h.sim
                .soap_log()
                .iter()
                .filter(|e| e.action == "Pause")
                .count(),
            1
        );
    }
}

#[test]
fn an_uncertain_accepted_stop_restores_levels_without_repeating_the_stop() {
    let h = House::new(true);
    crash(&h, "#Pause", 1);
    let before = h.sim.soap_log().len();
    assert_eq!(h.surface().adopt_sleep_timers(), 0);
    assert_eq!(levels(&h), (30, 20));
    assert_eq!(
        soap::get_transport_info(&h.sim.lan(), h.ip("RecoverA"))
            .unwrap()
            .state,
        TransportState::Paused
    );
    assert_eq!(
        get_remaining_sleep_timer(&h.sim.lan(), h.ip("RecoverA")).unwrap(),
        None
    );
    assert!(read(&h).fades.is_empty());
    assert!(read(&h).timers.is_empty());
    assert!(
        !h.sim.soap_log()[before..]
            .iter()
            .any(|e| matches!(e.action.as_str(), "Pause" | "Play" | "SetAVTransportURI"))
    );
}

#[test]
fn changed_volume_survives_recovery_and_retires_the_matching_backstop() {
    let h = House::new(true);
    crash(&h, "#SetVolume", 2);
    soap::set_volume(&h.sim.lan(), h.ip("RecoverB"), 47).unwrap();
    let before = h.sim.soap_log().len();
    assert_eq!(h.surface().adopt_sleep_timers(), 0);
    assert_eq!(levels(&h), (30, 47));
    assert_eq!(
        get_remaining_sleep_timer(&h.sim.lan(), h.ip("RecoverA")).unwrap(),
        None
    );
    assert!(read(&h).fades.is_empty());
    assert!(
        !h.sim.soap_log()[before..]
            .iter()
            .any(|e| e.action == "Pause")
    );
}

#[test]
fn unavailable_recovery_evidence_keeps_the_journal_through_another_restart() {
    let h = House::new(true);
    let saved = crash(&h, "#SetVolume", 2);
    h.replies.fail.lock().unwrap().insert(h.ip("RecoverA"));
    for _ in 0..2 {
        let restarted = h.surface();
        assert_eq!(restarted.adopt_sleep_timers(), 0);
        sleep(&restarted).save();
        assert_eq!(read(&h).fades, saved.fades);
    }
    h.replies.fail.lock().unwrap().clear();
    assert_eq!(h.surface().adopt_sleep_timers(), 1);
}

#[test]
fn failed_journal_publication_makes_no_volume_or_pause_write() {
    let h = House::new(true);
    playing(&h);
    std::fs::create_dir(h.scratch.path()).unwrap();
    let surface = h.surface();
    set(&surface, "RecoverA", "1s", false);
    let sleep = sleep(&surface);
    let timer = sleep.timers.get(&h.id("RecoverA")).unwrap();
    let ticket = sleep.timers.due_runs(timer.ends, 1).remove(0);
    let before = h.sim.soap_log().len();
    assert!(
        sleep
            .timers
            .run_due_with_checkpoint(
                &*surface.transport,
                &h.rooms.lock().unwrap(),
                &sleep.fader,
                &ticket,
                &Checkpoint::new(sleep),
            )
            .is_err()
    );
    assert_eq!(levels(&h), (30, 20));
    assert!(
        get_remaining_sleep_timer(&h.sim.lan(), h.ip("RecoverA"))
            .unwrap()
            .is_some()
    );
    assert!(
        !h.sim.soap_log()[before..]
            .iter()
            .any(|e| matches!(e.action.as_str(), "SetVolume" | "Pause"))
    );
}

#[test]
fn cancel_or_replace_keeps_cleanup_until_the_original_levels_are_restored() {
    for cancel in [false, true] {
        let h = House::new(true);
        crash(&h, "#SetVolume", 2);
        let restarted = h.surface();
        if cancel {
            cancel_timer(&restarted, "RecoverA");
        } else {
            set(&restarted, "RecoverA", "10m", false);
        }
        assert_eq!(
            read(&h).fades.len(),
            1,
            "timer mutation keeps unfinished cleanup"
        );
        let before = h.sim.soap_log().len();
        assert_eq!(restarted.adopt_sleep_timers(), 0);
        assert_eq!(levels(&h), (30, 20));
        assert!(read(&h).fades.is_empty());
        assert_eq!(read(&h).timers.len(), usize::from(!cancel));
        assert_eq!(
            get_remaining_sleep_timer(&h.sim.lan(), h.ip("RecoverA"))
                .unwrap()
                .is_some(),
            !cancel
        );
        assert!(
            !h.sim.soap_log()[before..]
                .iter()
                .any(|e| e.action == "Pause")
        );
    }
}

#[test]
fn a_manual_intent_during_a_recovery_read_fences_every_late_volume_write() {
    let h = House::new(true);
    crash(&h, "#SetVolume", 2);
    let restarted = h.surface();
    let (entered, resume) = h.hold_read();
    let worker = Arc::clone(&restarted);
    let join = std::thread::spawn(move || worker.adopt_sleep_timers());
    entered.recv_timeout(Duration::from_secs(5)).unwrap();
    let before = h.sim.soap_log().len();
    sleep(&restarted).volume_changed(&h.rooms.lock().unwrap(), &h.id("RecoverA"));
    soap::set_volume(&h.sim.lan(), h.ip("RecoverA"), 39).unwrap();
    resume.send(()).unwrap();
    assert_eq!(join.join().unwrap(), 0);
    assert_eq!(levels(&h), (39, 0));
    assert!(read(&h).fades.is_empty());
    assert!(read(&h).timers.is_empty());
    let log = h.sim.soap_log();
    assert_eq!(
        log[before..]
            .iter()
            .filter(|e| e.action == "SetVolume")
            .count(),
        1
    );
    assert!(!log[before..].iter().any(|e| e.action == "Pause"));
}

#[test]
fn dropping_the_recovery_guard_cannot_release_a_resumed_workers_journal() {
    let h = House::new(true);
    let fade = crash(&h, "#SetVolume", 2).fades.remove(0);
    let restarted = h.surface();
    let sleep = sleep(&restarted);
    assert_eq!(sleep.saved_timers().len(), 1);
    let recovery = Checkpoint::existing(sleep, &fade).unwrap();
    // The verified adoption hands this exact journal to its due worker.
    sleep
        .writing
        .lock()
        .unwrap()
        .resumed_fades
        .insert(fade.id.clone());
    let worker = Checkpoint::new(sleep);
    worker.save(&fade).unwrap();
    assert!(worker.current(&fade));
    drop(recovery);
    assert!(worker.current(&fade));
    assert!(super::super::fades::pending(sleep).is_empty());
    worker.complete(&fade).unwrap();
}

struct HoldCapture<'a> {
    inner: &'a dyn Transport,
    wait: Mutex<Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>>,
}

impl Transport for HoldCapture<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let reply = self.inner.soap_post(host, path, header, body)?;
        if header.contains("#GetMediaInfo") {
            let wait = self.wait.lock().unwrap().take();
            if let Some((arrived, resume)) = wait {
                arrived.send(()).unwrap();
                resume.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        }
        Ok(reply)
    }
}

#[test]
fn a_new_intent_during_initial_capture_retires_the_file_before_the_first_write() {
    for playback in [false, true] {
        let h = House::new(true);
        playing(&h);
        let surface = h.surface();
        set(&surface, "RecoverA", "1s", false);
        let timer = sleep(&surface).timers.get(&h.id("RecoverA")).unwrap();
        let ticket = sleep(&surface).timers.due_runs(timer.ends, 1).remove(0);
        let households = h.rooms.lock().unwrap().clone();
        let (arrived, entered) = mpsc::sync_channel(1);
        let (resume, waiting) = mpsc::channel();
        let worker = Arc::clone(&surface);
        let join = std::thread::spawn(move || {
            let sleep = sleep(&worker);
            let checkpoint = Checkpoint::new(sleep);
            let transport = HoldCapture {
                inner: &*worker.transport,
                wait: Mutex::new(Some((arrived, waiting))),
            };
            let result = sleep.timers.run_due_with_checkpoint(
                &transport,
                &households,
                &sleep.fader,
                &ticket,
                &checkpoint,
            );
            sleep.save();
            result
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let before = h.sim.soap_log().len();
        if playback {
            sleep(&surface).playback_changing(&h.id("RecoverA"));
            soap::set_av_transport_uri(
                &h.sim.lan(),
                h.ip("RecoverA"),
                "https://example.invalid/new-night-stream",
                "",
            )
            .unwrap();
        } else {
            sleep(&surface).volume_changed(&h.rooms.lock().unwrap(), &h.id("RecoverA"));
            soap::set_volume(&h.sim.lan(), h.ip("RecoverA"), 39).unwrap();
        }
        // A process stop here sees neither the obsolete due timer nor a fade
        // journal, even though the old worker has not returned from its read.
        assert!(read(&h).timers.is_empty());
        assert!(read(&h).fades.is_empty());
        assert_eq!(h.surface().adopt_sleep_timers(), 0);
        resume.send(()).unwrap();
        let error = join.join().unwrap().unwrap_err();
        assert!(
            error.to_string().contains("newer command superseded"),
            "{error}"
        );
        assert_eq!(levels(&h), (if playback { 30 } else { 39 }, 20));
        let log = h.sim.soap_log();
        assert_eq!(
            log[before..]
                .iter()
                .filter(|e| e.action == "SetVolume")
                .count(),
            usize::from(!playback)
        );
        assert!(!log[before..].iter().any(|e| e.action == "Pause"));
        assert!(read(&h).fades.is_empty());
    }
}

#[test]
fn playback_changed_before_checkpoint_creation_cannot_be_faded_by_an_old_ticket() {
    let h = House::new(true);
    playing(&h);
    let surface = h.surface();
    set(&surface, "RecoverA", "1s", false);
    let sleep = sleep(&surface);
    let timer = sleep.timers.get(&h.id("RecoverA")).unwrap();
    let ticket = sleep.timers.due_runs(timer.ends, 1).remove(0);
    sleep.playback_changing(&h.id("RecoverA"));
    soap::set_av_transport_uri(
        &h.sim.lan(),
        h.ip("RecoverA"),
        "https://example.invalid/new-night-stream",
        "",
    )
    .unwrap();
    let before = h.sim.soap_log().len();
    let checkpoint = Checkpoint::new(sleep);
    assert_eq!(
        sleep
            .timers
            .run_due_with_checkpoint(
                &*surface.transport,
                &h.rooms.lock().unwrap(),
                &sleep.fader,
                &ticket,
                &checkpoint,
            )
            .unwrap(),
        SleepOutcome::Interrupted
    );
    assert_eq!(levels(&h), (30, 20));
    assert!(read(&h).timers.is_empty());
    assert!(read(&h).fades.is_empty());
    assert!(
        !h.sim.soap_log()[before..]
            .iter()
            .any(|e| matches!(e.action.as_str(), "Pause" | "SetVolume"))
    );
}

struct HoldWrite<'a> {
    inner: &'a dyn Transport,
    action: &'static str,
    wait: Mutex<Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>>,
}

impl Transport for HoldWrite<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        header: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if header.contains(self.action) {
            let wait = self.wait.lock().unwrap().take();
            if let Some((arrived, resume)) = wait {
                arrived.send(()).unwrap();
                resume.recv_timeout(Duration::from_secs(5)).unwrap();
            }
        }
        self.inner.soap_post(host, path, header, body)
    }
}

#[test]
fn new_group_playback_waits_out_owned_writes_including_a_joined_rooms_fade() {
    for joined in [false, true] {
        let h = House::new(true);
        if !joined {
            playing(&h);
        }
        let surface = h.surface();
        let session = sleep(&surface);
        let households = h.rooms.lock().unwrap().clone();
        let coordinator = h.id(if joined { "RecoverB" } else { "RecoverA" });
        let timer = session
            .timers
            .start(
                &*surface.transport,
                &households,
                &session.fader,
                &coordinator,
                Duration::ZERO,
                Utc::now(),
            )
            .unwrap();
        // Arm a coordinator while it is standalone, then join its room to
        // the playing group. A follower cannot configure a native timer.
        if joined {
            playing(&h);
        }
        let households = h.rooms.lock().unwrap().clone();
        session.save();
        let ticket = session.timers.due_runs(timer.ends, 1).remove(0);
        let (arrived, entered) = mpsc::sync_channel(1);
        let (resume, waiting) = mpsc::channel();
        let worker = Arc::clone(&surface);
        let worker_houses = households.clone();
        let run = std::thread::spawn(move || {
            let sleep = sleep(&worker);
            let checkpoint = Checkpoint::new(sleep);
            let transport = HoldWrite {
                inner: &*worker.transport,
                action: if joined { "#SetVolume" } else { "#Pause" },
                wait: Mutex::new(Some((arrived, waiting))),
            };
            sleep.timers.run_due_with_checkpoint(
                &transport,
                &worker_houses,
                &sleep.fader,
                &ticket,
                &checkpoint,
            )
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let before = h.sim.soap_log().len();
        let (began, starting) = mpsc::sync_channel(1);
        let (done, finished) = mpsc::channel();
        let manual = Arc::clone(&surface);
        let group = h.id("RecoverA");
        let change = std::thread::spawn(move || {
            began.send(()).unwrap();
            sleep(&manual).playback_changing(&group);
            control::play_uri(
                &*manual.transport,
                &households,
                &group,
                "https://example.invalid/new-night-stream",
                "",
            )
            .unwrap();
            done.send(()).unwrap();
        });
        starting.recv_timeout(Duration::from_secs(5)).unwrap();
        let early = finished.recv_timeout(Duration::from_millis(100)).is_ok();
        resume.send(()).unwrap();
        assert!(!early, "new playback must wait out the old owned write");
        change.join().unwrap();
        match run.join().unwrap() {
            Ok(SleepOutcome::Paused | SleepOutcome::Interrupted) => {}
            Err(error) if joined => {
                // The worker may reacquire the room step before the waiting
                // interrupt, but the already-retired journal still fences it.
                assert!(
                    error
                        .to_string()
                        .contains("sleep fade's group or playback changed"),
                    "{error}"
                );
            }
            outcome => panic!("unexpected sleep outcome: {outcome:?}"),
        }
        assert_eq!(
            soap::get_transport_info(&h.sim.lan(), h.ip("RecoverA"))
                .unwrap()
                .state,
            TransportState::Playing
        );
        let log = h.sim.soap_log();
        let writes = &log[before..];
        if joined {
            assert!(!writes.iter().any(|e| matches!(
                e.action.as_str(),
                "Pause" | "BecomeCoordinatorOfStandaloneGroup"
            )));
        } else {
            let pause = writes.iter().position(|e| e.action == "Pause").unwrap();
            let playback = writes
                .iter()
                .position(|e| e.action == "SetAVTransportURI")
                .unwrap();
            assert!(pause < playback);
        }
    }
}
