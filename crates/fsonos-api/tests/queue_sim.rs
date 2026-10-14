//! The queue view through `Surface`, against `fsonos-sim` players over real
//! sockets: a page of a group's queue in order, how many tracks it holds,
//! the one playing, and paging past the first page.

use fsonos_api::ErrorCode;
use fsonos_api::surface::{Surface, Survey};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_sim::{SimHandle, SimHousehold};
use std::time::Duration;

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

fn house(sim: &SimHandle) -> Vec<HouseholdState> {
    fsonos_core::inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households
}

#[test]
fn a_groups_queue_reads_in_order_with_the_track_playing() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let lan = sim.lan();
    let h = house(&sim);
    let room = "Living Room";
    let coordinator = resolve_room(&h, room).unwrap().coordinator.id.clone();
    let tracks = [
        ("spotify:track:0SimQueueTrack00000001", "I. Allegro"),
        ("spotify:track:0SimQueueTrack00000002", "II. Andante"),
        ("spotify:track:0SimQueueTrack00000003", "III. Presto"),
    ];
    let first = control::queue_spotify_tracks(&lan, &h, &coordinator, &tracks)
        .unwrap()
        .expect("the household has a Spotify favorite to learn from");
    control::play_queue_from(&lan, &h, &coordinator, first + 1).unwrap();

    let s = surface(&sim);
    let queue = s.queue(&Client::Cli, room, 0, 2).unwrap();
    assert_eq!(queue.zone, room);
    assert_eq!(
        (queue.total, queue.playing),
        (3, Some(first + 1)),
        "{queue:?}"
    );
    let titles: Vec<&str> = queue.items.iter().map(|i| i.title.as_str()).collect();
    assert_eq!(titles, ["I. Allegro", "II. Andante"], "a page of 2");
    assert_eq!(queue.items[0].position, 1);
    assert!(
        queue.items[0]
            .uri
            .as_deref()
            .is_some_and(|u| u.contains("0SimQueueTrack00000001")),
        "{queue:?}"
    );
    assert!(
        queue.text().contains("▶   2. II. Andante"),
        "{}",
        queue.text()
    );
    assert!(queue.text().contains("… 1 more"), "{}", queue.text());

    // The next page starts where the first ended.
    let rest = s.queue(&Client::Cli, room, 2, 50).unwrap();
    assert_eq!(rest.items.len(), 1);
    assert_eq!(
        (rest.items[0].position, rest.items[0].title.as_str()),
        (3, "III. Presto")
    );

    let err = s.queue(&Client::Cli, "Atticc", 0, 50).unwrap_err();
    assert_eq!(err.code, ErrorCode::UnknownRoom, "{err:?}");
}
