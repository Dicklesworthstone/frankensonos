//! Queue selection crosses the same newer-intent boundary as other playback.

use super::*;
use crate::failure::ErrorCode;
use crate::surface::queue::QueuePlayRequest;
use fsonos_core::sleep::SleepRun;

fn selection(h: &House, surface: &Surface) -> QueuePlayRequest {
    control::queue_spotify_tracks(
        &h.sim.lan(),
        &h.rooms.lock().unwrap(),
        &h.id("RecoverA"),
        &[("spotify:track:0SimSavedSleep000002", "Morning")],
    )
    .unwrap()
    .unwrap();
    let queue = surface.queue(&Client::Cli, "RecoverA", 0, 100).unwrap();
    QueuePlayRequest {
        zone: queue.zone,
        position: 2,
        expected_uri: queue.items[1].uri.clone(),
        expected_revision: queue.revision,
    }
}

fn claim_due(h: &House, surface: &Surface) -> SleepRun {
    let session = sleep(surface);
    let timer = session
        .timers
        .start(
            &*surface.transport,
            &h.rooms.lock().unwrap(),
            &session.fader,
            &h.id("RecoverA"),
            Duration::ZERO,
            Utc::now(),
        )
        .unwrap();
    session.save();
    session.timers.due_runs(timer.ends, 1).remove(0)
}

#[test]
fn queue_play_retires_a_claimed_sleep_before_its_first_checkpoint() {
    for s2 in [false, true] {
        let h = House::new(s2);
        playing(&h);
        let surface = h.surface();
        let req = selection(&h, &surface);
        let ticket = claim_due(&h, &surface);
        assert!(surface.queue_play(&Client::Cli, &req).unwrap().changed);
        let before = h.sim.soap_log().len();
        let session = sleep(&surface);
        let checkpoint = Checkpoint::new(session);
        assert_eq!(
            session
                .timers
                .run_due_with_checkpoint(
                    &*surface.transport,
                    &h.rooms.lock().unwrap(),
                    &session.fader,
                    &ticket,
                    &checkpoint,
                )
                .unwrap(),
            SleepOutcome::Interrupted
        );
        assert_eq!(levels(&h), (30, 20));
        assert_eq!(
            soap::get_position_info(&h.sim.lan(), h.ip("RecoverA"))
                .unwrap()
                .track,
            2
        );
        assert_eq!(
            soap::get_transport_info(&h.sim.lan(), h.ip("RecoverA"))
                .unwrap()
                .state,
            TransportState::Playing
        );
        assert!(read(&h).timers.is_empty());
        assert!(read(&h).fades.is_empty());
        assert!(
            !h.sim.soap_log()[before..]
                .iter()
                .any(|e| matches!(e.action.as_str(), "Pause" | "SetVolume"))
        );
    }
}

#[test]
fn denied_or_stale_queue_play_preserves_the_claimed_sleep() {
    let h = House::new(true);
    playing(&h);
    let surface = h.surface();
    let mut req = selection(&h, &surface);
    let ticket = claim_due(&h, &surface);
    let original = sleep(&surface).timers.all();
    assert_eq!(
        surface.queue_play(&Client::Unknown, &req).unwrap_err().code,
        ErrorCode::PolicyDenied
    );
    req.expected_uri = Some("test:stale-queue-selection".into());
    assert_eq!(
        surface.queue_play(&Client::Cli, &req).unwrap_err().code,
        ErrorCode::QueueChanged
    );
    let session = sleep(&surface);
    assert_eq!(session.timers.all(), original);
    let checkpoint = Checkpoint::new(session);
    assert_eq!(
        session
            .timers
            .run_due_with_checkpoint(
                &*surface.transport,
                &h.rooms.lock().unwrap(),
                &session.fader,
                &ticket,
                &checkpoint,
            )
            .unwrap(),
        SleepOutcome::Paused
    );
    assert_eq!(
        soap::get_transport_info(&h.sim.lan(), h.ip("RecoverA"))
            .unwrap()
            .state,
        TransportState::Paused
    );
}

#[test]
fn queue_play_preserves_an_ordinary_future_sleep_timer() {
    let h = House::new(true);
    playing(&h);
    let surface = h.surface();
    let req = selection(&h, &surface);
    set(&surface, "RecoverA", "45m", false);
    let original = sleep(&surface).timers.all();
    assert!(surface.queue_play(&Client::Cli, &req).unwrap().changed);
    assert_eq!(sleep(&surface).timers.all(), original);
    assert_eq!(read(&h).timers, original);
}

