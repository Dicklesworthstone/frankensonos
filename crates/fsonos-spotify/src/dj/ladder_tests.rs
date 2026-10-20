//! sp4f.4: the precedence ladder holds for the soft rungs too. Over seeded
//! random libraries and signals, each rung outweighs every rung below it
//! together: a favor beats any feedback times a like times account taste,
//! one fresh dislike cancels a like and the most account taste, and a
//! steer's filters keep out what the preferences pin or favor.

use super::*;
use crate::feedback::{FeedbackModel, FeedbackSignal, Signal};
use crate::library::{LibraryItem, Origin};
use crate::prefs::{Items, Taste};
use crate::steer::{DjConstraints, Moods, Steer};
use crate::test_shelf::{MIDNIGHT, simulate_with, works_of};

/// One artist a song, each on an album of its own: every factor outside
/// the ladder (balance, spacing, period, genre, energy) is the same for
/// each song.
const ARTISTS: [&str; 12] = [
    "Ash Lane",
    "Birch Row",
    "Cedar Gate",
    "Dune Hall",
    "Elm Court",
    "Fern Way",
    "Glen Moor",
    "Hazel Cove",
    "Iris Bay",
    "Kestrel Ridge",
    "Larch Shore",
    "Myrtle Park",
];

const DAY: i64 = 86_400;

fn song(i: usize, origin: Origin, taste_pm: Option<u32>) -> LibraryItem {
    LibraryItem {
        source_uri: format!("spotify:track:ladder-{i}"),
        title: format!("Song {i}"),
        artists: vec![ARTISTS[i].into()],
        album: Some(format!("Album {i}")),
        album_uri: Some(format!("spotify:album:ladder-{i}")),
        album_artists: vec![ARTISTS[i].into()],
        disc_number: Some(1),
        track_number: Some(1),
        genres: vec!["indie pop".into()],
        release_year: Some(2001),
        duration_secs: Some(200),
        taste_pm,
        origin,
        ..LibraryItem::default()
    }
}

fn below(rng: &mut Rng, n: usize) -> usize {
    usize::try_from(rng.below_u64(u64::try_from(n).unwrap())).unwrap()
}

/// Any origin, liked or not.
fn any_origin(rng: &mut Rng) -> Origin {
    [
        Origin::SavedAlbum,
        Origin::LikedTrack,
        Origin::Both,
        Origin::Taste,
    ][below(rng, 4)]
}

/// No taste weight, or any up to well past what a sync writes (an older
/// sync's cache wrote up to ×1.8).
fn any_taste(rng: &mut Rng) -> Option<u32> {
    (below(rng, 4) != 0).then(|| 1000 + u32::try_from(below(rng, 1500)).unwrap())
}

/// A like boost from the default up to past the account rung.
fn any_config(rng: &mut Rng) -> DjConfig {
    DjConfig {
        liked_boost_pm: [DjConfig::default().liked_boost_pm, 1300, 2000][below(rng, 3)],
        ..DjConfig::default()
    }
}

/// Up to 40 signals of every kind about random songs, up to two months old.
fn any_feedback(rng: &mut Rng, pool: &WorkPool) -> FeedbackModel {
    let signals: Vec<FeedbackSignal> = (0..below(rng, 40))
        .map(|_| {
            let work = &pool.works()[below(rng, pool.len())];
            let signal = [
                Signal::Like,
                Signal::Dislike,
                Signal::EarlySkip,
                Signal::FullListen,
            ][below(rng, 4)];
            let age = i64::try_from(below(rng, 60)).unwrap() * DAY;
            FeedbackSignal::about(work, signal, MIDNIGHT - age)
        })
        .collect();
    FeedbackModel::from_signals(&signals, MIDNIGHT)
}

fn weight(pool: &WorkPool, w: usize, config: &DjConfig, feedback: Option<&FeedbackModel>) -> u64 {
    let recency = Recency::scan(pool, &[], config);
    weigh(pool, w, &recency, None, config, feedback, None)
}

