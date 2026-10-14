//! Adding to a group's queue through `Surface`, against `fsonos-sim`
//! players over real sockets: a track right after the one playing, an album
//! at the end (its tracks), undo taking the newest addition off, and an undo
//! that finds the queue changed leaving it alone and saying so.

use fsonos_api::ErrorCode;
use fsonos_api::surface::queue::QueueAddRequest;
use fsonos_api::surface::{Surface, Survey};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control::remove_all_tracks_from_queue;
use fsonos_sim::{SimHandle, SimHousehold};
use std::time::Duration;

const ROOM: &str = "Living Room";

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
    .with_action_log(Box::new(MemStore::default()), "mcp")
}

fn house(sim: &SimHandle) -> Vec<HouseholdState> {
    fsonos_core::inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households
}

fn add(source_uri: &str, title: &str, next: bool) -> QueueAddRequest {
    QueueAddRequest {
        zone: ROOM.into(),
        source_uri: source_uri.into(),
        title: Some(title.into()),
        next,
    }
}

fn titles(s: &Surface) -> Vec<String> {
    s.queue(&Client::Cli, ROOM, 0, 100)
        .unwrap()
        .items
        .into_iter()
        .map(|i| i.title)
        .collect()
}

#[test]
fn a_track_goes_next_an_album_to_the_end_and_undo_takes_them_off() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let lan = sim.lan();
    let h = house(&sim);
    let coordinator = resolve_room(&h, ROOM).unwrap().coordinator.id.clone();
    let tracks = [
        ("spotify:track:0SimQueueTrack00000001", "I. Allegro"),
        ("spotify:track:0SimQueueTrack00000002", "II. Andante"),
        ("spotify:track:0SimQueueTrack00000003", "III. Presto"),
    ];
    let first = control::queue_spotify_tracks(&lan, &h, &coordinator, &tracks)
        .unwrap()
        .expect("the household has a Spotify favorite to learn from");
    control::play_queue_from(&lan, &h, &coordinator, first).unwrap();

    let s = surface(&sim);
    let next = s
        .queue_add(
            &Client::Cli,
            &add("spotify:track:0SimQueueTrack00000009", "Encore", true),
        )
        .unwrap();
    assert_eq!(
        (next.first, next.count, next.playing_queue),
        (2, 1, true),
        "{next:?}"
    );
    assert_eq!(
        titles(&s),
        ["I. Allegro", "Encore", "II. Andante", "III. Presto"]
    );

    let album = s
        .queue_add(
            &Client::Cli,
            &add(
                "https://open.spotify.com/album/0SimQueueAlbum00000001",
                "Suite",
                false,
            ),
        )
        .unwrap();
    assert_eq!((album.first, album.count), (5, 3), "{album:?}");
    assert!(album.done.contains("queued 3 track(s)"), "{}", album.done);
    assert_eq!(titles(&s).len(), 7);

    // Undo takes the newest addition off, then the one before it.
    let undone = s.undo(&Client::Cli, true).unwrap().unwrap();
    assert_eq!(
        undone.unqueued,
        std::slice::from_ref(&coordinator),
        "{undone:?}"
    );
    assert!(
        undone
            .summary
            .contains("the tracks queued in Living Room taken off"),
        "{}",
        undone.summary
    );
    s.undo(&Client::Cli, true).unwrap().unwrap();
    assert_eq!(titles(&s), ["I. Allegro", "II. Andante", "III. Presto"]);
    sim.shutdown();
}

#[test]
fn an_undo_leaves_a_queue_that_changed_alone() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let lan = sim.lan();
    let s = surface(&sim);
    s.queue_add(
        &Client::Cli,
        &add("spotify:track:0SimQueueTrack00000009", "Encore", false),
    )
    .unwrap();
    let h = house(&sim);
    let host = resolve_room(&h, ROOM).unwrap().coordinator.ip;
    remove_all_tracks_from_queue(&lan, host).unwrap();

    let undone = s.undo(&Client::Cli, true).unwrap().unwrap();
    assert!(undone.unqueued.is_empty(), "{undone:?}");
    assert!(
        undone.unqueue_failures[0].1.contains("the queue changed"),
        "{undone:?}"
    );

    for (uri, why) in [
        ("x-rincon-mp3radio://example.invalid/a.mp3", "a stream"),
        ("spotify:artist:0SimQueueArtist0000001", "an artist"),
    ] {
        let err = s
            .queue_add(&Client::Cli, &add(uri, "x", false))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{why}: {err:?}");
    }
    sim.shutdown();
}
