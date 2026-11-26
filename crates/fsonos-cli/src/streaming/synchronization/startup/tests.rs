//! Generated PCM and real local IPC; no speaker, microphone or Pulse server.

use std::cell::{Cell, RefCell};
use std::io::{BufRead as _, BufReader, Write as _};
use std::os::unix::net::UnixStream;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

use super::*;

struct Player {
    delay: f64,
    paused: bool,
    events: Vec<Value>,
    writes: Vec<f64>,
    lose_reply: bool,
}

fn with_player(test: impl FnOnce(&mut mpv::Controller, Arc<Mutex<Player>>)) {
    let (socket, mut peer) = UnixStream::pair().unwrap();
    let state = Arc::new(Mutex::new(Player {
        delay: 0.1,
        paused: false,
        events: Vec::new(),
        writes: Vec::new(),
        lose_reply: false,
    }));
    let remote = Arc::clone(&state);
    let worker = thread::spawn(move || {
        peer.set_read_timeout(Some(Duration::from_secs(30)))
            .unwrap();
        peer.set_write_timeout(Some(Duration::from_secs(3)))
            .unwrap();
        let mut reader = BufReader::new(peer.try_clone().unwrap());
        loop {
            let mut line = String::new();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {}
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionReset => break,
                Err(error) => panic!("startup IPC fixture: {error}"),
            }
            let request: Value = serde_json::from_str(&line).unwrap();
            let command = request["command"].as_array().unwrap();
            let mut state = remote.lock().unwrap();
            for event in std::mem::take(&mut state.events) {
                writeln!(peer, "{event}").unwrap();
            }
            let data = match command[0].as_str().unwrap() {
                "observe_property" => Value::Null,
                "get_property" => match command[1].as_str().unwrap() {
                    "path" => json!("startup-fixture.mp4"),
                    "audio-device" => json!("pulse/movie"),
                    "audio-delay" => json!(state.delay),
                    "pause" => json!(state.paused),
                    "speed" => json!(1.0),
                    other => panic!("unexpected read {other}"),
                },
                "set_property" => {
                    assert_eq!(command[1], "audio-delay");
                    let delay = command[2].as_f64().unwrap();
                    state.delay = delay;
                    state.writes.push(delay);
                    if state.lose_reply {
                        break;
                    }
                    Value::Null
                }
                other => panic!("unexpected startup command {other}"),
            };
            let response =
                json!({"request_id": request["request_id"], "error": "success", "data": data});
            drop(state);
            if writeln!(peer, "{response}").is_err() {
                break;
            }
        }
    });
    let client = mpv::Client::from_stream(socket).unwrap();
    let mut controller = mpv::Controller::new(client, 0.0).unwrap();
    test(&mut controller, state);
    drop(controller);
    worker.join().unwrap();
}

fn stop() -> Stop {
    Stop {
        session: Arc::new(AtomicBool::new(false)),
        stream: Arc::new(AtomicBool::new(false)),
        deadline: Some(Instant::now() + Duration::from_secs(30)),
    }
}

fn probe() -> probe::Config {
    probe::Config::named("movie", "movie.monitor").unwrap()
}

fn pcm() -> Vec<u8> {
    delayed_pcm(32_000, 3_200)
}

fn delayed_pcm(length: usize, lag: usize) -> Vec<u8> {
    let mut seed = 0xF50A_u32;
    let samples: Vec<i16> = (0..length)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            i16::try_from((seed >> 16) & 0x3fff).unwrap() - 8_192
        })
        .collect();
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for (index, sample) in samples.iter().enumerate() {
        bytes.extend(sample.to_le_bytes());
        bytes.extend(
            index
                .checked_sub(lag)
                .map_or(0, |at| samples[at])
                .to_le_bytes(),
        );
    }
    bytes
}

