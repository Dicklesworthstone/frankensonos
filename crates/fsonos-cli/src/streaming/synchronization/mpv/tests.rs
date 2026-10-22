use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::{Client, Controller, LIMIT};

struct State {
    delay: f64,
    paused: bool,
    media: String,
    output: String,
    speed: f64,
    writes: Vec<f64>,
    lose_set_reply: bool,
    events: Vec<Value>,
    subscriptions: Vec<String>,
    event_after_final_read: Option<Value>,
    output_reads: usize,
}

impl Default for State {
    fn default() -> Self {
        Self {
            delay: 0.1,
            paused: false,
            media: "synthetic.mp4".into(),
            output: "auto".into(),
            speed: 1.0,
            writes: Vec::new(),
            lose_set_reply: false,
            events: Vec::new(),
            subscriptions: Vec::new(),
            event_after_final_read: None,
            output_reads: 0,
        }
    }
}

fn server() -> (Client, Arc<Mutex<State>>, thread::JoinHandle<()>) {
    let (client, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    peer.set_write_timeout(Some(Duration::from_secs(3)))
        .unwrap();
    let state = Arc::new(Mutex::new(State::default()));
    let remote = Arc::clone(&state);
    let worker = thread::spawn(move || {
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                Err(error) => panic!("read test IPC: {error}"),
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            let command = request["command"].as_array().unwrap();
            let mut state = remote.lock().unwrap();
            for event in std::mem::take(&mut state.events) {
                writeln!(peer, "{event}").unwrap();
            }
            let data = match command[0].as_str().unwrap() {
                "observe_property" => {
                    state
                        .subscriptions
                        .push(command[2].as_str().unwrap().to_owned());
                    Value::Null
                }
                "get_property" => match command[1].as_str().unwrap() {
                    "path" => json!(state.media),
                    "audio-device" => json!(state.output),
                    "audio-delay" => json!(state.delay),
                    "pause" => json!(state.paused),
                    "speed" => json!(state.speed),
                    other => panic!("unexpected read {other}"),
                },
                "set_property" => {
                    assert_eq!(command[1], "audio-delay");
                    state.delay = command[2].as_f64().unwrap();
                    let delay = state.delay;
                    state.writes.push(delay);
                    if state.lose_set_reply {
                        break;
                    }
                    Value::Null
                }
                other => panic!("unexpected command {other}"),
            };
            // Each snapshot reads path and audio-device twice. Inject after the
            // last reply, in the same packet, to exercise the trailing-event drain.
            let trailing = if command[0] == "get_property" && command[1] == "audio-device" {
                state.output_reads += 1;
                if state.output_reads.is_multiple_of(2) {
                    state.event_after_final_read.take()
                } else {
                    None
                }
            } else {
                None
            };
            drop(state);
            let response = json!({
                "request_id": request["request_id"], "error": "success", "data": data,
            });
            let packet = trailing.map_or_else(
                || format!("{response}\n"),
                |event| format!("{response}\n{event}\n"),
            );
            if peer.write_all(packet.as_bytes()).is_err() {
                break;
            }
        }
    });
    (Client::from_stream(client).unwrap(), state, worker)
}

fn apply(controller: &mut Controller, lag: f64) -> anyhow::Result<bool> {
    let Some(window) = controller.begin_window()? else {
        return Ok(false);
    };
    controller.apply(lag, window)
}

#[test]
fn applies_absolute_negative_targets_with_hysteresis_and_restores() {
    let (client, state, worker) = server();
    let mut controller = Controller::new(client, 0.0).unwrap();
    assert!(apply(&mut controller, 3.0).unwrap());
    assert!(!apply(&mut controller, 3.02).unwrap());
    assert!(apply(&mut controller, 3.2).unwrap());
    controller.restore().unwrap();
    controller.restore().unwrap();
    let writes = state.lock().unwrap().writes.clone();
    assert_eq!(writes.len(), 3);
    for (actual, expected) in writes.iter().zip([-3.0, -3.2, 0.1]) {
        assert!((actual - expected).abs() < 1e-9);
    }
    drop(controller);
    worker.join().unwrap();
}

#[test]
fn explicit_baseline_does_not_stack_a_previous_sessions_correction() {
    let (client, state, worker) = server();
    state.lock().unwrap().delay = -3.0;
    let mut controller = Controller::new(client, 0.2).unwrap();
    apply(&mut controller, 3.0).unwrap();
    assert!((state.lock().unwrap().delay + 2.8).abs() < 1e-9);
    controller.restore().unwrap();
    assert!((state.lock().unwrap().delay + 3.0).abs() < 1e-9);
    drop(controller);
    worker.join().unwrap();
}

