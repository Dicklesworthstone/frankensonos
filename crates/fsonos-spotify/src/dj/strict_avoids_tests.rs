use super::*;
use crate::library::LibraryItem;
use crate::prefs::{Items, KEYS, PREFERENCES_FILE, Taste};
use crate::steer::DjConstraints;
use crate::test_shelf::{MIDNIGHT, simulate_with, song_items, works_of};

fn songs() -> Vec<LibraryItem> {
    let library = song_items();
    ["Paper Moons", "Blue Hours", "Skyline"]
        .into_iter()
        .map(|title| {
            library
                .iter()
                .find(|item| item.title == title)
                .unwrap()
                .clone()
        })
        .collect()
}

fn avoiding(items: &[LibraryItem], strict_avoids: bool) -> Preferences {
    Preferences {
        strict_avoids,
        avoid: Taste {
            artists: items.iter().map(|item| item.artists[0].clone()).collect(),
            ..Taste::default()
        },
        ..Preferences::default()
    }
}

#[test]
fn strict_avoids_are_opt_in_and_persist_through_scalar_edits_and_unset() {
    let moods = Moods::builtin();
    let legacy = Preferences::parse(
        "energy = 27\n[avoid]\ngenres = [\"pop\"]\n",
        "legacy-preferences.toml",
        &moods,
    )
    .unwrap();
    assert!(!legacy.strict_avoids);
    assert!(KEYS.contains(&"strict_avoids"));
    let mut enabled = legacy.clone();
    enabled.add("strict_avoids", " true ", &moods).unwrap();
    assert!(enabled.strict_avoids);
    assert_eq!(enabled.energy, legacy.energy);
    assert_eq!(enabled.avoid, legacy.avoid);
    for invalid in ["", "yes", "1", "TRUE"] {
        assert!(enabled.add("strict_avoids", invalid, &moods).is_err());
        assert!(
            enabled.strict_avoids,
            "a rejected value must not disable strict avoids"
        );
    }
    assert_eq!(
        Preferences::parse(&enabled.to_toml().unwrap(), "roundtrip", &moods).unwrap(),
        enabled
    );
    for invalid in ["strict_avoids = 1", "strict_avoids = \"true\""] {
        assert!(Preferences::parse(invalid, "invalid-preferences.toml", &moods).is_err());
    }

    let dir = crate::fake_spotify::scratch_dir("strict-avoids");
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join(PREFERENCES_FILE);
    enabled.save(&path).unwrap();
    assert_eq!(Preferences::load(&path, &moods).unwrap(), enabled);
    let (disabled, changed) =
        Preferences::edit(&path, &moods, |prefs| prefs.remove("strict_avoids", None)).unwrap();
    assert!(changed);
    assert_eq!(disabled, legacy);
    assert_eq!(Preferences::load(&path, &moods).unwrap(), legacy);
    let bytes = std::fs::read(&path).unwrap();
    let (_, changed) =
        Preferences::edit(&path, &moods, |prefs| prefs.remove("strict_avoids", None)).unwrap();
    assert!(!changed);
    assert_eq!(std::fs::read(&path).unwrap(), bytes);
    enabled.add("strict_avoids", "false", &moods).unwrap();
    assert_eq!(enabled, legacy);
    std::fs::remove_dir_all(dir).unwrap();
}

