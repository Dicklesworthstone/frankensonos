//! Repeat avoidance follows the music across recordings, while each pick
//! still contains one intact recording and all permitted versions can play.

use std::collections::HashSet;

use super::{DjConfig, Factor, PickContext, PlayRecord, Rng, WorkPool, pick_next, plan};
use crate::library::{LibraryItem, Origin};
use crate::test_shelf::{item, works_of};

fn recording(album: &str, work: u8) -> Vec<LibraryItem> {
    ["I. Allegro", "II. Andante"]
        .into_iter()
        .enumerate()
        .map(|(index, movement)| {
            let mut track = item(
                format!("spotify:track:{album}{index}"),
                format!("Sonata No. {work}: {movement}"),
                "Ludwig van Beethoven",
                (
                    format!("Recording {album}"),
                    format!("spotify:album:{album}"),
                ),
                300,
                Origin::SavedAlbum,
            );
            track.disc_number = Some(1);
            track.track_number = Some(u32::try_from(index + 1).unwrap());
            track
        })
        .collect()
}

fn shelf() -> WorkPool {
    let tracks: Vec<_> = [("first", 1), ("second", 1), ("third", 2), ("fourth", 3)]
        .into_iter()
        .flat_map(|(album, work)| recording(album, work))
        .collect();
    let pool = works_of(&tracks);
    assert_eq!(
        pool.len(),
        4,
        "recordings stay independent queue selections"
    );
    assert_eq!(pool.works()[0].work_key, pool.works()[1].work_key);
    pool
}

fn without_artist_spacing() -> DjConfig {
    DjConfig {
        artist_cooldown_works: 0,
        ..DjConfig::default()
    }
}

#[test]
fn a_movement_cools_every_recording_by_play_count_and_time() {
    let pool = shelf();
    let first = &pool.works()[0];
    // Only the second movement was heard; the other album is still the same
    // music. Check each cooldown independently so spacing cannot hide it.
    let history = [PlayRecord::at(&first.movements[1].track.source_uri, 1_000)];
    let ctx = PickContext {
        history: &history,
        now: Some(1_060),
        ..PickContext::default()
    };
    for config in [
        DjConfig {
            work_cooldown_secs: 0,
            ..without_artist_spacing()
        },
        DjConfig {
            work_cooldown_plays: 0,
            ..without_artist_spacing()
        },
    ] {
        for seed in 0..64 {
            let next = pick_next(&pool, &ctx, &config, &mut Rng::new(seed)).unwrap();
            assert_ne!(next.work.work_key, first.work_key);
        }
    }
}

#[test]
fn queued_or_heard_elsewhere_cools_all_recordings_until_expiry() {
    let pool = shelf();
    let first = &pool.works()[0];
    let config = without_artist_spacing();
    let elsewhere = [PlayRecord::at(&first.movements[0].track.source_uri, 1_000)];
    let ctx = PickContext {
        elsewhere: &elsewhere,
        now: Some(1_000),
        ..PickContext::default()
    };
    for seed in 0..64 {
        let next = pick_next(&pool, &ctx, &config, &mut Rng::new(seed)).unwrap();
        assert_ne!(next.work.work_key, first.work_key);
    }
    let expired = PickContext {
        now: Some(1_000 + config.work_cooldown_secs),
        ..ctx
    };
    let after: HashSet<_> = (0..64)
        .map(|seed| {
            pick_next(&pool, &expired, &config, &mut Rng::new(seed))
                .unwrap()
                .work
                .album_key
                .as_str()
        })
        .collect();
    assert!(after.contains(first.album_key.as_str()));
    assert!(after.contains(pool.works()[1].album_key.as_str()));
}

