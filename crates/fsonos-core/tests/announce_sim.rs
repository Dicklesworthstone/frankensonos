//! Announcements against `fsonos-sim` over real loopback sockets, in both
//! generations: the clip plays in the target rooms only, and every zone is
//! put back as it was.

use fsonos_core::announce::{Announcer, Clip, Ending};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec, SoapLogEntry};
use fsonos_types::{PlayerId, TransportState};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Kitchen", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
            SimPlayerSpec::new("Den", SimModel::Play5Gen1),
        ])
        .s2([
            SimPlayerSpec::new("Living Room", SimModel::One),
            SimPlayerSpec::new("Bedroom", SimModel::Play1),
        ])
        .spawn()
        .unwrap()
}

fn houses(sim: &SimHandle, lan: &SimLan) -> Vec<HouseholdState> {
    ["Kitchen", "Living Room"]
        .iter()
        .map(|room| {
            let mut st = HouseholdState::default();
            st.apply_topology(&get_zone_group_state(lan, sim.player(room).unwrap().ip).unwrap());
            st
        })
        .collect()
}

fn id(h: &[HouseholdState], room: &str) -> PlayerId {
    resolve_room(h, room).unwrap().player.id.clone()
}

fn clip(name: &str, secs: u64) -> Clip {
    Clip {
        url: format!("http://192.0.2.250:3400/media/{name}.wav"),
        title: name.into(),
        duration: Duration::from_secs(secs),
    }
}

fn no_cap(_: &str) -> u8 {
    100
}

fn fast() -> Announcer {
    Announcer::with_timing(Duration::from_millis(5), Duration::from_secs(5))
}

/// Run `f` while the simulator's clock runs about 10 times faster than
/// real time, so clips reach their end.
fn with_clock_running<R: Send>(sim: &SimHandle, f: impl FnOnce() -> R + Send) -> R {
    let clock = sim.clock();
    let done = AtomicBool::new(false);
    std::thread::scope(|s| {
        let job = s.spawn(|| {
            let r = f();
            done.store(true, Ordering::SeqCst);
            r
        });
        while !done.load(Ordering::SeqCst) {
            clock.advance(Duration::from_millis(10));
            std::thread::sleep(Duration::from_millis(1));
        }
        job.join().unwrap()
    })
}

/// `room` plays a two-track Spotify queue, at track 2, 40 s in.
fn play_queue(sim: &SimHandle, lan: &SimLan, room: &str) {
    let h = houses(sim, lan);
    let coord = resolve_room(&h, room).unwrap().coordinator.id.clone();
    control::queue_spotify_tracks(
        lan,
        &h,
        &coord,
        &[
            ("spotify:track:0SimAnnounceA000000001", "First"),
            ("spotify:track:0SimAnnounceB000000002", "Second"),
        ],
    )
    .unwrap()
    .unwrap();
    control::play_queue_from(lan, &h, &coord, 2).unwrap();
    sim.clock().advance(Duration::from_secs(40));
}

/// What `room`'s group is playing: (state, track, position, track URI).
fn playing(lan: &SimLan, h: &[HouseholdState], room: &str) -> (TransportState, u32, u32, String) {
    let coord = resolve_room(h, room).unwrap().coordinator.id.clone();
    let now = control::playback(lan, h, &coord).unwrap();
    (
        now.transport.state,
        now.position.track,
        now.position.position_secs.unwrap_or(0),
        now.position.uri,
    )
}

/// Each room's (coordinator, volume, mute).
fn levels(lan: &SimLan, h: &[HouseholdState], rooms: &[&str]) -> Vec<(PlayerId, u8, bool)> {
    rooms
        .iter()
        .map(|room| {
            let target = resolve_room(h, room).unwrap();
            let ip = target.player.ip;
            (
                target.coordinator.id.clone(),
                soap::get_volume(lan, ip).unwrap(),
                soap::get_mute(lan, ip).unwrap(),
            )
        })
        .collect()
}

/// The values of argument `arg` that `room` was sent with `action`.
fn sent(log: &[SoapLogEntry], room: &str, action: &str, arg: &str) -> Vec<String> {
    log.iter()
        .filter(|e| e.room == room && e.action == action)
        .filter_map(|e| {
            e.args
                .iter()
                .find(|(k, _)| k == arg)
                .map(|(_, v)| v.clone())
        })
        .collect()
}