#[test]
fn every_standing_avoid_family_can_exhaust_without_relaxing() {
    let item = songs().remove(0);
    let moods = Moods::parse(
        "[moods.owner-avoided]\ninclude_artists = [\"Juniper Vale\"]\n",
        "strict-avoids-test",
    )
    .unwrap();
    for avoid in [
        Taste {
            genres: vec!["pop".into()],
            ..Taste::default()
        },
        Taste {
            artists: item.artists.clone(),
            ..Taste::default()
        },
        Taste {
            eras: vec!["2010s".into()],
            ..Taste::default()
        },
        Taste {
            moods: vec!["owner-avoided".into()],
            ..Taste::default()
        },
    ] {
        for strict_avoids in [false, true] {
            let pool = works_of(std::slice::from_ref(&item)).with_preferences(
                Preferences {
                    strict_avoids,
                    avoid: avoid.clone(),
                    ..Preferences::default()
                },
                &moods,
            );
            assert!(pool.verdict(0).avoids(), "{avoid:?}");
            let mut rng = Rng::new(31);
            let before = format!("{rng:?}");
            let picked = pick_next(
                &pool,
                &PickContext::default(),
                &DjConfig::default(),
                &mut rng,
            );
            if strict_avoids {
                assert!(picked.is_none(), "{avoid:?}");
                assert_eq!(
                    format!("{rng:?}"),
                    before,
                    "no selection consumes no randomness"
                );
                assert_eq!(
                    pool.why_nothing_plays(None, None).as_deref(),
                    Some(
                        "your preferences keep all 1 works out: 1 avoided while strict avoids are on"
                    )
                );
            } else {
                assert!(picked.unwrap().reason.has(Factor::Avoided));
                assert_eq!(pool.why_nothing_plays(None, None), None);
            }
        }
    }
}

#[test]
fn a_partially_allowed_pool_keeps_the_same_seeded_rotation() {
    let items = songs();
    let prefs = avoiding(&items[..1], false);
    let adaptive = works_of(&items).with_preferences(prefs.clone(), &Moods::builtin());
    let strict = works_of(&items).with_preferences(
        Preferences {
            strict_avoids: true,
            ..prefs
        },
        &Moods::builtin(),
    );
    let config = DjConfig::default();
    let adaptive_picks = simulate_with(&adaptive, &config, 17, 30, Some(20), None, None);
    let strict_picks = simulate_with(&strict, &config, 17, 30, Some(20), None, None);
    assert_eq!(strict_picks, adaptive_picks);
    assert!(strict_picks.iter().all(|pick| {
        pick.movements[0].track.source_uri != items[0].source_uri
            && !pick.reason.has(Factor::Avoided)
    }));
    assert_eq!(strict.why_nothing_plays(None, None), None);
}

#[test]
fn pins_and_explicit_named_steering_keep_their_precedence_over_strict_avoids() {
    let items = songs();
    let prefs = avoiding(&items, true);
    let pool = works_of(&items).with_preferences(prefs.clone(), &Moods::builtin());
    let config = DjConfig::default();
    let pinned = works_of(&items).with_preferences(
        Preferences {
            pin: Items {
                tracks: vec![items[0].source_uri.clone()],
                ..Items::default()
            },
            ..prefs
        },
        &Moods::builtin(),
    );
    let pick = pick_next(&pinned, &PickContext::default(), &config, &mut Rng::new(7)).unwrap();
    assert_eq!(pick.movements[0].track.source_uri, items[0].source_uri);
    assert!(!pick.reason.has(Factor::Avoided));
    assert_eq!(pinned.why_nothing_plays(None, None), None);

    for constraints in [
        DjConstraints {
            include_artists: items[1].artists.clone(),
            ..DjConstraints::default()
        },
        DjConstraints {
            include_composers: items[1].artists.clone(),
            ..DjConstraints::default()
        },
        DjConstraints {
            include_genres: vec!["jazz".into()],
            ..DjConstraints::default()
        },
    ] {
        let steer = Steer {
            mood: None,
            constraints: DjConstraints {
                strict: true,
                expires_at: Some(MIDNIGHT + 60),
                ..constraints
            },
        };
        let context = PickContext {
            now: Some(MIDNIGHT),
            steer: Some(&steer),
            ..PickContext::default()
        };
        let pick = pick_next(&pool, &context, &config, &mut Rng::new(7)).unwrap();
        assert_eq!(pick.movements[0].track.source_uri, items[1].source_uri);
        assert!(!pick.reason.has(Factor::Avoided));
        assert_eq!(pool.why_nothing_plays(Some(&steer), context.now), None);
        let expired = PickContext {
            now: Some(MIDNIGHT + 60),
            ..context
        };
        assert!(pick_next(&pool, &expired, &config, &mut Rng::new(7)).is_none());
        assert_eq!(
            pool.why_nothing_plays(Some(&steer), expired.now).as_deref(),
            Some("your preferences keep all 3 works out: 3 avoided while strict avoids are on")
        );
    }
}

