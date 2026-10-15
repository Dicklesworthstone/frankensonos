use super::*;
use crate::playback::{EventSource, PlaybackObservation};
use fsonos_proto::control::{MediaInfo, PositionInfo, TransportInfo};
use fsonos_proto::gena::Notify;
use fsonos_types::TransportState;

fn targets() -> Vec<(PlayerId, IpAddr)> {
    vec![
        (PlayerId("clock-a".into()), "192.0.2.1".parse().unwrap()),
        (PlayerId("clock-b".into()), "192.0.2.2".parse().unwrap()),
    ]
}

fn request(playback: &mut Playback, id: &PlayerId, uri: &str, now: Instant) {
    let notify = Notify {
        sid: "uuid:clock".into(),
        seq: 1,
        path: "/AVTransport".into(),
        properties: vec![(
            "LastChange".into(),
            format!(
                "<Event><InstanceID val=\"0\"><TransportState val=\"PLAYING\"/>\
                 <CurrentTrackURI val=\"{uri}\"/></InstanceID></Event>"
            ),
        )],
    };
    playback
        .apply(id, EventSource::AvTransport, &notify, now)
        .unwrap();
}

fn observed(playback: &mut Playback, id: &PlayerId, now: Instant) {
    let observation = PlaybackObservation {
        transport: TransportInfo {
            state: TransportState::Playing,
            status: "OK".into(),
        },
        position: PositionInfo {
            track: 0,
            duration_secs: None,
            position_secs: Some(23),
            uri: "test:clock".into(),
            metadata: None,
        },
        media: MediaInfo {
            tracks: 0,
            uri: "test:clock".into(),
            uri_metadata: String::new(),
        },
    };
    playback.apply_observation(id, &observation, now);
}

#[test]
fn event_samples_without_an_end_deadline_and_then_stays_quiet() {
    let now = Instant::now();
    let targets = targets();
    let mut playback = Playback::default();
    let mut watchdog = Watchdog::default();
    request(&mut playback, &targets[0].0, "test:clock", now);
    assert_eq!(playback.of(&targets[0].0).unwrap().expected_end(), None);
    let probe = watchdog.next(&targets, &playback, now).unwrap();
    assert!(probe.position_requested);
    observed(&mut playback, &targets[0].0, now);
    watchdog.completed(probe, true, now);
    assert!(
        watchdog
            .next(&targets, &playback, now + Duration::from_secs(600))
            .is_none()
    );
}

#[test]
fn repeated_events_and_rapid_track_changes_keep_a_fixed_coalescing_deadline() {
    let now = Instant::now();
    let targets = targets();
    let mut playback = Playback::default();
    let mut watchdog = Watchdog::default();
    request(&mut playback, &targets[0].0, "test:clock", now);
    let probe = watchdog.next(&targets, &playback, now).unwrap();
    observed(&mut playback, &targets[0].0, now);
    watchdog.completed(probe, true, now);
    for tick in 1..20 {
        let at = now + Duration::from_millis(tick * 100);
        let uri = format!("test:clock-{tick}");
        request(&mut playback, &targets[0].0, &uri, at);
        assert!(watchdog.next(&targets, &playback, at).is_none());
    }
    let probe = watchdog
        .next(&targets, &playback, now + SAMPLE_INTERVAL)
        .unwrap();
    assert_eq!(probe.identity.uri, "test:clock-19");
    assert!(probe.position_requested);
}

#[test]
fn failures_retry_the_pending_sample_without_new_events_and_do_not_block_other_rooms() {
    let now = Instant::now();
    let targets = targets();
    let mut playback = Playback::default();
    let mut watchdog = Watchdog::default();
    request(&mut playback, &targets[0].0, "test:clock", now);
    request(&mut playback, &targets[1].0, "test:clock", now);
    let probe = watchdog.next(&targets, &playback, now).unwrap();
    assert_eq!(probe.player, targets[0].0);
    watchdog.completed(probe, false, now);
    let other = watchdog.next(&targets, &playback, now).unwrap();
    assert_eq!(other.player, targets[1].0);
    for second in 1..30 {
        let at = now + Duration::from_secs(second);
        request(&mut playback, &targets[0].0, "test:clock", at);
        assert!(watchdog.next(&targets[..1], &playback, at).is_none());
    }
    assert!(
        watchdog
            .next(&targets[..1], &playback, now + RETRY_MIN)
            .unwrap()
            .position_requested
    );
    assert!(watchdog.next(&[], &playback, now).is_none());
    assert!(watchdog.attempts.is_empty());
    assert!(watchdog.position_after.is_empty());
}

#[test]
fn live_worker_recovers_initial_and_same_track_seek_clocks_on_both_generations() {
    use crate::live::{Live, LiveConfig};
    use fsonos_proto::control::{pause, play, seek_position, set_av_transport_uri};
    use fsonos_proto::net::Lan;
    use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
    use std::sync::Arc;

    for generation in [1, 2] {
        let builder = SimHousehold::builder();
        let sim = match generation {
            1 => builder.s1([SimPlayerSpec::new("Clock", SimModel::Play5Gen1)]),
            _ => builder.s2([SimPlayerSpec::new("Clock", SimModel::One)]),
        }
        .spawn()
        .unwrap();
        let host = sim.player("Clock").unwrap().ip;
        let id = PlayerId(sim.player("Clock").unwrap().uuid.clone());
        let lan = Arc::new(
            Lan::start()
                .unwrap()
                .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect()),
        );
        let live = Live::start_with(Arc::new(sim.lan()), lan, LiveConfig::new(vec![host]));
        assert!(live.wait_ready(Duration::from_secs(10)));
        let events = live.subscribe();
        let wait = |expected: u32, transport| {
            let deadline = Instant::now() + Duration::from_secs(10);
            loop {
                if live.player(&id).is_some_and(|state| {
                    state.transport == Some(transport)
                        && state.position_at(Instant::now()).is_some_and(|position| {
                            if transport == TransportState::Playing {
                                (expected..=expected + 5).contains(&position)
                            } else {
                                position == expected
                            }
                        })
                }) {
                    break;
                }
                assert!(Instant::now() < deadline, "the live clock did not converge");
                std::thread::sleep(Duration::from_millis(10));
            }
        };
        set_av_transport_uri(&sim.lan(), host, "test:clock", "").unwrap();
        seek_position(&sim.lan(), host, 73).unwrap();
        play(&sim.lan(), host).unwrap();
        wait(73, TransportState::Playing);
        pause(&sim.lan(), host).unwrap();
        seek_position(&sim.lan(), host, 17).unwrap();
        wait(17, TransportState::Paused);
        while events.try_recv().is_ok() {}
        // Same URI, queue slot and PAUSED state: no ordinary Changes exist.
        seek_position(&sim.lan(), host, 41).unwrap();
        let sent = sim.soap_log().len();
        wait(41, TransportState::Paused);
        assert!(
            events.try_iter().any(|event| {
                matches!(event, LiveEvent::Playback { player, .. } if player == id)
            })
        );
        assert!(
            sim.soap_log()[sent..]
                .iter()
                .all(|call| call.action.starts_with("Get")),
            "clock repair must not start, skip, seek or rewrite music"
        );
        live.stop();
        sim.shutdown();
    }
}
