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
        }
    }
}

fn server() -> (Client, Arc<Mutex<State>>, thread::JoinHandle<()>) {
    let (client, mut peer) = UnixStream::pair().unwrap();
    peer.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
    peer.set_write_timeout(Some(Duration::from_secs(3))).unwrap();
    let state = Arc::new(Mutex::new(State::default()));
    let remote = Arc::clone(&state);
    let worker = thread::spawn(move || {
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        loop {
            let mut line = String::new();
            if reader.read_line(&mut line).unwrap() == 0 {
                break;
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            let command = request["command"].as_array().unwrap();
            let mut state = remote.lock().unwrap();
            let data = match command[0].as_str().unwrap() {
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
            drop(state);
            let response = json!({
                "request_id": request["request_id"], "error": "success", "data": data,
            });
            writeln!(peer, "{response}").unwrap();
        }
    });
    (Client::from_stream(client).unwrap(), state, worker)
}

#[test]
fn applies_absolute_negative_targets_with_hysteresis_and_restores() {
    let (client, state, worker) = server();
    let mut controller = Controller::new(client, 0.0).unwrap();
    assert!(controller.apply(3.0).unwrap());
    assert!(!controller.apply(3.02).unwrap());
    assert!(controller.apply(3.2).unwrap());
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
    controller.apply(3.0).unwrap();
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
    assert!(!controller.apply(3.0).unwrap());
    assert!(state.lock().unwrap().writes.is_empty());
    state.lock().unwrap().paused = false;
    controller.apply(3.0).unwrap();
    state.lock().unwrap().delay = 0.7;
    assert!(controller.apply(3.5).is_err());
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
        controller.apply(3.0).unwrap();
        if change_output {
            state.lock().unwrap().output = "other".into();
        } else {
            state.lock().unwrap().media = "new.mp4".into();
        }
        assert!(controller.apply(3.1).is_err());
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
    assert!(controller.apply(3.0).is_err());
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
    peer.set_write_timeout(Some(Duration::from_secs(2))).unwrap();
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