#[test]
fn remastered_songs_share_cooldown_without_merging_their_recordings() {
    let mut tracks = Vec::new();
    for (id, title) in [
        ("original", "Evening Lights"),
        ("remaster", "Evening Lights - Remastered 2011"),
        ("other", "A Different Day"),
    ] {
        let mut track = item(
            format!("spotify:track:{id}"),
            title.into(),
            "Synthetic Pop Artist",
            (id.into(), format!("spotify:album:{id}")),
            180,
            Origin::SavedAlbum,
        );
        track.genres = vec!["pop".into()];
        tracks.push(track);
    }
    let pool = works_of(&tracks);
    assert_eq!(pool.len(), 3);
    assert_eq!(pool.works()[0].work_key, pool.works()[1].work_key);
    assert!(pool.works().iter().all(|work| !work.is_classical()));
    let history = [PlayRecord::new("spotify:track:original")];
    let ctx = PickContext {
        history: &history,
        ..PickContext::default()
    };
    for seed in 0..32 {
        let next = pick_next(&pool, &ctx, &without_artist_spacing(), &mut Rng::new(seed)).unwrap();
        assert_eq!(next.movements.len(), 1);
        assert_eq!(next.movements[0].track.source_uri, "spotify:track:other");
    }
}

#[test]
fn many_editions_cannot_force_rotation_while_a_different_work_is_fresh() {
    let mut tracks: Vec<_> = (0..32)
        .flat_map(|version| recording(&format!("edition{version}"), 1))
        .collect();
    tracks.extend(recording("fresh", 2));
    let pool = works_of(&tracks);
    let history = [PlayRecord::new("spotify:track:edition00")];
    let ctx = PickContext {
        history: &history,
        ..PickContext::default()
    };
    for seed in 0..64 {
        let next = pick_next(&pool, &ctx, &without_artist_spacing(), &mut Rng::new(seed)).unwrap();
        assert_eq!(next.work.album_key, "spotify:album:fresh");
        assert!(!next.reason.has(Factor::Rotation));
    }
}

#[test]
fn exhausted_small_shelves_rotate_works_and_retain_recording_choices() {
    let pool = shelf();
    let config = without_artist_spacing();
    let history: Vec<_> = [0, 2, 3]
        .into_iter()
        .map(|index| PlayRecord::new(&pool.works()[index].movements[0].track.source_uri))
        .collect();
    let ctx = PickContext {
        history: &history,
        ..PickContext::default()
    };
    let mut recordings = HashSet::new();
    for seed in 0..64 {
        let next = pick_next(&pool, &ctx, &config, &mut Rng::new(seed)).unwrap();
        assert_eq!(next.work.work_key, pool.works()[0].work_key);
        assert!(next.reason.has(Factor::Rotation));
        recordings.insert(next.work.album_key.as_str());
    }
    assert_eq!(
        recordings.len(),
        2,
        "either recording of the oldest work can play"
    );

    let picks = plan(&pool, &ctx, &config, 24, &mut Rng::new(7));
    for cycle in picks.chunks_exact(3) {
        let keys: HashSet<_> = cycle.iter().map(|pick| &pick.work.work_key).collect();
        assert_eq!(keys.len(), 3, "hear every work before repeating one");
    }
    for pick in picks {
        assert_eq!(pick.movements.len(), 2);
        assert_eq!(pick.movements[0].track_number, Some(1));
        assert_eq!(pick.movements[1].track_number, Some(2));
        assert!(
            pick.movements
                .iter()
                .all(|track| track.album_key == pick.work.album_key)
        );
    }
}

/// A common uneven library: several works by one artist, one by another.
/// Keep both the song and intact classical-recording paths under test.
fn uneven_shelf(classical: bool) -> WorkPool {
    let mut tracks = Vec::new();
    for (prefix, count, artist) in [
        ("many", 6, "Ludwig van Beethoven"),
        ("single", 1, "Johann Sebastian Bach"),
    ] {
        for number in 1..=count {
            let album = format!("{prefix}{number}");
            if classical {
                let mut movements = recording(&album, number);
                for movement in &mut movements {
                    movement.artists[0] = artist.into();
                }
                tracks.extend(movements);
            } else {
                let mut song = item(
                    format!("spotify:track:{album}"),
                    format!("Song {number}"),
                    &format!("Synthetic {prefix} Artist"),
                    (album.clone(), format!("spotify:album:{album}")),
                    180,
                    Origin::SavedAlbum,
                );
                song.genres = vec!["pop".into()];
                tracks.push(song);
            }
        }
    }
    let pool = works_of(&tracks);
    assert_eq!(pool.len(), 7);
    pool
}