#[test]
fn startup_corrects_once_then_passive_tracking_uses_fresh_agreement_and_restores() {
    with_player(|controller, state| {
        let stop = stop();
        let captures = Cell::new(0);
        let phases = RefCell::new(Vec::new());
        let bytes = pcm();
        with_capture(
            controller,
            1_000,
            &stop,
            probe(),
            |settled| {
                phases.borrow_mut().push(settled);
                Ok(true)
            },
            |_, _, _| {
                assert!(state.lock().unwrap().writes.is_empty());
                captures.set(captures.get() + 1);
                Ok(Some(bytes.clone()))
            },
        )
        .unwrap();
        assert_eq!(captures.get(), 3);
        assert_eq!(
            *phases.borrow(),
            [false, true, false, true, false, true, true]
        );
        assert_eq!(state.lock().unwrap().writes, [-0.4]);
        // The real passive loop uses the same controller, but none of the
        // startup confidence or probe configuration. Hysteresis avoids a write.
        let passive = Cell::new(0);
        super::super::run_with_capture(
            controller,
            1_000,
            &stop,
            |settled| Ok(settled || passive.get() < 3),
            || {
                passive.set(passive.get() + 1);
                Ok(Some(bytes.clone()))
            },
        )
        .unwrap();
        assert_eq!(passive.get(), 3);
        assert_eq!(state.lock().unwrap().writes, [-0.4]);
        controller.restore().unwrap();
        assert_eq!(state.lock().unwrap().writes, [-0.4, 0.1]);
    });
}

#[test]
fn first_probe_can_correct_a_multi_second_lag_only_while_the_stream_is_still_owned() {
    for still_owned in [true, false] {
        with_player(|controller, state| {
            // A 14-second capture has time for three distinct reference
            // excerpts and the full five-second search after each one.
            let mut capture = Some(delayed_pcm(14 * 8_000, 33_000));
            let phases = RefCell::new(Vec::new());
            let result = with_capture(
                controller,
                5_000,
                &stop(),
                probe(),
                |settled| {
                    let mut phases = phases.borrow_mut();
                    phases.push(settled);
                    Ok(still_owned || phases.len() < 3)
                },
                |_, _, _| Ok(Some(capture.take().expect("expected only one capture"))),
            );
            assert_eq!(*phases.borrow(), [false, true, true]);
            assert_eq!(result.is_ok(), still_owned);
            if still_owned {
                assert_eq!(state.lock().unwrap().writes, [-4.125]);
                controller.restore().unwrap();
                assert_eq!(state.lock().unwrap().writes, [-4.125, 0.1]);
            } else {
                assert!(state.lock().unwrap().writes.is_empty());
            }
        });
    }
}

#[test]
fn first_probe_cannot_overwrite_a_seek_or_a_manual_delay_after_estimation() {
    for manual_delay in [false, true] {
        with_player(|controller, state| {
            let mut capture = Some(delayed_pcm(14 * 8_000, 33_000));
            let mut checks = 0;
            let result = with_capture(
                controller,
                5_000,
                &stop(),
                probe(),
                |_| {
                    checks += 1;
                    if checks == 3 {
                        let mut player = state.lock().unwrap();
                        if manual_delay {
                            player.delay = 0.7;
                        } else {
                            player.events.push(json!({"event": "seek"}));
                        }
                    }
                    Ok(true)
                },
                |_, _, _| Ok(Some(capture.take().expect("only one probe was authorized"))),
            );
            assert!(result.is_err());
            assert_eq!(checks, 3);
            let player = state.lock().unwrap();
            assert!(player.writes.is_empty());
            assert_eq!(
                player.delay.to_bits(),
                if manual_delay { 0.7_f64 } else { 0.1_f64 }.to_bits()
            );
        });
    }
}

