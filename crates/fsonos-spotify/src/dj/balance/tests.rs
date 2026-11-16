use std::collections::HashSet;

use super::*;
use crate::classical::EnergyBasis;
use crate::dj::{
    Aim, DjConfig, Factor, PickContext, Recency, SCALE, Scoring, UnavailableTrack, pick_next,
};
use crate::feedback::{FeedbackModel, FeedbackSignal, Signal};
use crate::library::{LibraryItem, Origin};
use crate::prefs::{Items, Preferences, Taste};
use crate::steer::{DjConstraints, Moods, Steer};
use crate::test_shelf::{item, works_of};

fn recording(album: &str, number: usize, artist: &str, classical: bool) -> Vec<LibraryItem> {
    let movements = if classical { 2 } else { 1 };
    (0..movements)
        .map(|movement| {
            let title = if classical {
                format!(
                    "Sonata No. {number}: {}",
                    ["I. Allegro", "II. Andante"][movement]
                )
            } else {
                format!("Song {number}")
            };
            let mut track = item(
                format!("spotify:track:{album}{movement}"),
                title,
                artist,
                (album.into(), format!("spotify:album:{album}")),
                180,
                Origin::SavedAlbum,
            );
            track.disc_number = Some(1);
            track.track_number = Some(u32::try_from(movement + 1).unwrap());
            if !classical {
                track.genres = vec!["pop".into()];
            }
            track
        })
        .collect()
}

#[test]
fn extra_editions_do_not_change_the_work_draw_or_join_recordings() {
    for classical in [false, true] {
        let (first, second) = if classical {
            ("Ludwig van Beethoven", "Johann Sebastian Bach")
        } else {
            ("Synthetic First Artist", "Synthetic Second Artist")
        };
        let mut tracks = recording("original", 1, first, classical);
        tracks.extend(recording("other", 2, second, classical));
        let base = works_of(&tracks);
        for edition in 0..31 {
            tracks.extend(recording(&format!("edition{edition}"), 1, first, classical));
        }
        let expanded = works_of(&tracks);
        let all: Vec<_> = (0..expanded.len()).collect();
        let counts = composer_counts(&expanded, &all);
        assert_eq!(counts.len(), 2);
        assert!(counts.values().all(|&count| count == 1));
        let scores = vec![SCALE; expanded.len()];
        let lottery = RecordingLottery::new(&expanded, &all, &scores);
        assert_eq!(lottery.groups.len(), 2);
        assert_eq!(lottery.groups[0].count, 32);
        assert_eq!(lottery.groups[0].weight(), lottery.groups[1].weight());

        let config = DjConfig::default();
        let context = PickContext::default();
        let mut heard = HashSet::new();
        for seed in 0..2_048 {
            let expected = pick_next(&base, &context, &config, &mut Rng::new(seed)).unwrap();
            let actual = pick_next(&expanded, &context, &config, &mut Rng::new(seed)).unwrap();
            assert_eq!(actual.work.work_key, expected.work.work_key, "seed {seed}");
            assert_eq!(actual.movements, actual.work.movements.as_slice());
            assert_eq!(actual.movements.len(), if classical { 2 } else { 1 });
            assert!(
                actual
                    .movements
                    .iter()
                    .all(|movement| { movement.album_key == actual.work.album_key })
            );
            heard.insert(actual.work.album_key.as_str());
        }
        assert_eq!(
            heard.len(),
            expanded.len(),
            "every intact edition remains selectable"
        );
    }
}

#[test]
fn excluded_catalogues_do_not_dilute_an_artists_permitted_song() {
    let config = DjConfig::default();
    let moods = Moods::builtin();
    for exclusion in [
        "banned",
        "avoided",
        "explicit",
        "strict",
        "unavailable",
        "long",
    ] {
        let mut tracks = recording("one", 1, "Synthetic First Artist", false);
        tracks.extend(recording("two", 2, "Synthetic Second Artist", false));
        let mut preferences = Preferences::default();
        let mut unavailable = Vec::new();
        let steer = (exclusion == "strict").then(|| Steer {
            mood: None,
            constraints: DjConstraints {
                include_genres: vec!["pop".into()],
                strict: true,
                ..DjConstraints::default()
            },
        });
        let base = works_of(&tracks);
        for number in 3..103 {
            let mut extra = recording(
                &format!("excluded{number}"),
                number,
                "Synthetic First Artist",
                false,
            );
            let track = &mut extra[0];
            match exclusion {
                "banned" => preferences.ban.tracks.push(track.source_uri.clone()),
                "avoided" | "strict" => track.genres = vec!["rock".into()],
                "explicit" => track.explicit = true,
                "unavailable" => unavailable.push(UnavailableTrack {
                    source_uri: track.source_uri.clone(),
                    retry_at: 2_000,
                }),
                "long" => track.duration_secs = Some(7_200),
                _ => unreachable!(),
            }
            tracks.extend(extra);
        }
        if exclusion == "avoided" {
            preferences.avoid.genres = vec!["rock".into()];
        }
        let expanded = works_of(&tracks).with_preferences(preferences, &moods);
        let context = PickContext {
            unavailable: &unavailable,
            now: Some(1_000),
            steer: steer.as_ref(),
            ..PickContext::default()
        };
        for seed in 0..128 {
            let expected = pick_next(&base, &context, &config, &mut Rng::new(seed)).unwrap();
            let actual = pick_next(&expanded, &context, &config, &mut Rng::new(seed)).unwrap();
            assert_eq!(
                actual.work.work_key, expected.work.work_key,
                "{exclusion}, seed {seed}"
            );
            assert!(
                !actual.reason.has(Factor::ArtistBalance),
                "{exclusion}: one permitted song each"
            );
        }
    }
}