#[test]
fn artist_spacing_still_chooses_a_fresh_different_artist() {
    for classical in [false, true] {
        let pool = uneven_shelf(classical);
        let history = [PlayRecord::at(
            &pool.works()[0].movements[0].track.source_uri,
            1_000,
        )];
        let ctx = PickContext {
            history: &history,
            now: Some(1_060),
            ..PickContext::default()
        };
        for seed in 0..64 {
            let next = pick_next(&pool, &ctx, &DjConfig::default(), &mut Rng::new(seed)).unwrap();
            assert_eq!(next.work.work_key, pool.works()[6].work_key);
            assert!(!next.reason.has(Factor::Rotation));
        }
    }
}

#[test]
fn artist_spacing_does_not_repeat_a_work_while_unheard_music_remains() {
    for classical in [false, true] {
        let pool = uneven_shelf(classical);
        let history: Vec<_> = [6, 0, 1]
            .into_iter()
            .map(|index| PlayRecord::at(&pool.works()[index].movements[0].track.source_uri, 1_000))
            .collect();
        let ctx = PickContext {
            history: &history,
            now: Some(1_060),
            ..PickContext::default()
        };
        for seed in 0..64 {
            let next = pick_next(&pool, &ctx, &DjConfig::default(), &mut Rng::new(seed)).unwrap();
            assert!(
                pool.works()[2..6]
                    .iter()
                    .any(|work| work.work_key == next.work.work_key),
                "artist spacing must yield to unheard music, not repeat {}",
                next.work.title
            );
            assert!(!next.reason.has(Factor::Rotation));
            assert_eq!(next.movements.len(), if classical { 2 } else { 1 });
        }
    }
}

#[test]
fn artist_spacing_does_not_repeat_a_work_queued_in_another_room() {
    for classical in [false, true] {
        let pool = uneven_shelf(classical);
        let history: Vec<_> = [0, 1]
            .into_iter()
            .map(|index| PlayRecord::at(&pool.works()[index].movements[0].track.source_uri, 1_000))
            .collect();
        let elsewhere = [PlayRecord::at(
            &pool.works()[6].movements[0].track.source_uri,
            1_060,
        )];
        let ctx = PickContext {
            history: &history,
            elsewhere: &elsewhere,
            now: Some(1_060),
            ..PickContext::default()
        };
        for seed in 0..64 {
            let next = pick_next(&pool, &ctx, &DjConfig::default(), &mut Rng::new(seed)).unwrap();
            assert_ne!(next.work.work_key, pool.works()[6].work_key);
            assert!(!next.reason.has(Factor::Rotation));
        }
    }
}

#[test]
fn artist_spacing_does_not_skip_older_works_when_an_uneven_shelf_rotates() {
    for classical in [false, true] {
        let pool = uneven_shelf(classical);
        let history: Vec<_> = [0, 1, 2, 3, 6, 4, 5]
            .into_iter()
            .map(|index| PlayRecord::new(&pool.works()[index].movements[0].track.source_uri))
            .collect();
        let ctx = PickContext {
            history: &history,
            ..PickContext::default()
        };
        let picks = plan(&pool, &ctx, &DjConfig::default(), 21, &mut Rng::new(7));
        assert_eq!(picks[0].work.work_key, pool.works()[0].work_key);
        for cycle in picks.chunks_exact(pool.len()) {
            let keys: HashSet<_> = cycle.iter().map(|pick| &pick.work.work_key).collect();
            assert_eq!(
                keys.len(),
                pool.len(),
                "hear every work before repeating one"
            );
        }
        for pick in picks {
            assert!(pick.reason.has(Factor::Rotation));
            assert_eq!(pick.movements.len(), if classical { 2 } else { 1 });
        }
    }
}