#[test]
fn incomplete_first_probe_evidence_uses_the_original_three_capture_budget() {
    with_player(|controller, state| {
        let mut first = delayed_pcm(14 * 8_000, 33_000);
        // Remove the microphone's middle excerpt. The entire recording still
        // correlates strongly, but the isolated evidence cannot be confirmed.
        for frame in first[4 * (33_000 + 24_000)..4 * (33_000 + 48_000)]
            .as_chunks_mut::<4>()
            .0
        {
            frame[2..].fill(0);
        }
        let clean = delayed_pcm(14 * 8_000, 33_000);
        let mut captures = [first, clean.clone(), clean].into_iter();
        with_capture(
            controller,
            5_000,
            &stop(),
            probe(),
            |_| Ok(true),
            |_, _, _| Ok(captures.next()),
        )
        .unwrap();
        assert!(captures.next().is_none());
        assert_eq!(state.lock().unwrap().writes, [-4.125]);
    });
}

#[test]
fn inconclusive_startup_never_exceeds_three_audible_attempts() {
    with_player(|controller, state| {
        let captures = Cell::new(0);
        let error = with_capture(
            controller,
            1_000,
            &stop(),
            probe(),
            |_| Ok(true),
            |_, _, _| {
                captures.set(captures.get() + 1);
                Ok(Some(vec![0; 128_000]))
            },
        )
        .unwrap_err();
        assert!(error.to_string().contains("exhausted three windows"));
        assert_eq!(captures.get(), 3);
        assert!(state.lock().unwrap().writes.is_empty());
    });
}

#[test]
fn paused_startup_never_opens_capture_or_resumes_playback() {
    with_player(|controller, state| {
        state.lock().unwrap().paused = true;
        let result = with_capture(
            controller,
            1_000,
            &stop(),
            probe(),
            |_| Ok(true),
            |_, _, _| panic!("paused startup must not record or emit"),
        );
        assert!(result.is_err());
        assert!(state.lock().unwrap().paused);
        assert!(state.lock().unwrap().writes.is_empty());
    });
}

#[test]
fn interrupted_startup_is_not_rearmed_and_cannot_use_old_windows() {
    for event in [
        "seek",
        "playback-restart",
        "audio-reconfig",
        "end-file",
        "event-queue-overflow",
    ] {
        with_player(|controller, state| {
            let captures = Cell::new(0);
            let bytes = pcm();
            let result = with_capture(
                controller,
                1_000,
                &stop(),
                probe(),
                |_| Ok(true),
                |_, _, _| {
                    captures.set(captures.get() + 1);
                    if captures.get() == 2 {
                        state.lock().unwrap().events.push(json!({"event": event}));
                    }
                    Ok(Some(bytes.clone()))
                },
            );
            assert!(result.is_err(), "{event}");
            assert_eq!(captures.get(), 2);
            assert!(state.lock().unwrap().writes.is_empty());
        });
    }
}

#[test]
fn in_capture_guard_observes_a_pause_resume_cycle_even_when_playing_again() {
    with_player(|controller, state| {
        let result = with_capture(
            controller,
            1_000,
            &stop(),
            probe(),
            |_| Ok(true),
            |controller, window, _| {
                state.lock().unwrap().events.extend([
                    json!({"event": "property-change", "name": "pause", "data": true}),
                    json!({"event": "property-change", "name": "pause", "data": false}),
                ]);
                uninterrupted(controller, window)?;
                panic!("the active capture guard must reject the delivered interruption")
            },
        );
        assert!(result.is_err());
        assert!(state.lock().unwrap().writes.is_empty());
    });
}

#[test]
fn source_refusal_before_capture_after_capture_or_after_estimation_prevents_correction() {
    for refusal in [1, 2, 7] {
        with_player(|controller, state| {
            let reads = Cell::new(0);
            let captures = Cell::new(0);
            let bytes = pcm();
            let result = with_capture(
                controller,
                1_000,
                &stop(),
                probe(),
                |_| {
                    reads.set(reads.get() + 1);
                    anyhow::ensure!(reads.get() != refusal, "room no longer owns this stream");
                    Ok(true)
                },
                |_, _, _| {
                    captures.set(captures.get() + 1);
                    Ok(Some(bytes.clone()))
                },
            );
            assert!(result.is_err());
            assert_eq!(
                captures.get(),
                match refusal {
                    1 => 0,
                    2 => 1,
                    _ => 3,
                }
            );
            assert!(state.lock().unwrap().writes.is_empty());
        });
    }
}

