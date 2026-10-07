//! The live model against the owner's real players. Inaudible: it surveys,
//! subscribes to events, waits for each player's initial NOTIFYs, then ends
//! every subscription. Ignored by default; on the speaker LAN run
//! `FSONOS_SEEDS=ip,ip cargo test -p fsonos-core --test live_lan_model --
//! --ignored --nocapture`.

use fsonos_core::live::{Live, LiveConfig};
use fsonos_proto::net::Lan;
use fsonos_types::PlayerId;
use std::net::IpAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

#[test]
#[ignore = "needs the owner's LAN: FSONOS_SEEDS=ip,ip"]
fn the_live_model_runs_on_the_real_households() {
    let seeds: Vec<IpAddr> = std::env::var("FSONOS_SEEDS")
        .expect("FSONOS_SEEDS=ip,ip")
        .split(',')
        .map(|s| s.trim().parse().expect("an IP address"))
        .collect();
    let lan = Arc::new(Lan::start().unwrap());
    let live = Live::start(lan, LiveConfig::new(seeds));
    assert!(
        live.wait_ready(Duration::from_secs(30)),
        "{:?}",
        live.snapshot().last_error
    );
    // Every player reports its volume (RenderingControl); every group
    // coordinator also reports transport state (AVTransport is subscribed on
    // coordinators only: a member plays what its coordinator plays).
    let households = live.households();
    let ids: Vec<PlayerId> = households
        .iter()
        .flat_map(|h| h.players.iter().map(|p| p.id.clone()))
        .collect();
    let coordinators: Vec<PlayerId> = households
        .iter()
        .flat_map(|h| h.groups.iter().map(|g| g.coordinator.clone()))
        .filter(|c| ids.contains(c))
        .collect();
    let counts = || {
        let volumes = ids
            .iter()
            .filter(|id| live.player(id).is_some_and(|p| p.volume.is_some()))
            .count();
        let transports = coordinators
            .iter()
            .filter(|id| live.player(id).is_some_and(|p| p.transport.is_some()))
            .count();
        (volumes, transports)
    };
    let deadline = Instant::now() + Duration::from_secs(20);
    while counts() != (ids.len(), coordinators.len()) && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
    }
    let snap = live.snapshot();
    let (volumes, transports) = counts();
    println!(
        "live model: {} household(s), {} player(s) in {} group(s), {} subscription(s); volume from {volumes}/{} players, transport from {transports}/{} coordinators",
        snap.households.len(),
        ids.len(),
        coordinators.len(),
        snap.subscriptions,
        ids.len(),
        coordinators.len()
    );
    assert_eq!(
        (volumes, transports),
        (ids.len(), coordinators.len()),
        "every subscription's initial NOTIFY arrives"
    );
    live.stop();
}
