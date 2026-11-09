//! Renderer failures remain distinct from an ordinary stop through direct
//! reads, the live model, and the public event stream on S1 and S2 sockets.

use fsonos_api::events::EventDto;
use fsonos_api::surface::{Surface, Survey};
use fsonos_core::clock::SystemClock;
use fsonos_core::live::{Live, LiveConfig};
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::control as soap;
use fsonos_proto::net::Lan;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use std::sync::Arc;
use std::time::{Duration, Instant};

fn fixture() -> SimHandle {
    SimHousehold::builder()
        .s1([SimPlayerSpec::new("First", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Second", SimModel::One)])
        .spawn()
        .unwrap()
}

fn surface(sim: &SimHandle) -> Surface {
    let survey: Survey = Box::new(|t| {
        Ok(fsonos_core::inventory::survey(t, &[], Duration::from_millis(500))?.households)
    });
    Surface::new(
        Box::new(sim.lan()),
        survey,
        Policy::default(),
        Box::new(SystemClock),
    )
}

#[test]
fn direct_state_reports_a_failure_and_an_ordinary_stop_on_both_generations() {
    let sim = fixture();
    let surface = surface(&sim);
    let lan = sim.lan();
    for room in ["First", "Second"] {
        let host = sim.player(room).unwrap().ip;
        soap::set_av_transport_uri(&lan, host, "https://audio.example.invalid/track.mp3", "")
            .unwrap();
        soap::play(&lan, host).unwrap();
        assert_eq!(
            surface
                .zone_state(&Client::Cli, room)
                .unwrap()
                .transport_status
                .as_deref(),
            Some("OK")
        );
        sim.playback_error(room).unwrap();
        let marker = sim.soap_log().len();
        let failed = surface.zone_state(&Client::Cli, room).unwrap();
        assert_eq!(failed.transport_state, "stopped");
        assert_eq!(failed.transport_status.as_deref(), Some("ERROR_OCCURRED"));
        assert!(failed.playback_notice().is_some());
        assert!(
            sim.soap_log()[marker..]
                .iter()
                .all(|call| call.action.starts_with("Get"))
        );
        soap::stop(&lan, host).unwrap();
        let stopped = surface.zone_state(&Client::Cli, room).unwrap();
        assert_eq!(stopped.transport_state, "stopped");
        assert_eq!(stopped.transport_status.as_deref(), Some("OK"));
        assert_eq!(stopped.playback_notice(), None);
        // An intentional Stop resets the playback clock. It must retain
        // the selected source and occurrence while clearing the error.
        assert_eq!(
            failed
                .track
                .as_ref()
                .map(|track| (&track.uri, track.queue_position)),
            stopped
                .track
                .as_ref()
                .map(|track| (&track.uri, track.queue_position))
        );
    }
}

fn status_event(surface: &Surface, since: &mut u64, room: &str, expected: &str) -> EventDto {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let reply = surface
            .events_after(&Client::LoopbackHttp, Some(*since), Duration::from_secs(1))
            .unwrap();
        *since = reply.last_id;
        if let Some(event) = reply.events.into_iter().find(|event| {
            event.event == "zone.state"
                && event.data["room"] == room
                && event.data["transport_status"] == expected
        }) {
            return event;
        }
        assert!(
            Instant::now() < deadline,
            "no {expected} renderer event for {room}"
        );
    }
}

#[test]
fn live_status_only_recovery_reaches_state_and_events_on_both_generations() {
    let sim = fixture();
    for room in ["First", "Second"] {
        soap::set_av_transport_uri(
            &sim.lan(),
            sim.player(room).unwrap().ip,
            "https://audio.example.invalid/track.mp3",
            "",
        )
        .unwrap();
    }
    let lan = Arc::new(
        Lan::start()
            .unwrap()
            .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect())
            .with_ssdp_target(sim.ssdp_addr()),
    );
    let live = Arc::new(Live::start(Arc::clone(&lan), LiveConfig::new(Vec::new())));
    assert!(live.wait_ready(Duration::from_secs(10)));
    let surface = surface(&sim).with_live(&live);
    let mut since = surface
        .events_after(&Client::LoopbackHttp, None, Duration::ZERO)
        .unwrap()
        .last_id;
    for room in ["First", "Second"] {
        let host = sim.player(room).unwrap().ip;
        // The renderer remains stopped throughout: the only change here is
        // its status. Subscribers must still wake and reread the zone.
        sim.playback_error(room).unwrap();
        let _ = status_event(&surface, &mut since, room, "ERROR_OCCURRED");
        let failed = surface.zone_state(&Client::LoopbackHttp, room).unwrap();
        assert_eq!(failed.transport_state, "stopped");
        assert_eq!(failed.transport_status.as_deref(), Some("ERROR_OCCURRED"));
        soap::stop(&*lan, host).unwrap();
        let cleared = status_event(&surface, &mut since, room, "OK");
        assert!(cleared.data.get("transport").is_none(), "{cleared:?}");
        let state = surface.zone_state(&Client::LoopbackHttp, room).unwrap();
        assert_eq!(state.transport_state, "stopped");
        assert_eq!(state.transport_status.as_deref(), Some("OK"));
    }
}