#[test]
fn cancellation_and_expiry_before_capture_never_emit() {
    for expired in [false, true] {
        with_player(|controller, state| {
            let mut stop = stop();
            if expired {
                stop.deadline = Some(Instant::now());
            } else {
                stop.stream.store(true, Ordering::Release);
            }
            assert!(
                with_capture(
                    controller,
                    1_000,
                    &stop,
                    probe(),
                    |_| Ok(true),
                    |_, _, _| { panic!("cancelled calibration must not capture") }
                )
                .is_err()
            );
            assert!(state.lock().unwrap().writes.is_empty());
        });
    }
}

#[test]
fn final_verification_cancellation_and_manual_delay_override_are_not_overwritten() {
    for cancel in [false, true] {
        with_player(|controller, state| {
            let stop = stop();
            let reads = Cell::new(0);
            let bytes = pcm();
            let result = with_capture(
                controller,
                1_000,
                &stop,
                probe(),
                |_| {
                    reads.set(reads.get() + 1);
                    if reads.get() == 7 {
                        if cancel {
                            stop.stream.store(true, Ordering::Release);
                        } else {
                            state.lock().unwrap().delay = 0.7;
                        }
                    }
                    Ok(true)
                },
                |_, _, _| Ok(Some(bytes.clone())),
            );
            assert!(result.is_err());
            assert!(state.lock().unwrap().writes.is_empty());
            let expected = if cancel { 0.1 } else { 0.7 };
            assert!((state.lock().unwrap().delay - expected).abs() < 1e-9);
        });
    }
}

#[test]
fn an_already_correct_delay_needs_no_write_and_a_lost_reply_is_never_replayed() {
    for lost in [false, true] {
        with_player(|controller, state| {
            if lost {
                state.lock().unwrap().lose_reply = true;
            } else {
                let window = controller.begin_window().unwrap().unwrap();
                controller.apply(0.4, window).unwrap();
                state.lock().unwrap().writes.clear();
            }
            let bytes = pcm();
            let result = with_capture(
                controller,
                1_000,
                &stop(),
                probe(),
                |_| Ok(true),
                |_, _, _| Ok(Some(bytes.clone())),
            );
            assert_eq!(result.is_err(), lost);
            assert_eq!(state.lock().unwrap().writes.len(), usize::from(lost));
            if lost {
                assert!(controller.restore().is_err());
                assert_eq!(state.lock().unwrap().writes, [-0.4]);
            }
        });
    }
}

#[test]
fn capture_guard_refusal_prevents_even_launching_the_executable() {
    use super::super::input::Input;
    let options = Options {
        ffmpeg: "/must-not-be-launched".into(),
        input: Input::Paired {
            format: "lavfi".into(),
            source: "anullsrc".into(),
        },
        max_lag_ms: 1_000,
    };
    let error = capture::record_guarded(&options, &stop(), Some(&probe()), || {
        anyhow::bail!("playback changed before capture")
    })
    .unwrap_err();
    assert_eq!(error.to_string(), "playback changed before capture");
}

#[test]
#[ignore = "requires FFmpeg; finite generated silence only, no microphone or output device"]
fn active_guard_failure_interrupts_a_real_capture_before_its_window_finishes() {
    use super::super::input::Input;
    let options = Options {
        ffmpeg: std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".into()),
        input: Input::Paired {
            format: "lavfi".into(),
            source: "anullsrc=r=8000:cl=stereo,arealtime".into(),
        },
        max_lag_ms: 1_000,
    };
    let checks = Cell::new(0);
    let started = Instant::now();
    let error = capture::record_guarded(&options, &stop(), None, || {
        checks.set(checks.get() + 1);
        anyhow::ensure!(
            checks.get() < 2,
            "playback interruption during real capture"
        );
        Ok(())
    })
    .unwrap_err();
    assert!(error.to_string().contains("playback interruption"));
    assert_eq!(checks.get(), 2);
    assert!(started.elapsed() < Duration::from_secs(5));
}
