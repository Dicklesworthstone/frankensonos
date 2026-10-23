//! A set's groups go at once (co55.9), through `Surface` against
//! `fsonos-sim` players over real sockets: `pause everywhere` while one
//! player answers every request a second late pauses every other group at
//! once, not after the slow one's reads; it still pauses the slow one too,
//! and one undo resumes all four.

use fsonos_api::plan::{TransportAction, plan_play, plan_transport_set};
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{PlayRequest, ZoneRequest};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_proto::control::get_transport_info;
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

const ROOMS: [&str; 4] = ["Kitchen", "Office", "Den", "Bedroom"];
/// The player that answers late.
const SLOW: &str = "Den";
const LATE: Duration = Duration::from_secs(1);
const STREAM: &str = "x-rincon-mp3radio://stream.example.invalid/house.mp3";

#[test]
fn pause_everywhere_pauses_the_other_groups_at_once_while_one_player_is_slow() {
    let sim = SimHousehold::builder()
        .s2(ROOMS.map(|room| SimPlayerSpec::new(room, SimModel::One)))
        .spawn()
        .unwrap();
    let houses: Vec<HouseholdState> =
        fsonos_core::inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
            .unwrap()
            .households;
    let survey: Survey = Box::new(move |_| Ok(houses.clone()));
    let s = Surface::new(
        Box::new(sim.lan()),
        survey,
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "cli");
    for room in ROOMS {
        let request = PlayRequest {
            zone: room.into(),
            source_uri: STREAM.into(),
            title: None,
        };
        s.control(&Client::Cli, "play", |h| plan_play(h, &request))
            .unwrap();
    }
    let lan = sim.lan();
    let ips: Vec<(&str, IpAddr)> = ROOMS
        .iter()
        .map(|room| (*room, sim.player(room).unwrap().ip))
        .collect();
    let state = |room: &str| {
        let ip = ips.iter().find(|(r, _)| *r == room).unwrap().1;
        get_transport_info(&lan, ip).map(|t| t.state).ok()
    };
    let all =
        |want: TransportState, rooms: &[&str]| rooms.iter().all(|room| state(room) == Some(want));
    assert!(all(TransportState::Playing, &ROOMS));
    sim.set_latency(SLOW, LATE).unwrap();

    // Watch the other three while the set runs.
    let others: Vec<&str> = ROOMS.into_iter().filter(|room| *room != SLOW).collect();
    let started = Instant::now();
    let finished = AtomicBool::new(false);
    let (paused_after, out) = std::thread::scope(|scope| {
        let watcher = scope.spawn(|| {
            while !finished.load(Ordering::SeqCst) {
                if all(TransportState::Paused, &others) {
                    return Some(started.elapsed());
                }
                std::thread::sleep(Duration::from_millis(20));
            }
            None
        });
        let everywhere = ZoneRequest {
            zone: "everywhere".into(),
        };
        let out = s.control_set(&Client::Cli, "pause", |h| {
            plan_transport_set(h, &everywhere, TransportAction::Pause)
        });
        finished.store(true, Ordering::SeqCst);
        (watcher.join().unwrap(), out)
    });
    let took = started.elapsed();
    let out = out.unwrap();
    assert!(took >= LATE, "the slow player was not slow: {took:?}");
    let paused_after = paused_after.unwrap_or(took);
    assert!(
        paused_after < Duration::from_millis(1500),
        "the other groups paused {paused_after:?} in, behind the slow one (the set took {took:?})"
    );
    assert!(all(TransportState::Paused, &ROOMS), "{out:?}");

    // One action: one undo resumes every group, the slow one too.
    let undone = s.undo(&Client::Cli, false).unwrap();
    assert!(all(TransportState::Playing, &ROOMS), "{undone:?}");
    sim.shutdown();
}