#[test]
fn an_announcement_plays_in_its_rooms_and_everything_is_put_back() {
    let sim = sim();
    let lan = sim.lan();
    play_queue(&sim, &lan, "Kitchen");
    let h = houses(&sim, &lan);
    control::join(&lan, &h, &id(&h, "Office"), &id(&h, "Kitchen")).unwrap();
    for (room, level) in [("Kitchen", 20), ("Office", 50), ("Den", 10)] {
        control::set_volume(&lan, &h, &id(&h, room), level).unwrap();
    }
    control::set_mute(&lan, &h, &id(&h, "Den"), true).unwrap();
    let h = houses(&sim, &lan);
    let rooms = ["Kitchen", "Office", "Den"];
    let before = levels(&lan, &h, &rooms);
    let (_, _, _, kitchen_uri) = playing(&lan, &h, "Kitchen");
    let log_from = sim.soap_log().len();

    // Office (in Kitchen's group) and the Den; the Den is capped at 30.
    let targets = [
        resolve_room(&h, "Office").unwrap(),
        resolve_room(&h, "Den").unwrap(),
    ];
    let chime = clip("chime", 2);
    let cap = |room: &str| if room == "Den" { 30 } else { 100 };
    let report =
        with_clock_running(&sim, || fast().announce(&lan, &targets, &chime, 35, &cap)).unwrap();
    let house = &report.households[0];
    assert_eq!(house.outcome, Ok(Ending::Finished), "{report:#?}");
    assert!(house.restore_failed.is_empty(), "{report:#?}");
    assert_eq!(house.lead, id(&h, "Den"), "the Den leads where it is");
    assert_eq!(
        house.levels,
        [("Office".to_string(), 35), ("Den".to_string(), 30)]
    );

    // The Den played the clip with Office joined to it (out of Kitchen's
    // group), both loud enough and the Den unmuted; Kitchen never stopped.
    let log = &sim.soap_log()[log_from..];
    let den_sources = sent(log, "Den", "SetAVTransportURI", "CurrentURI");
    assert!(den_sources.contains(&chime.url), "{den_sources:?}");
    assert_eq!(
        sent(log, "Office", "SetAVTransportURI", "CurrentURI").first(),
        Some(&format!("x-rincon:{}", id(&h, "Den").0))
    );
    assert!(sent(log, "Office", "SetVolume", "DesiredVolume").contains(&"35".into()));
    assert!(sent(log, "Den", "SetVolume", "DesiredVolume").contains(&"30".into()));
    assert_eq!(
        sent(log, "Den", "SetMute", "DesiredMute").first(),
        Some(&"0".into())
    );
    assert!(sent(log, "Kitchen", "SetAVTransportURI", "CurrentURI").is_empty());
    assert!(
        !log.iter()
            .any(|e| e.room == "Kitchen" && e.action == "Stop")
    );

    // Back as it was: Office in Kitchen's group, the levels, the Den muted,
    // and Kitchen still on track 2 of its queue.
    let h = houses(&sim, &lan);
    assert_eq!(levels(&lan, &h, &rooms), before);
    let (state, track, position, uri) = playing(&lan, &h, "Kitchen");
    assert_eq!(
        (state, track, uri),
        (TransportState::Playing, 2, kitchen_uri)
    );
    assert!(position >= 40, "not rewound: {position}");
    assert_ne!(playing(&lan, &h, "Den").0, TransportState::Playing);
}

#[test]
fn a_member_steps_out_to_announce_and_back_in_while_its_group_plays_on() {
    let sim = sim();
    let lan = sim.lan();
    play_queue(&sim, &lan, "Kitchen");
    let h = houses(&sim, &lan);
    control::join(&lan, &h, &id(&h, "Office"), &id(&h, "Kitchen")).unwrap();
    let h = houses(&sim, &lan);
    let rooms = ["Kitchen", "Office"];
    let before = levels(&lan, &h, &rooms);
    let log_from = sim.soap_log().len();

    let office = [resolve_room(&h, "Office").unwrap()];
    let report = with_clock_running(&sim, || {
        fast().announce(&lan, &office, &clip("office", 2), 35, &no_cap)
    })
    .unwrap();
    let house = &report.households[0];
    assert_eq!(house.outcome, Ok(Ending::Finished), "{report:#?}");
    assert!(house.restore_failed.is_empty(), "{report:#?}");
    assert_eq!(house.lead, id(&h, "Office"));
    let log = &sim.soap_log()[log_from..];
    assert!(
        log.iter()
            .any(|e| e.room == "Office" && e.action == "BecomeCoordinatorOfStandaloneGroup")
    );
    assert!(sent(log, "Kitchen", "SetAVTransportURI", "CurrentURI").is_empty());
    let h = houses(&sim, &lan);
    assert_eq!(
        levels(&lan, &h, &rooms),
        before,
        "Office is back with Kitchen"
    );
    let (state, track, position, _) = playing(&lan, &h, "Kitchen");
    assert_eq!((state, track), (TransportState::Playing, 2));
    assert!(position >= 40, "not rewound: {position}");
}