#[test]
fn pause_holds_delay_and_manual_override_prevents_further_writes() {
    let (client, state, worker) = server();
    let mut controller = Controller::new(client, 0.0).unwrap();
    state.lock().unwrap().paused = true;
    assert!(!apply(&mut controller, 3.0).unwrap());
    assert!(state.lock().unwrap().writes.is_empty());
    state.lock().unwrap().paused = false;
    apply(&mut controller, 3.0).unwrap();
    state.lock().unwrap().delay = 0.7;
    assert!(apply(&mut controller, 3.5).is_err());
    assert!(controller.restore().is_err());
    assert_eq!(state.lock().unwrap().writes.len(), 1);
    drop(controller);
    worker.join().unwrap();
}

#[test]
fn a_new_media_file_or_output_is_not_corrected_or_restored() {
    for change_output in [false, true] {
        let (client, state, worker) = server();
        let mut controller = Controller::new(client, 0.0).unwrap();
        apply(&mut controller, 3.0).unwrap();
        if change_output {
            state.lock().unwrap().output = "other".into();
        } else {
            state.lock().unwrap().media = "new.mp4".into();
        }
        assert!(apply(&mut controller, 3.1).is_err());
        assert!(controller.restore().is_err());
        assert_eq!(state.lock().unwrap().writes.len(), 1);
        drop(controller);
        worker.join().unwrap();
    }
}

#[test]
fn lost_write_acknowledgment_is_never_replayed_or_reconnected() {
    let (client, state, worker) = server();
    let mut controller = Controller::new(client, 0.0).unwrap();
    state.lock().unwrap().lose_set_reply = true;
    assert!(apply(&mut controller, 3.0).is_err());
    assert!(controller.restore().is_err());
    assert_eq!(state.lock().unwrap().writes.len(), 1);
    drop(controller);
    worker.join().unwrap();
}

#[test]
fn invalid_speed_and_baselines_fail_before_a_write() {
    let (client, state, worker) = server();
    state.lock().unwrap().speed = 1.1;
    assert!(Controller::new(client, 0.0).is_err());
    assert!(state.lock().unwrap().writes.is_empty());
    worker.join().unwrap();
    let (client, state, worker) = server();
    assert!(Controller::new(client, f64::NAN).is_err());
    assert!(state.lock().unwrap().writes.is_empty());
    worker.join().unwrap();
}

fn raw_response(reply: Vec<u8>) -> (Client, thread::JoinHandle<()>) {
    let (client, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(2))).unwrap();
    peer.set_write_timeout(Some(Duration::from_secs(2)))
        .unwrap();
    let worker = thread::spawn(move || {
        let mut line = String::new();
        BufReader::new(peer.try_clone().unwrap())
            .read_line(&mut line)
            .unwrap();
        let _ = peer.write_all(&reply);
    });
    (Client::from_stream(client).unwrap(), worker)
}

#[test]
fn correlates_replies_while_ignoring_events_and_unrelated_request_ids() {
    let (mut client, worker) = raw_response(
        concat!(
            "{\"event\":\"tick\"}\n",
            "{\"request_id\":0,\"error\":\"success\",\"data\":99}\n",
            "{\"request_id\":1,\"error\":\"success\",\"data\":0.5}\n",
        )
        .as_bytes()
        .to_vec(),
    );
    assert_eq!(client.get("audio-delay").unwrap(), json!(0.5));
    worker.join().unwrap();
}

#[test]
fn rejects_malformed_denied_oversized_and_incomplete_replies() {
    for reply in [
        b"not json\n".to_vec(),
        b"{\"request_id\":1,\"error\":\"failure\"}\n".to_vec(),
        b"{\"request_id\":1".to_vec(),
        vec![b' '; LIMIT + 1],
    ] {
        let (mut client, worker) = raw_response(reply);
        assert!(client.get("audio-delay").is_err());
        drop(client);
        worker.join().unwrap();
    }
}

