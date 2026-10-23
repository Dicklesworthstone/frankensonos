//! Adding to a group's queue through `Surface`, against `fsonos-sim`
//! players over real sockets: a track right after the one playing, an album
//! at the end (its tracks), undo taking the newest addition off, and an undo
//! that finds the queue changed leaving it alone and saying so.

use fsonos_api::plan::plan_volume;
use fsonos_api::surface::queue::QueueAddRequest;
use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{ErrorCode, VolumeRequest};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::MemStore;
use fsonos_core::{HouseholdState, control, resolve_room};
use fsonos_proto::control::remove_all_tracks_from_queue;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan};
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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

/// Passes everything to the sim, but loses every queue add's reply: the
/// tracks are queued all the same. Counts the adds sent.
struct LostAdd {
    lan: SimLan,
    adds: Arc<AtomicUsize>,
}

impl Transport for LostAdd {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let answer = self.lan.soap_post(host, path, action, body)?;
        if action.contains("#AddURIToQueue") {
            self.adds.fetch_add(1, Ordering::SeqCst);
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected lost add reply".into(),
            });
        }
        Ok(answer)
    }
}

fn lost_add_surface(sim: &SimHandle, adds: &Arc<AtomicUsize>) -> Surface {
    let houses = house(sim);
    Surface::new(
        Box::new(LostAdd {
            lan: sim.lan(),
            adds: Arc::clone(adds),
        }),
        Box::new(move |_| Ok(houses.clone())),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "mcp")
}

/// A track whose add reply is lost is read back from the queue (19d76f11):
/// the add is confirmed without being sent twice, and undo can take it off.
#[test]
fn a_track_add_with_a_lost_reply_is_confirmed_once_and_undoable() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let adds = Arc::new(AtomicUsize::new(0));
    let s = lost_add_surface(&sim, &adds);
    let added = s
        .queue_add(
            &Client::Cli,
            &add("spotify:track:0SimQueueTrack00000009", "Encore", false),
        )
        .unwrap();
    assert_eq!(added.count, 1, "{added:?}");
    assert_eq!(adds.load(Ordering::SeqCst), 1, "the add was not replayed");
    assert_eq!(titles(&s), ["Encore"]);
    let undone = s.undo(&Client::Cli, true).unwrap().unwrap();
    assert!(!undone.unqueued.is_empty(), "{undone:?}");
    assert!(titles(&s).is_empty(), "{undone:?}");
    sim.shutdown();
}

/// An album whose add reply is lost may have queued an unknown expansion: it
/// is logged as failed, never sent twice, and undo stops at it rather than
/// reach past it.
#[test]
fn an_album_add_with_a_lost_reply_is_logged_and_stops_undo() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let adds = Arc::new(AtomicUsize::new(0));
    let s = lost_add_surface(&sim, &adds);
    let failed = s
        .queue_add(
            &Client::Cli,
            &add(
                "https://open.spotify.com/album/0SimQueueAlbum00000001",
                "Suite",
                false,
            ),
        )
        .unwrap_err();
    assert_eq!(adds.load(Ordering::SeqCst), 1, "the add was not replayed");
    let refused = s.undo(&Client::Cli, true).unwrap_err();
    assert!(
        refused.detail.contains("queue edit"),
        "{refused:?} after {failed:?}"
    );
    sim.shutdown();
}

/// Passes everything to the sim, but once armed loses the next
/// GetMediaInfo reply: the first read a Spotify add makes, before it sends
/// anything.
struct LostRead {
    lan: SimLan,
    armed: Arc<AtomicBool>,
}

impl Transport for LostRead {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.contains("#GetMediaInfo") && self.armed.swap(false, Ordering::SeqCst) {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected lost read".into(),
            });
        }
        self.lan.soap_post(host, path, action, body)
    }
}

/// An add that fails before it sends anything queued nothing: it does not
/// stand in undo's way, and the volume set before it is undone.
#[test]
fn an_add_that_fails_before_sending_leaves_undo_alone() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let houses = house(&sim);
    let host = resolve_room(&houses, ROOM).unwrap().player.ip;
    let level = || fsonos_proto::control::get_volume(&sim.lan(), host).unwrap();
    let was = level();
    let armed = Arc::new(AtomicBool::new(false));
    let s = Surface::new(
        Box::new(LostRead {
            lan: sim.lan(),
            armed: Arc::clone(&armed),
        }),
        Box::new(move |_| Ok(houses.clone())),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "mcp");
    let req = VolumeRequest {
        zone: ROOM.into(),
        volume: Some(i64::from(if was >= 50 { was - 5 } else { was + 5 })),
        delta: None,
        group: false,
    };
    s.control(&Client::Cli, "set_volume", |h| plan_volume(h, &req))
        .unwrap();
    armed.store(true, Ordering::SeqCst);
    let failed = s
        .queue_add(
            &Client::Cli,
            &add("spotify:track:0SimQueueTrack00000009", "Encore", false),
        )
        .unwrap_err();
    let undone = s
        .undo(&Client::Cli, true)
        .unwrap()
        .unwrap_or_else(|| panic!("the volume set is undone past {failed:?}"));
    assert!(undone.failures.is_empty(), "{undone:?}");
    assert_eq!(level(), was);
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
        ("spotify:genre:0SimQueueGenre00000001", "a genre"),
    ] {
        let err = s
            .queue_add(&Client::Cli, &add(uri, "x", false))
            .unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{why}: {err:?}");
    }
    sim.shutdown();
}