#[test]
fn forbidden_editions_do_not_reduce_the_eligible_recordings_share() {
    let mut tracks = recording("one", 1, "Ludwig van Beethoven", true);
    tracks.extend(recording("two", 2, "Johann Sebastian Bach", true));
    let base = works_of(&tracks);
    let mut preferences = Preferences::default();
    for number in 0..30 {
        let album = format!("excluded{number}");
        let mut extra = recording(&album, 1, "Ludwig van Beethoven", true);
        if number % 2 == 0 {
            extra[1].explicit = true;
        } else {
            preferences
                .ban
                .albums
                .push(format!("spotify:album:{album}"));
        }
        tracks.extend(extra);
    }
    let expanded = works_of(&tracks).with_preferences(preferences, &Moods::builtin());
    for seed in 0..128 {
        let expected = pick_next(
            &base,
            &PickContext::default(),
            &DjConfig::default(),
            &mut Rng::new(seed),
        )
        .unwrap();
        let actual = pick_next(
            &expanded,
            &PickContext::default(),
            &DjConfig::default(),
            &mut Rng::new(seed),
        )
        .unwrap();
        assert_eq!(actual.work.album_key, expected.work.album_key);
        assert!(!actual.reason.summary.contains("eligible recordings"));
    }
}

#[test]
fn favored_and_pinned_recordings_keep_their_original_score_ratios() {
    let mut tracks = recording("plain", 1, "Ludwig van Beethoven", true);
    let mut favored = recording("favored", 1, "Ludwig van Beethoven", true);
    for track in &mut favored {
        track.artists.push("Synthetic Favored Performer".into());
    }
    tracks.extend(favored);
    tracks.extend(recording("pinned", 1, "Ludwig van Beethoven", true));
    let preferences = Preferences {
        favor: Taste {
            artists: vec!["Synthetic Favored Performer".into()],
            ..Taste::default()
        },
        pin: Items {
            albums: vec!["spotify:album:pinned".into()],
            ..Items::default()
        },
        ..Preferences::default()
    };
    let pool = works_of(&tracks).with_preferences(preferences, &Moods::builtin());
    let candidates: Vec<_> = (0..pool.len()).collect();
    let composers = composer_counts(&pool, &candidates);
    let config = DjConfig::default();
    let recency = Recency::scan(&pool, &[], &config);
    let scoring = Scoring {
        pool: &pool,
        recency: &recency,
        aim: None,
        config: &config,
        feedback: None,
        composers: &composers,
    };
    let scores: Vec<_> = candidates
        .iter()
        .map(|&index| scoring.weight(index, None))
        .collect();
    assert_eq!(scores, [SCALE, SCALE * 2, SCALE * 3]);
    let lottery = RecordingLottery::new(&pool, &candidates, &scores);
    assert_eq!(lottery.groups.len(), 1);
    assert_eq!(lottery.groups[0].weight(), SCALE * 3);
    assert_eq!(lottery.groups[0].preferred_count, 1);
    let mut observed = HashSet::new();
    for seed in 0..128 {
        let mut expected_rng = Rng::new(seed);
        expected_rng.below_u64(SCALE * 3);
        let recording = draw(&[SCALE, SCALE * 2, SCALE * 3], &mut expected_rng);
        let actual =
            pick_next(&pool, &PickContext::default(), &config, &mut Rng::new(seed)).unwrap();
        assert_eq!(actual.work.album_key, pool.works[recording].album_key);
        assert_eq!(actual.reason.has(Factor::Preference), recording != 0);
        assert!(
            actual
                .reason
                .summary
                .ends_with("one of 3 eligible recordings")
        );
        observed.insert(recording);
    }
    assert_eq!(observed.len(), 3);
}

