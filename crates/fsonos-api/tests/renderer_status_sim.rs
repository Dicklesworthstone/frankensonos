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

#[test]
fn position_failures_preserve_transport_controls_and_recover_without_changing_playback() {
    let sim = fixture();
    let surface = surface(&sim);
    let lan = sim.lan();
    for room in ["First", "Second"] {
        let host = sim.player(room).unwrap().ip;
        soap::set_av_transport_uri(
            &lan,
            host,
            "x-rincon-mp3radio://stream.example.invalid/radio",
            "",
        )
        .unwrap();
        soap::set_volume(&lan, host, 23).unwrap();
        soap::set_mute(&lan, host, true).unwrap();
        soap::play(&lan, host).unwrap();
        sim.upnp_fault(room, "GetPositionInfo", 501).unwrap();
        let marker = sim.soap_log().len();

        let zones = surface.zones(&Client::Cli).unwrap();
        assert_eq!(
            zones
                .iter()
                .find(|zone| zone.coordinator_room == room)
                .unwrap()
                .transport_state,
            "playing"
        );
        assert!(
            sim.soap_log()[marker..]
                .iter()
                .all(|call| call.action != "GetPositionInfo")
        );
        let state = surface.zone_state(&Client::Cli, room).unwrap();
        assert_eq!(state.transport_state, "playing");
        assert_eq!(state.transport_status.as_deref(), Some("OK"));
        assert_eq!((state.volume, state.mute), (Some(23), Some(true)));
        assert_eq!(state.play_mode.as_deref(), Some("NORMAL"));
        assert!(state.track.is_none());
        assert!(
            state
                .track_error
                .as_ref()
                .is_some_and(|error| error.contains("501"))
        );
        let wire = serde_json::to_value(&state).unwrap();
        assert!(
            wire.get("track").is_none(),
            "unknown source is not an empty/zero observation"
        );
        assert!(wire["track_error"].is_string());

        // Ownership-sensitive mutations still require the complete observation.
        let houses = fsonos_core::inventory::survey(&lan, &[], Duration::from_millis(500))
            .unwrap()
            .households;
        let target = fsonos_core::resolve_room(&houses, room).unwrap();
        assert!(fsonos_core::control::playback(&lan, &houses, &target.coordinator.id).is_err());

        sim.clear_faults(room).unwrap();
        let recovered = surface.zone_state(&Client::Cli, room).unwrap();
        assert!(recovered.track_error.is_none());
        assert_eq!(recovered.transport_state, "playing");
        assert_eq!(
            recovered.track.unwrap().uri,
            "x-rincon-mp3radio://stream.example.invalid/radio"
        );
        assert!(
            sim.soap_log()[marker..]
                .iter()
                .all(|call| call.action.starts_with("Get"))
        );
    }
}

#[test]
fn a_transport_failure_remains_unknown_and_cannot_be_mistaken_for_a_partial_success() {
    let sim = fixture();
    let surface = surface(&sim);
    for room in ["First", "Second"] {
        sim.upnp_fault(room, "GetTransportInfo", 501).unwrap();
        let zone = surface.zone(&Client::Cli, room).unwrap();
        assert_eq!(zone.transport_state, "unknown");
        assert!(surface.zone_state(&Client::Cli, room).is_err());
        sim.clear_faults(room).unwrap();
        let empty = surface.zone_state(&Client::Cli, room).unwrap();
        assert!(empty.track.is_none());
        assert!(
            empty.track_error.is_none(),
            "an observed empty source is distinct from a failed read"
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
