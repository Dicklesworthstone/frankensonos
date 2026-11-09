//! A renderer error can leave state, source and occurrence unchanged. Real
//! GENA must publish that status-only change to the daemon's subscribers.

use fsonos_core::live::{Live, LiveConfig, LiveEvent};
use fsonos_core::playback::Changes;
use fsonos_proto::control::{set_av_transport_uri, stop};
use fsonos_proto::net::Lan;
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::{PlayerId, TransportState};
use std::sync::Arc;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

fn status_change(events: &Receiver<LiveEvent>, player: &PlayerId, status: &str) -> Changes {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        match events.recv_timeout(left) {
            Ok(LiveEvent::Playback {
                player: changed,
                changes,
            }) if changed == *player
                && changes
                    .transport_status
                    .as_ref()
                    .is_some_and(|(_, next)| next.as_deref() == Some(status)) =>
            {
                return changes;
            }
            Ok(_) => assert!(Instant::now() < deadline, "status was not pushed: {status}"),
            Err(error) => panic!("status was not pushed: {status}: {error}"),
        }
    }
}

#[test]
fn status_only_gena_errors_and_recovery_wake_live_subscribers_on_s1_and_s2() {
    for generation in [1, 2] {
        let builder = SimHousehold::builder();
        let sim = match generation {
            1 => builder.s1([SimPlayerSpec::new("Room", SimModel::Play5Gen1)]),
            _ => builder.s2([SimPlayerSpec::new("Room", SimModel::One)]),
        }
        .spawn()
        .unwrap();
        let host = sim.player("Room").unwrap().ip;
        let lan = Arc::new(
            Lan::start()
                .unwrap()
                .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect())
                .with_ssdp_target(sim.ssdp_addr()),
        );
        let uri = "x-rincon-mp3radio://example.invalid/status-test.mp3";
        set_av_transport_uri(&*lan, host, uri, "").unwrap();
        let live = Live::start(Arc::clone(&lan), LiveConfig::new(vec![host]));
        assert!(live.wait_ready(Duration::from_secs(10)));
        let player = live.households()[0].groups[0].coordinator.clone();
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if live.player(&player).is_some_and(|state| {
                state.transport == Some(TransportState::Stopped)
                    && state.transport_status.as_deref() == Some("OK")
                    && state.media_uri.as_deref() == Some(uri)
            }) {
                break;
            }
            assert!(Instant::now() < deadline, "initial status was not observed");
            std::thread::sleep(Duration::from_millis(20));
        }
        let before = live.player(&player).unwrap();
        let events = live.subscribe();
        sim.playback_error("Room").unwrap();
        let changed = status_change(&events, &player, "ERROR_OCCURRED");
        assert_eq!(
            changed.transport_status,
            Some((Some("OK".into()), Some("ERROR_OCCURRED".into())))
        );
        assert_eq!(changed.transport, None);
        assert_eq!(changed.track, None);
        assert_eq!(changed.media_uri, None);
        let failed = live.player(&player).unwrap();
        assert_eq!(failed.transport_status.as_deref(), Some("ERROR_OCCURRED"));
        assert_eq!(failed.transport, before.transport);
        assert_eq!(failed.media_uri, before.media_uri);
        assert_eq!(failed.track_uri, before.track_uri);
        assert_eq!(failed.queue_position, before.queue_position);

        // A successful Stop clears the error without changing STOPPED.
        stop(&*lan, host).unwrap();
        let changed = status_change(&events, &player, "OK");
        assert_eq!(
            changed.transport_status,
            Some((Some("ERROR_OCCURRED".into()), Some("OK".into())))
        );
        assert_eq!(changed.transport, None);
        assert_eq!(changed.track, None);
        assert_eq!(changed.media_uri, None);
        assert_eq!(
            live.player(&player).unwrap().transport_status.as_deref(),
            Some("OK")
        );
        live.stop();
        sim.shutdown();
    }
}