#[test]
fn neutral_editions_do_not_dilute_manual_preference_or_feedback_shielding() {
    for pinned in [false, true] {
        let mut tracks = recording("preferred", 1, "Ludwig van Beethoven", true);
        for track in &mut tracks {
            track.artists.push("Synthetic Favored Performer".into());
        }
        tracks.extend(recording("other", 2, "Johann Sebastian Bach", true));
        let preferences = if pinned {
            Preferences {
                pin: Items {
                    albums: vec!["spotify:album:preferred".into()],
                    ..Items::default()
                },
                ..Preferences::default()
            }
        } else {
            Preferences {
                favor: Taste {
                    artists: vec!["Synthetic Favored Performer".into()],
                    ..Taste::default()
                },
                ..Preferences::default()
            }
        };
        let moods = Moods::builtin();
        let base = works_of(&tracks).with_preferences(preferences.clone(), &moods);
        for number in 0..99 {
            tracks.extend(recording(
                &format!("neutral{number}"),
                1,
                "Ludwig van Beethoven",
                true,
            ));
        }
        let expanded = works_of(&tracks).with_preferences(preferences, &moods);
        let feedback = FeedbackModel::from_signals(
            &[FeedbackSignal::about(
                &expanded.works[0],
                Signal::Dislike,
                1_000,
            )],
            1_000,
        );
        let config = DjConfig::default();
        let candidates: Vec<_> = (0..expanded.len()).collect();
        let composers = composer_counts(&expanded, &candidates);
        let recency = Recency::scan(&expanded, &[], &config);
        let scoring = Scoring {
            pool: &expanded,
            recency: &recency,
            aim: None,
            config: &config,
            feedback: Some(&feedback),
            composers: &composers,
        };
        let scores: Vec<_> = candidates
            .iter()
            .map(|&index| scoring.weight(index, None))
            .collect();
        let preferred_score = SCALE * if pinned { 3 } else { 2 };
        assert_eq!(scores[0], preferred_score, "preference shields feedback");
        assert!(scores[2..].iter().all(|&score| score < SCALE));
        let lottery = RecordingLottery::new(&expanded, &candidates, &scores);
        assert_eq!(lottery.groups[0].count, 100);
        assert_eq!(lottery.groups[0].preferred_count, 1);
        assert_eq!(lottery.groups[0].weight(), preferred_score);
        let context = PickContext {
            feedback: Some(&feedback),
            ..PickContext::default()
        };
        let mut heard_neutral = false;
        for seed in 0..512 {
            let expected = pick_next(&base, &context, &config, &mut Rng::new(seed)).unwrap();
            let actual = pick_next(&expanded, &context, &config, &mut Rng::new(seed)).unwrap();
            assert_eq!(actual.work.work_key, expected.work.work_key, "seed {seed}");
            heard_neutral |= actual.work.album_key.starts_with("spotify:album:neutral");
        }
        assert!(
            heard_neutral,
            "manual preferences remain soft for recordings"
        );
    }
}

#[test]
fn duplicate_energy_evidence_does_not_bias_unknown_music() {
    let mut tracks = recording("calm", 1, "Synthetic Calm Artist", false);
    tracks.extend(recording("bright", 2, "Synthetic Bright Artist", false));
    tracks.extend(recording("unknown", 3, "Synthetic Unknown Artist", false));
    let mut works = works_of(&tracks).works().to_vec();
    for (work, energy) in works.iter_mut().zip([Some(20), Some(80), None]) {
        for movement in &mut work.movements {
            movement.energy = energy.unwrap_or(50);
            movement.energy_basis = energy.map_or(EnergyBasis::Unknown, |_| EnergyBasis::Genre);
        }
    }
    let base = WorkPool::from_works(works.clone());
    for number in 0..30 {
        let mut edition = works[0].clone();
        edition.album_key = format!("spotify:album:calm{number}");
        for movement in &mut edition.movements {
            movement.track.source_uri = format!("spotify:track:calm{number}");
            movement.album_key.clone_from(&edition.album_key);
        }
        works.push(edition);
    }
    let expanded = WorkPool::from_works(works);
    for energy in [20, 50, 80] {
        let expected = Aim::over(&base, &[0, 1, 2], energy);
        let actual = Aim::over(&expanded, &(0..expanded.len()).collect::<Vec<_>>(), energy);
        assert_eq!(actual.unknown_pm, expected.unknown_pm);
        let context = PickContext {
            energy_target: Some(energy),
            ..PickContext::default()
        };
        for seed in 0..64 {
            let expected =
                pick_next(&base, &context, &DjConfig::default(), &mut Rng::new(seed)).unwrap();
            let actual = pick_next(
                &expanded,
                &context,
                &DjConfig::default(),
                &mut Rng::new(seed),
            )
            .unwrap();
            assert_eq!(actual.work.work_key, expected.work.work_key);
        }
    }
}

#[test]
fn means_keep_positive_scores_reachable_without_sum_overflow() {
    assert_eq!(
        RecordingGroup {
            first: 0,
            count: 2,
            preference_pm: 1000,
            preferred_count: 2,
            total: u128::from(u64::MAX) * 2
        }
        .weight(),
        u64::MAX
    );
    assert_eq!(
        RecordingGroup {
            first: 0,
            count: 10_000,
            preference_pm: 1000,
            preferred_count: 10_000,
            total: 1
        }
        .weight(),
        1
    );
    assert_eq!(
        RecordingGroup {
            first: 0,
            count: 10_000,
            preference_pm: 1000,
            preferred_count: 10_000,
            total: 0
        }
        .weight(),
        0
    );
}