#[test]
fn silent_peer_has_a_total_request_deadline() {
    let (socket, _peer) = UnixStream::pair().unwrap();
    let mut client = Client::from_stream(socket).unwrap();
    client.timeout = Duration::from_millis(30);
    let started = Instant::now();
    assert!(client.get("audio-delay").is_err());
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn registers_read_only_observers_before_accepting_measurements() {
    let (client, state, worker) = server();
    let mut controller = Controller::new(client, 0.0).unwrap();
    assert!(controller.begin_window().unwrap().is_some());
    let state = state.lock().unwrap();
    assert_eq!(state.subscriptions, super::events::PROPERTIES);
    assert!(state.writes.is_empty());
    drop(state);
    drop(controller);
    worker.join().unwrap();
}

#[test]
fn delivered_pause_resume_during_capture_discards_the_window_but_can_resume() {
    let (client, state, worker) = server();
    let mut controller = Controller::new(client, 0.0).unwrap();
    let window = controller.begin_window().unwrap().unwrap();
    state.lock().unwrap().events.extend([
        json!({"event": "property-change", "name": "pause", "data": true}),
        json!({"event": "property-change", "name": "pause", "data": false}),
    ]);
    assert!(!controller.finish_window(window).unwrap());
    assert!(!controller.apply(3.0, window).unwrap());
    assert!(state.lock().unwrap().writes.is_empty());
    let fresh = controller.begin_window().unwrap().unwrap();
    assert_ne!(fresh, window);
    assert!(controller.finish_window(fresh).unwrap());
    assert!(controller.apply(3.0, fresh).unwrap());
    controller.restore().unwrap();
    assert_eq!(state.lock().unwrap().writes.len(), 2);
    drop(controller);
    worker.join().unwrap();
}

#[test]
fn seek_or_audio_restart_after_estimation_blocks_the_pending_write() {
    for event in ["seek", "playback-restart", "audio-reconfig"] {
        let (client, state, worker) = server();
        let mut controller = Controller::new(client, 0.0).unwrap();
        let window = controller.begin_window().unwrap().unwrap();
        assert!(controller.finish_window(window).unwrap());
        state.lock().unwrap().events.push(json!({"event": event}));
        assert!(!controller.apply(3.0, window).unwrap());
        assert!(state.lock().unwrap().writes.is_empty());
        let fresh = controller.begin_window().unwrap().unwrap();
        assert_ne!(window, fresh);
        assert!(controller.apply(3.0, fresh).unwrap());
        drop(controller);
        worker.join().unwrap();
    }
}

#[test]
fn same_path_reload_and_temporary_output_changes_relinquish_cleanup() {
    for events in [
        vec![json!({"event": "end-file"}), json!({"event": "start-file"})],
        vec![
            json!({"event": "property-change", "name": "audio-device", "data": "other"}),
            json!({"event": "property-change", "name": "audio-device", "data": "auto"}),
        ],
    ] {
        let (client, state, worker) = server();
        let mut controller = Controller::new(client, 0.0).unwrap();
        assert!(apply(&mut controller, 3.0).unwrap());
        let window = controller.begin_window().unwrap().unwrap();
        state.lock().unwrap().events = events;
        assert!(controller.finish_window(window).is_err());
        assert!(controller.restore().is_err());
        assert_eq!(state.lock().unwrap().writes.len(), 1);
        drop(controller);
        worker.join().unwrap();
    }
}

#[test]
fn an_event_after_a_snapshot_reply_is_not_left_for_the_next_write() {
    let (client, state, worker) = server();
    let mut controller = Controller::new(client, 0.0).unwrap();
    let window = controller.begin_window().unwrap().unwrap();
    state.lock().unwrap().event_after_final_read = Some(json!({"event": "seek"}));
    assert!(!controller.apply(3.0, window).unwrap());
    assert!(state.lock().unwrap().writes.is_empty());
    drop(controller);
    worker.join().unwrap();
}

#[test]
fn event_loss_disables_correction_instead_of_reusing_partial_evidence() {
    let (client, state, worker) = server();
    let mut controller = Controller::new(client, 0.0).unwrap();
    let window = controller.begin_window().unwrap().unwrap();
    state
        .lock()
        .unwrap()
        .events
        .push(json!({"event": "event-queue-overflow"}));
    assert!(controller.apply(3.0, window).is_err());
    assert!(controller.begin_window().is_err());
    assert!(state.lock().unwrap().writes.is_empty());
    drop(controller);
    worker.join().unwrap();
}

#[test]
fn fragmented_events_are_completed_before_authorizing_a_window() {
    let (socket, mut peer) = UnixStream::pair().unwrap();
    let mut client = Client::from_stream(socket).unwrap();
    peer.write_all(b"{\"event\":\"se").unwrap();
    let (release, released) = std::sync::mpsc::channel();
    let worker = thread::spawn(move || {
        thread::sleep(Duration::from_millis(20));
        peer.write_all(b"ek\"}\n").unwrap();
        // Hold the peer open across the drain; EOF is a failure, not success.
        released.recv_timeout(Duration::from_secs(2)).unwrap();
    });
    client.drain_events().unwrap();
    assert_eq!(client.events.revision(), 1);
    release.send(()).unwrap();
    worker.join().unwrap();
}

fn stop_sync() -> super::super::Stop {
    super::super::Stop {
        session: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        stream: Arc::new(std::sync::atomic::AtomicBool::new(false)),
        deadline: Some(Instant::now() + Duration::from_secs(20)),
    }
}

fn pcm_window() -> Vec<u8> {
    let mut seed = 0xF50A_u32;
    let samples: Vec<i16> = (0..32_000)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            i16::try_from((seed >> 16) & 0x3fff).unwrap() - 8_192
        })
        .collect();
    let mut pcm = Vec::with_capacity(samples.len() * 4);
    for (index, sample) in samples.iter().enumerate() {
        let delayed = index.checked_sub(3_200).map_or(0, |at| samples[at]);
        pcm.extend(sample.to_le_bytes());
        pcm.extend(delayed.to_le_bytes());
    }
    pcm
}

