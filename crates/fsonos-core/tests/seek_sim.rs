//! `control::seek` against `fsonos-sim` players over real sockets: a seek
//! from the track's start, one from where it is (kept within the track),
//! and the position the speaker reports after each.

use fsonos_core::control::{self, Seek};
use fsonos_core::{HouseholdState, resolve_room};
use fsonos_sim::{SimHandle, SimHousehold};
use std::time::Duration;

fn house(sim: &SimHandle) -> Vec<HouseholdState> {
    fsonos_core::inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households
}

#[test]
fn an_absolute_and_a_relative_seek_land_in_the_track() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let lan = sim.lan();
    let h = house(&sim);
    let coordinator = resolve_room(&h, "Living Room")
        .unwrap()
        .coordinator
        .id
        .clone();
    // A queued track of three minutes, playing.
    let tracks = [("spotify:track:0SimSeekTrack000000001", "Andante")];
    let first = control::queue_spotify_tracks(&lan, &h, &coordinator, &tracks)
        .unwrap()
        .expect("the household has a Spotify favorite to learn from");
    control::play_queue_from(&lan, &h, &coordinator, first).unwrap();
    let position = || {
        control::playback(&lan, &h, &coordinator)
            .unwrap()
            .position
            .position_secs
            .unwrap_or_default()
    };

    assert_eq!(
        control::seek(&lan, &h, &coordinator, Seek::At(90)).unwrap(),
        90
    );
    assert!((90..92).contains(&position()), "{}", position());
    let back = control::seek(&lan, &h, &coordinator, Seek::By(-30)).unwrap();
    assert!((60..62).contains(&back), "{back}");
    assert!((60..63).contains(&position()), "{}", position());
    // Past the end: the track's last second, not the next track.
    assert_eq!(
        control::seek(&lan, &h, &coordinator, Seek::By(10_000)).unwrap(),
        179
    );
    let before = control::seek(&lan, &h, &coordinator, Seek::By(-10_000)).unwrap();
    assert_eq!(before, 0);
    sim.shutdown();
}