#[derive(Clone, Copy, Debug)]
enum WhileWaiting {
    Unchanged,
    QueueEdited,
    GroupChanged,
}

#[test]
fn queue_play_waits_out_an_owned_pause_and_rechecks_the_selection() {
    for change in [
        WhileWaiting::Unchanged,
        WhileWaiting::QueueEdited,
        WhileWaiting::GroupChanged,
    ] {
        let h = House::new(true);
        playing(&h);
        let surface = h.surface();
        let req = selection(&h, &surface);
        let ticket = claim_due(&h, &surface);
        let (arrived, entered) = mpsc::sync_channel(1);
        let (resume, waiting) = mpsc::channel();
        let worker = Arc::clone(&surface);
        let households = h.rooms.lock().unwrap().clone();
        let run = std::thread::spawn(move || {
            let session = sleep(&worker);
            let checkpoint = Checkpoint::new(session);
            session.timers.run_due_with_checkpoint(
                &HoldWrite {
                    inner: &*worker.transport,
                    action: "#Pause",
                    wait: Mutex::new(Some((arrived, waiting))),
                },
                &households,
                &session.fader,
                &ticket,
                &checkpoint,
            )
        });
        entered.recv_timeout(Duration::from_secs(5)).unwrap();
        let before = h.sim.soap_log().len();
        let (done, finished) = mpsc::channel();
        let manual = Arc::clone(&surface);
        let play = std::thread::spawn(move || {
            let result = manual.queue_play(&Client::Cli, &req);
            done.send(()).unwrap();
            result
        });
        // The persistence marker is published before waiting out the held
        // Pause. Use it as a barrier so edits land after the initial checks,
        // rather than relying on a sleep to guess when preparation finished.
        let deadline = Instant::now() + Duration::from_secs(5);
        let committed = loop {
            if sleep(&surface)
                .writing
                .lock()
                .unwrap()
                .intent_epochs
                .contains_key(&h.id("RecoverA"))
            {
                break true;
            }
            if finished.try_recv().is_ok() || Instant::now() >= deadline {
                break false;
            }
            std::thread::sleep(Duration::from_millis(1));
        };
        if committed {
            match change {
                WhileWaiting::Unchanged => {}
                WhileWaiting::QueueEdited => {
                    soap::enqueue_uri(
                        &h.sim.lan(),
                        h.ip("RecoverA"),
                        "test:concurrent-queue-edit",
                        "",
                        0,
                        false,
                    )
                    .unwrap();
                }
                WhileWaiting::GroupChanged => {
                    soap::leave_group(&h.sim.lan(), h.ip("RecoverB"), &h.id("RecoverB")).unwrap();
                }
            }
        }
        // Always release and join the worker, including the unfixed failure
        // case, so a failed assertion cannot leave a blocked test thread.
        resume.send(()).unwrap();
        let result = play.join().unwrap();
        let outcome = run.join().unwrap().unwrap();
        assert!(
            committed,
            "queue play skipped the playback boundary: {change:?}"
        );
        assert!(matches!(
            outcome,
            SleepOutcome::Paused | SleepOutcome::Interrupted
        ));
        let log = h.sim.soap_log();
        let writes = &log[before..];
        match change {
            WhileWaiting::Unchanged => {
                assert!(result.unwrap().changed);
                let pause = writes.iter().position(|e| e.action == "Pause").unwrap();
                let start = writes
                    .iter()
                    .position(|e| e.action == "SetAVTransportURI")
                    .unwrap();
                assert!(
                    pause < start,
                    "the older Pause must finish before queue playback"
                );
                assert_eq!(
                    soap::get_transport_info(&h.sim.lan(), h.ip("RecoverA"))
                        .unwrap()
                        .state,
                    TransportState::Playing
                );
                assert_eq!(
                    soap::get_position_info(&h.sim.lan(), h.ip("RecoverA"))
                        .unwrap()
                        .track,
                    2
                );
            }
            WhileWaiting::QueueEdited | WhileWaiting::GroupChanged => {
                assert_eq!(
                    result.unwrap_err().code,
                    if matches!(change, WhileWaiting::QueueEdited) {
                        ErrorCode::QueueChanged
                    } else {
                        ErrorCode::NotCoordinator
                    }
                );
                // The injected regroup sends SetAVTransportURI to RecoverB;
                // the rejected selection must not start RecoverA's queue.
                let coordinator = h.id("RecoverA");
                assert!(!writes.iter().any(|e| {
                    e.player == coordinator.0
                        && matches!(e.action.as_str(), "SetAVTransportURI" | "Seek" | "Play")
                }));
            }
        }
    }
}