#[test]
fn continuous_loop_rechecks_the_room_before_correction_and_restores_after_success() {
    let (client, state, worker) = server();
    let mut controller = Controller::new(client, 0.0).unwrap();
    let stop = stop_sync();
    let captures = std::cell::Cell::new(0);
    let phases = std::cell::RefCell::new(Vec::new());
    let pcm = pcm_window();
    super::super::run_with_capture(
        &mut controller,
        1_000,
        &stop,
        |settled| {
            phases.borrow_mut().push(settled);
            Ok(settled || captures.get() < 3)
        },
        || {
            captures.set(captures.get() + 1);
            Ok(Some(pcm.clone()))
        },
    )
    .unwrap();
    assert_eq!(captures.get(), 3);
    assert_eq!(
        *phases.borrow(),
        [false, true, false, true, false, true, true, false]
    );
    assert_eq!(state.lock().unwrap().writes.len(), 1);
    assert!((state.lock().unwrap().delay + 0.4).abs() < 1e-9);
    controller.restore().unwrap();
    assert_eq!(state.lock().unwrap().writes.len(), 2);
    assert!((state.lock().unwrap().delay - 0.1).abs() < 1e-9);
    drop(controller);
    worker.join().unwrap();
}

#[test]
fn a_changed_room_after_capture_stops_the_loop_without_correcting_or_recapturing() {
    let (client, state, worker) = server();
    let mut controller = Controller::new(client, 0.0).unwrap();
    let captures = std::cell::Cell::new(0);
    let error = super::super::run_with_capture(
        &mut controller,
        1_000,
        &stop_sync(),
        |settled| {
            anyhow::ensure!(!settled, "original room group changed");
            Ok(true)
        },
        || {
            captures.set(captures.get() + 1);
            Ok(Some(pcm_window()))
        },
    )
    .unwrap_err();
    assert!(error.to_string().contains("room group changed"));
    assert_eq!(captures.get(), 1);
    assert!(state.lock().unwrap().writes.is_empty());
    drop(controller);
    worker.join().unwrap();
}

#[test]
fn seek_discarded_capture_requires_three_new_complete_windows() {
    let (client, state, worker) = server();
    let mut controller = Controller::new(client, 0.0).unwrap();
    let captures = std::cell::Cell::new(0);
    let pcm = pcm_window();
    super::super::run_with_capture(
        &mut controller,
        1_000,
        &stop_sync(),
        |settled| Ok(settled || captures.get() < 6),
        || {
            captures.set(captures.get() + 1);
            assert!(state.lock().unwrap().writes.is_empty());
            if captures.get() == 3 {
                state.lock().unwrap().events.push(json!({"event": "seek"}));
            }
            Ok(Some(pcm.clone()))
        },
    )
    .unwrap();
    assert_eq!(captures.get(), 6);
    assert_eq!(state.lock().unwrap().writes.len(), 1);
    assert!((state.lock().unwrap().delay + 0.4).abs() < 1e-9);
    drop(controller);
    worker.join().unwrap();
}

#[test]
fn cancellation_during_final_room_verification_prevents_the_delay_write() {
    let (client, state, worker) = server();
    let mut controller = Controller::new(client, 0.0).unwrap();
    let stop = stop_sync();
    let settled_reads = std::cell::Cell::new(0);
    let pcm = pcm_window();
    super::super::run_with_capture(
        &mut controller,
        1_000,
        &stop,
        |settled| {
            if settled {
                settled_reads.set(settled_reads.get() + 1);
                if settled_reads.get() == 4 {
                    stop.stream
                        .store(true, std::sync::atomic::Ordering::Release);
                }
            }
            Ok(true)
        },
        || Ok(Some(pcm.clone())),
    )
    .unwrap();
    assert_eq!(settled_reads.get(), 4);
    assert!(state.lock().unwrap().writes.is_empty());
    drop(controller);
    worker.join().unwrap();
}