/// A favored work outweighs any work the preferences say nothing about,
/// whatever feedback, likes and account taste either has.
#[test]
fn a_favor_outweighs_feedback_a_like_and_account_taste_together() {
    let moods = Moods::builtin();
    let mut compared = 0;
    for seed in 1..=300 {
        let mut rng = Rng::new(seed);
        let config = any_config(&mut rng);
        let items: Vec<LibraryItem> = (0..ARTISTS.len())
            .map(|i| song(i, any_origin(&mut rng), any_taste(&mut rng)))
            .collect();
        let favored: Vec<String> = ARTISTS
            .iter()
            .filter(|_| below(&mut rng, 3) == 0)
            .map(|&a| a.to_owned())
            .collect();
        let pool = works_of(&items).with_preferences(
            Preferences {
                favor: Taste {
                    artists: favored,
                    ..Taste::default()
                },
                ..Preferences::default()
            },
            &moods,
        );
        assert_eq!(pool.len(), ARTISTS.len(), "one work a song");
        let feedback = any_feedback(&mut rng, &pool);
        let of = |filter: fn(Verdict) -> bool| -> Vec<usize> {
            (0..pool.len())
                .filter(|&w| filter(pool.verdict(w)))
                .collect()
        };
        let plain = of(|v| v == Verdict::default());
        for a in of(|v| v.favored) {
            let wa = weight(&pool, a, &config, Some(&feedback));
            for &b in &plain {
                let wb = weight(&pool, b, &config, Some(&feedback));
                assert!(
                    wa >= wb,
                    "seed {seed}: favored {} ({wa}) under {} ({wb})",
                    pool.works()[a].title,
                    pool.works()[b].title
                );
                compared += 1;
            }
        }
    }
    assert!(compared > 1000, "only {compared} pairs");
}

/// One fresh dislike of a liked song cancels the like and the most account
/// taste: it weighs no more than the same song with none of the three.
#[test]
fn one_dislike_cancels_a_like_and_the_most_account_taste() {
    for seed in 1..=200 {
        let mut rng = Rng::new(seed);
        let config = any_config(&mut rng);
        let disliked = below(&mut rng, ARTISTS.len());
        let liked = [Origin::LikedTrack, Origin::Both][below(&mut rng, 2)];
        let taste = Some(1000 + u32::try_from(below(&mut rng, 1500)).unwrap());
        let mut items: Vec<LibraryItem> = (0..ARTISTS.len())
            .map(|i| song(i, Origin::SavedAlbum, None))
            .collect();
        let neutral = works_of(&items);
        items[disliked] = song(disliked, liked, taste);
        let signalled = works_of(&items);
        let title = format!("Song {disliked}");
        let at = |pool: &WorkPool| pool.works().iter().position(|w| w.title == title).unwrap();
        let (w, n) = (at(&signalled), at(&neutral));
        assert!(signalled.works()[w].is_liked());
        let dislike = FeedbackModel::from_signals(
            &[FeedbackSignal::about(
                &signalled.works()[w],
                Signal::Dislike,
                MIDNIGHT,
            )],
            MIDNIGHT,
        );
        let with = weight(&signalled, w, &config, Some(&dislike));
        let without = weight(&neutral, n, &config, None);
        assert!(
            with <= without,
            "seed {seed}: {title} liked ({liked:?}, taste {taste:?}) and disliked weighs {with}, \
             more than plain {without}"
        );
    }
}

/// A steer decides what may play: an artist it leaves out never plays, even
/// pinned or favored, and only the artists it asks for do.
#[test]
fn a_steer_keeps_out_what_the_preferences_pin_or_favor() {
    let moods = Moods::builtin();
    for seed in 1..=40 {
        let mut rng = Rng::new(seed);
        let items: Vec<LibraryItem> = (0..ARTISTS.len())
            .map(|i| song(i, any_origin(&mut rng), any_taste(&mut rng)))
            .collect();
        let mut order: Vec<&str> = ARTISTS.to_vec();
        for i in (1..order.len()).rev() {
            order.swap(i, below(&mut rng, i + 1));
        }
        let asked: Vec<String> = order[..2 + below(&mut rng, 3)]
            .iter()
            .map(|&a| a.to_owned())
            .collect();
        let left_out: Vec<String> = order[asked.len()..].iter().map(|&a| a.to_owned()).collect();
        let pool = works_of(&items).with_preferences(
            Preferences {
                pin: Items {
                    artists: left_out[..2].to_vec(),
                    ..Items::default()
                },
                favor: Taste {
                    artists: left_out[2..].to_vec(),
                    ..Taste::default()
                },
                ..Preferences::default()
            },
            &moods,
        );
        let feedback = any_feedback(&mut rng, &pool);
        let steer = Steer {
            mood: None,
            constraints: DjConstraints {
                include_artists: asked.clone(),
                exclude_artists: left_out[..1].to_vec(),
                ..DjConstraints::default()
            },
        };
        let picks = simulate_with(
            &pool,
            &DjConfig::default(),
            seed,
            12,
            Some(20),
            Some(&steer),
            Some(&feedback),
        );
        for pick in &picks {
            assert!(
                asked.contains(&pick.work.composer),
                "seed {seed}: {} by {} played, asked for {asked:?}",
                pick.work.title,
                pick.work.composer
            );
        }
    }
}
