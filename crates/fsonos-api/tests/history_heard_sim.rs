//! What the house plays reaches the history even when fsonos didn't start
//! it: the daemon's live model records a coordinator's new track once,
//! with no DJ running. Against `fsonos-sim` over real loopback sockets.

use fsonos_api::surface::{Surface, Survey, follow};
use fsonos_core::clock::SystemClock;
use fsonos_core::control;
use fsonos_core::live::{Live, LiveConfig};
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_proto::net::Lan;
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use std::sync::Arc;
use std::time::{Duration, Instant};

const EVENING: &str = "x-rincon-mp3radio://stream.example.invalid/evening.mp3";
const LATE: &str = "x-rincon-mp3radio://stream.example.invalid/late.mp3";

#[test]
fn a_play_started_outside_fsonos_is_in_the_history() {
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
    let surface = Arc::new(
        Surface::new(
            Box::new(sim.lan()),
            survey,
            Policy::default(),
            Box::new(SystemClock),
        )
        .with_live(&live)
        .with_action_log(Box::new(MemStore::default()), "cli"),
    );
    follow(&surface);
    let kitchen = live
        .households()
        .iter()
        .flat_map(|h| &h.players)
        .find(|p| p.room_name == "Kitchen")
        .unwrap()
        .id
        .clone();
    let plays = || surface.recent_plays(&Client::Cli, None, 10).unwrap();
    let heard = |n: usize| {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline && plays().len() < n {
            std::thread::sleep(Duration::from_millis(100));
        }
        plays()
    };

    // Started on the speaker itself, as the Sonos app would.
    control::play_uri(&*lan, &live.households(), &kitchen, EVENING, "").unwrap();
    let after_one = heard(1);
    assert_eq!(after_one.len(), 1, "{after_one:?}");
    assert_eq!(after_one[0].source_uri, EVENING);
    assert_eq!(after_one[0].room.as_deref(), Some("Kitchen"));

    // A pause and resume is the same play.
    control::pause(&*lan, &live.households(), &kitchen).unwrap();
    control::resume(&*lan, &live.households(), &kitchen).unwrap();
    std::thread::sleep(Duration::from_secs(1));
    assert_eq!(plays().len(), 1, "{:?}", plays());

    control::play_uri(&*lan, &live.households(), &kitchen, LATE, "").unwrap();
    let after_two = heard(2);
    assert_eq!(after_two.len(), 2, "{after_two:?}");
    assert_eq!(after_two[0].source_uri, LATE, "newest first");
    sim.shutdown();
}
