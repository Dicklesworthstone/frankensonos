//! The real LAN transport under a house like the owner's: fourteen players in
//! two households, two of them not answering, and a survey listening. Each
//! player has a request lane of its own, so controlling the healthy players
//! never waits behind either.

use fsonos_proto::Transport;
use fsonos_proto::control::{get_position_info, set_volume};
use fsonos_proto::net::Lan;
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

/// The rooms that do not answer (each waits out the transport's timeout).
const SILENT: [&str; 2] = ["S1 Room 7", "S2 Room 7"];

fn house() -> SimHandle {
    let s1: Vec<SimPlayerSpec> = (1..=7)
        .map(|n| SimPlayerSpec::new(&format!("S1 Room {n}"), SimModel::Play5Gen1))
        .collect();
    let s2: Vec<SimPlayerSpec> = (1..=7)
        .map(|n| SimPlayerSpec::new(&format!("S2 Room {n}"), SimModel::One))
        .collect();
    SimHousehold::builder().s1(s1).s2(s2).spawn().unwrap()
}

#[test]
fn healthy_players_answer_promptly_beside_silent_ones_and_a_survey() {
    let sim = house();
    let lan = Arc::new(
        Lan::start()
            .unwrap()
            .with_routes(sim.players().iter().map(|p| (p.ip, p.addr)).collect())
            .with_ssdp_target(sim.ssdp_addr()),
    );
    let rooms: Vec<(String, IpAddr)> = sim
        .players()
        .iter()
        .map(|p| (p.room.clone(), p.ip))
        .collect();
    assert_eq!(rooms.len(), 14);
    for room in SILENT {
        sim.set_latency(room, Duration::from_secs(30)).unwrap();
    }

    // The load: each silent player asked again and again, and a survey
    // listening throughout, as the daemon's reads and surveys do.
    let stop = Arc::new(AtomicBool::new(false));
    let mut load = Vec::new();
    for (_, ip) in rooms
        .iter()
        .filter(|(room, _)| SILENT.contains(&room.as_str()))
    {
        let (lan, stop, ip) = (Arc::clone(&lan), Arc::clone(&stop), *ip);
        load.push(thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let _ = get_position_info(&*lan, ip);
            }
        }));
    }
    {
        let (lan, stop) = (Arc::clone(&lan), Arc::clone(&stop));
        load.push(thread::spawn(move || {
            while !stop.load(Ordering::Acquire) {
                let _ = lan.discover(1, Duration::from_secs(2));
            }
        }));
    }
    thread::sleep(Duration::from_millis(300));

    let mut took = Vec::new();
    for round in 0..5_u8 {
        for (room, ip) in rooms
            .iter()
            .filter(|(room, _)| !SILENT.contains(&room.as_str()))
        {
            let started = Instant::now();
            let set = set_volume(&*lan, *ip, 20 + round);
            took.push(started.elapsed());
            assert!(set.is_ok(), "{room}: {set:?}");
        }
    }
    stop.store(true, Ordering::Release);
    took.sort();
    let p99 = took[(took.len() * 99).div_ceil(100) - 1];
    assert!(
        p99 < Duration::from_millis(500),
        "p99 {p99:?} over {} SetVolume calls (slowest {:?})",
        took.len(),
        took.last()
    );
    for worker in load {
        worker.join().unwrap();
    }
    sim.shutdown();
}
