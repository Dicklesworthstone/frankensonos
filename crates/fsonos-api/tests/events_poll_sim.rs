//! The event stream as a long poll (`GET /events?wait=`, the `events` tool):
//! a cursor from now, a wait that ends as soon as the house changes, and a
//! reset once the ring has dropped what the client had not seen. Against a
//! live model over `fsonos-sim` players on real loopback sockets.

use fsonos_api::events::RING;
use fsonos_api::surface::{Surface, Survey};
use fsonos_core::clock::SystemClock;
use fsonos_core::live::{Live, LiveConfig};
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::control::set_volume;
use fsonos_proto::net::Lan;
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use serde_json::json;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
fn a_long_poll_waits_for_the_next_change_and_reports_a_gap() {
    let sim = SimHousehold::builder()
        .s1([SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1)])
        .spawn()
        .unwrap();
    let lan = Arc::new(
        Lan::start()
            .unwrap()
            .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect())
            .with_ssdp_target(sim.ssdp_addr()),
    );
    let live = Arc::new(Live::start(Arc::clone(&lan), LiveConfig::new(Vec::new())));
    assert!(live.wait_ready(Duration::from_secs(10)));
    let survey: Survey = Box::new(|_| Ok(Vec::new()));
    let surface = Surface::new(
        Box::new(sim.lan()),
        survey,
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_live(&live);
    let me = Client::LoopbackHttp;

    // Without `since`: nothing yet, and where to start from.
    let now = surface.events_after(&me, None, Duration::ZERO).unwrap();
    assert!(now.events.is_empty(), "{now:?}");
    assert_eq!(now.reset, None);

    // A change on the player while the poll waits ends the wait: polling on
    // from each answer's last_id, the change arrives well before the waits
    // would have run out.
    let kitchen = sim.player("Kitchen").unwrap().ip;
    let lan2 = sim.lan();
    let changed = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(300));
        set_volume(&lan2, kitchen, 37).unwrap();
    });
    let started = Instant::now();
    let (mut since, mut seen) = (now.last_id, Vec::new());
    while !seen.iter().any(|e: &fsonos_api::events::EventDto| {
        e.event == "zone.state" && e.data["volume"] == json!(37)
    }) && started.elapsed() < Duration::from_secs(15)
    {
        let next = surface
            .events_after(&me, Some(since), Duration::from_secs(20))
            .unwrap();
        assert_eq!(next.reset, None);
        assert_eq!(next.last_id, next.events.last().map_or(since, |e| e.id));
        since = next.last_id;
        seen.extend(next.events);
    }
    changed.join().unwrap();
    assert!(
        seen.iter()
            .any(|e| e.event == "zone.state" && e.data["volume"] == json!(37)),
        "{seen:?}"
    );
    assert!(started.elapsed() < Duration::from_secs(15), "{seen:?}");
    assert!(since > now.last_id);

    // More than the ring holds happened since: the poll says so, and still
    // answers with what is kept.
    let bus = surface.events(&me).unwrap();
    for n in 0..=RING {
        bus.publish("zone.state", json!({ "n": n }));
    }
    let behind = surface
        .events_after(&me, Some(since), Duration::ZERO)
        .unwrap();
    let oldest = behind.reset.expect("a gap");
    assert!(oldest > since + 1, "{oldest}");
    assert_eq!(behind.events.first().map(|e| e.id), Some(oldest));
    assert_eq!(behind.events.len(), RING);
}