#[test]
fn unavailable_permitted_tracks_do_not_force_strict_avoids_to_relax() {
    let items = songs();
    let items = &items[..2];
    let unavailable = [UnavailableTrack {
        source_uri: items[1].source_uri.clone(),
        retry_at: MIDNIGHT + 60,
    }];
    let context = PickContext {
        unavailable: &unavailable,
        now: Some(MIDNIGHT),
        ..PickContext::default()
    };
    let config = DjConfig::default();
    for strict_avoids in [false, true] {
        let pool = works_of(items)
            .with_preferences(avoiding(&items[..1], strict_avoids), &Moods::builtin());
        let picked = pick_next(&pool, &context, &config, &mut Rng::new(43));
        if strict_avoids {
            assert!(picked.is_none());
        } else {
            let picked = picked.unwrap();
            assert_eq!(picked.movements[0].track.source_uri, items[0].source_uri);
            assert!(picked.reason.has(Factor::Avoided));
        }
        let expired = PickContext {
            now: Some(MIDNIGHT + 60),
            ..context
        };
        let picked = pick_next(&pool, &expired, &config, &mut Rng::new(43)).unwrap();
        assert_eq!(picked.movements[0].track.source_uri, items[1].source_uri);
        assert!(!picked.reason.has(Factor::Avoided));
    }

    let all_unavailable: Vec<_> = items
        .iter()
        .map(|item| UnavailableTrack {
            source_uri: item.source_uri.clone(),
            retry_at: MIDNIGHT + 60,
        })
        .collect();
    let named = Steer {
        mood: None,
        constraints: DjConstraints {
            include_artists: items[0].artists.clone(),
            ..DjConstraints::default()
        },
    };
    let pool = works_of(items).with_preferences(avoiding(items, true), &Moods::builtin());
    assert!(
        pick_next(
            &pool,
            &PickContext {
                unavailable: &all_unavailable,
                steer: Some(&named),
                ..context
            },
            &config,
            &mut Rng::new(43),
        )
        .is_none()
    );
}

#[test]
fn exhausted_preferences_count_bans_explicit_tracks_and_strict_avoids_once() {
    let mut items = songs();
    items[0].explicit = true;
    items[1].explicit = true;
    let prefs = Preferences {
        ban: Items {
            tracks: vec![items[0].source_uri.clone()],
            ..Items::default()
        },
        ..avoiding(&items, true)
    };
    let pool = works_of(&items).with_preferences(prefs.clone(), &Moods::builtin());
    assert_eq!(
        pool.why_nothing_plays(None, None).as_deref(),
        Some(
            "your preferences keep all 3 works out: 1 banned, 1 explicit while explicit tracks are off, 1 avoided while strict avoids are on"
        )
    );
    let explicit_allowed = works_of(&items).with_preferences(
        Preferences {
            explicit: true,
            ..prefs.clone()
        },
        &Moods::builtin(),
    );
    assert_eq!(
        explicit_allowed.why_nothing_plays(None, None).as_deref(),
        Some(
            "your preferences keep all 3 works out: 1 banned, 2 avoided while strict avoids are on"
        )
    );
    let legacy = works_of(&items[..2]).with_preferences(
        Preferences {
            strict_avoids: false,
            ..prefs
        },
        &Moods::builtin(),
    );
    assert_eq!(
        legacy.why_nothing_plays(None, None).as_deref(),
        Some(
            "your preferences keep all 2 works out: 1 banned, 1 explicit while explicit tracks are off"
        )
    );
}