#[test]
fn s2_a_whole_group_announces_in_place_and_a_stuck_clip_is_stopped() {
    let sim = sim();
    let lan = sim.lan();
    let h = houses(&sim, &lan);
    let living = id(&h, "Living Room");
    let (uri, didl) = control::spotify_track_source(
        &lan,
        &h,
        &living,
        "spotify:track:0SimAnnounceS200000001",
        "Aria",
    )
    .unwrap()
    .unwrap();
    control::play_uri(&lan, &h, &living, &uri, &didl).unwrap();
    control::join(&lan, &h, &id(&h, "Bedroom"), &living).unwrap();
    let h = houses(&sim, &lan);
    let rooms = ["Living Room", "Bedroom"];
    let before = levels(&lan, &h, &rooms);
    let log_from = sim.soap_log().len();

    // The clock stands still, so the clip never ends: at its length plus
    // the grace it is stopped, and the group's own music comes back.
    let targets = [
        resolve_room(&h, "Bedroom").unwrap(),
        resolve_room(&h, "Living Room").unwrap(),
    ];
    let announcer = Announcer::with_timing(Duration::from_millis(5), Duration::from_millis(300));
    let started = Instant::now();
    let report = announcer
        .announce(&lan, &targets, &clip("stuck", 1), 35, &no_cap)
        .unwrap();
    assert!(started.elapsed() >= Duration::from_millis(1300));
    let house = &report.households[0];
    assert_eq!(house.outcome, Ok(Ending::Deadline), "{report:#?}");
    assert!(house.restore_failed.is_empty(), "{report:#?}");
    assert_eq!(house.lead, living, "the group leads in place");
    let log = &sim.soap_log()[log_from..];
    assert!(sent(log, "Bedroom", "SetAVTransportURI", "CurrentURI").is_empty());
    assert!(
        log.iter()
            .any(|e| e.room == "Living Room" && e.action == "Stop")
    );
    let h = houses(&sim, &lan);
    assert_eq!(levels(&lan, &h, &rooms), before);
    let (state, _, _, now_uri) = playing(&lan, &h, "Living Room");
    assert_eq!((state, now_uri), (TransportState::Playing, uri));

    // A step that fails still puts everything back: the Bedroom cannot
    // play, so it never announces, and rejoins the Living Room.
    sim.upnp_fault("Bedroom", "Play", 701).unwrap();
    let report = announcer
        .announce(
            &lan,
            &[resolve_room(&h, "Bedroom").unwrap()],
            &clip("refused", 1),
            35,
            &no_cap,
        )
        .unwrap();
    let house = &report.households[0];
    assert!(
        house.outcome.as_ref().is_err_and(|e| e.contains("701")),
        "{report:#?}"
    );
    assert!(house.restore_failed.is_empty(), "{report:#?}");
    let h = houses(&sim, &lan);
    assert_eq!(levels(&lan, &h, &rooms), before);
    assert_eq!(playing(&lan, &h, "Living Room").0, TransportState::Playing);
}

#[test]
fn announcements_take_turns_per_household_and_households_run_side_by_side() {
    let sim = sim();
    let lan = sim.lan();
    play_queue(&sim, &lan, "Kitchen");
    let h = houses(&sim, &lan);
    let announcer = fast();
    let kitchen = [resolve_room(&h, "Kitchen").unwrap()];
    let (a, b) = (clip("first", 2), clip("second", 2));
    let log_from = sim.soap_log().len();
    let reports = with_clock_running(&sim, || {
        std::thread::scope(|s| {
            let one = s.spawn(|| announcer.announce(&lan, &kitchen, &a, 35, &no_cap));
            let two = s.spawn(|| announcer.announce(&lan, &kitchen, &b, 35, &no_cap));
            [one.join().unwrap().unwrap(), two.join().unwrap().unwrap()]
        })
    });
    for r in &reports {
        assert_eq!(r.households[0].outcome, Ok(Ending::Finished), "{r:#?}");
    }
    assert_eq!(announcer.pending(), 0);

    // Kitchen's sources in order: one clip, its queue back, the other clip,
    // its queue back. The second never cut into the first.
    let queue = format!("x-rincon-queue:{}#0", id(&h, "Kitchen").0);
    let sources = sent(
        &sim.soap_log()[log_from..],
        "Kitchen",
        "SetAVTransportURI",
        "CurrentURI",
    );
    assert_eq!(sources.len(), 4, "{sources:?}");
    assert_eq!([&sources[1], &sources[3]], [&queue, &queue], "{sources:?}");
    let mut clips = [sources[0].clone(), sources[2].clone()];
    clips.sort();
    assert_eq!(clips, [a.url.clone(), b.url.clone()]);

    // Both households in one call: each announces and is put back.
    let h = houses(&sim, &lan);
    let both = [
        resolve_room(&h, "Kitchen").unwrap(),
        resolve_room(&h, "Bedroom").unwrap(),
    ];
    let report = with_clock_running(&sim, || {
        announcer.announce(&lan, &both, &clip("both", 2), 35, &no_cap)
    })
    .unwrap();
    assert_eq!(report.households.len(), 2);
    assert!(
        report
            .households
            .iter()
            .all(|x| x.outcome == Ok(Ending::Finished) && x.restore_failed.is_empty()),
        "{report:#?}"
    );
    let h = houses(&sim, &lan);
    let (state, track, _, _) = playing(&lan, &h, "Kitchen");
    assert_eq!((state, track), (TransportState::Playing, 2));
    assert_ne!(playing(&lan, &h, "Bedroom").0, TransportState::Playing);
}
