use super::*;
use crate::dj::{DjConfig, PickContext, Rng, WorkPool, pick_next};
use crate::prefs::{Items, Preferences, Taste};
use crate::test_shelf::{MIDNIGHT, mixed_items, simulate_steered, song_items, works_of};

fn strict(mut constraints: DjConstraints) -> DjConstraints {
    constraints.strict = true;
    constraints
}

#[test]
fn strict_is_opt_in_and_survives_mood_session_and_saved_json_merges() {
    let old: DjConstraints = serde_json::from_str(r#"{"include_genres":["jazz"]}"#).unwrap();
    assert!(!old.strict);
    assert!(serde_json::to_value(&old).unwrap().get("strict").is_none());
    let rules = strict(old.clone());
    assert!(rules.merged(&DjConstraints::default()).strict);
    assert!(DjConstraints::default().merged(&rules).strict);
    assert!(!old.merged(&DjConstraints::default()).strict);

    let moods = Moods::parse(
        "[moods.strict-jazz]\ninclude_genres = [\"jazz\"]\nstrict = true\n",
        "strict-test",
    )
    .unwrap();
    for (mood, constraints) in [
        (Some("strict-jazz".into()), DjConstraints::default()),
        (Some("dinner".into()), rules),
    ] {
        let session = DjSession {
            zone: "synthetic-strict-zone".into(),
            mood,
            constraints,
            expires_at: Some(MIDNIGHT + 600),
        };
        let restored = DjSession::from_store(&session.to_store().unwrap()).unwrap();
        assert_eq!(restored, session);
        assert!(
            restored
                .steer(&moods, Some(MIDNIGHT))
                .unwrap()
                .unwrap()
                .constraints
                .strict
        );
        assert!(
            restored
                .steer(&moods, Some(MIDNIGHT + 600))
                .unwrap()
                .is_none()
        );
    }
}

#[test]
fn every_filter_family_can_exhaust_without_relaxing() {
    let pool = works_of(&mixed_items());
    let works = pool.works();
    let hays: Vec<String> = works.iter().map(haystack).collect();
    let cases = [
        DjConstraints {
            include_keywords: vec!["absent synthetic keyword".into()],
            ..DjConstraints::default()
        },
        DjConstraints {
            periods: vec![Period::Baroque],
            exclude_periods: vec![Period::Baroque],
            ..DjConstraints::default()
        },
        DjConstraints {
            decades: vec![1230],
            ..DjConstraints::default()
        },
        DjConstraints {
            min_work_minutes: Some(u32::MAX),
            ..DjConstraints::default()
        },
        DjConstraints {
            include_genres: vec!["absent synthetic genre".into()],
            ..DjConstraints::default()
        },
        DjConstraints {
            include_composers: vec!["Absent Synthetic Composer".into()],
            ..DjConstraints::default()
        },
        DjConstraints {
            include_artists: vec!["Absent Synthetic Artist".into()],
            ..DjConstraints::default()
        },
    ];
    for adaptive in cases {
        let (allowed, relaxed) = admit(works, &hays, &adaptive, 5);
        assert!(!allowed.is_empty() && !relaxed.is_empty(), "{adaptive:?}");
        let constraints = strict(adaptive);
        assert_eq!(admit(works, &hays, &constraints, 5), (vec![], vec![]));
        let steer = Steer {
            mood: None,
            constraints,
        };
        let context = PickContext {
            steer: Some(&steer),
            now: Some(MIDNIGHT),
            ..PickContext::default()
        };
        assert!(pick_next(&pool, &context, &DjConfig::default(), &mut Rng::new(9)).is_none());
        let why = pool
            .why_nothing_plays(Some(&steer), Some(MIDNIGHT))
            .unwrap();
        assert!(why.contains("no music matches strict steering"), "{why}");
    }
}

#[test]
fn strict_exclusions_never_let_an_entirely_excluded_pool_back_in() {
    let pool = works_of(&mixed_items());
    let song = pool
        .works()
        .iter()
        .find(|work| !work.is_classical() && !work.genres().is_empty())
        .unwrap();
    let classical = pool
        .works()
        .iter()
        .find(|work| work.is_classical())
        .unwrap();
    let cases = [
        (
            song,
            DjConstraints {
                exclude_keywords: vec![song.title.clone()],
                ..DjConstraints::default()
            },
        ),
        (
            song,
            DjConstraints {
                exclude_artists: credited(song),
                ..DjConstraints::default()
            },
        ),
        (
            song,
            DjConstraints {
                exclude_genres: song
                    .genres()
                    .iter()
                    .map(|genre| (*genre).to_owned())
                    .collect(),
                ..DjConstraints::default()
            },
        ),
        (
            song,
            DjConstraints {
                exclude_decades: vec![song.year().unwrap() / 10 * 10],
                ..DjConstraints::default()
            },
        ),
        (
            classical,
            DjConstraints {
                exclude_composers: vec![classical.composer.clone()],
                ..DjConstraints::default()
            },
        ),
        (
            classical,
            DjConstraints {
                exclude_periods: vec![classical.period],
                ..DjConstraints::default()
            },
        ),
    ];
    for (work, adaptive) in cases {
        let works = std::slice::from_ref(work);
        let hays = [haystack(work)];
        assert_eq!(admit(works, &hays, &adaptive, 5).0, [0], "legacy fallback");
        assert_eq!(admit(works, &hays, &strict(adaptive), 5), (vec![], vec![]));
    }
}

#[test]
fn a_small_matching_set_rotates_without_relaxing_or_breaking_work_boundaries() {
    let pool = works_of(&mixed_items());
    let steer = Steer {
        mood: None,
        constraints: strict(DjConstraints {
            decades: vec![2020],
            ..DjConstraints::default()
        }),
    };
    let hays: Vec<String> = pool.works().iter().map(haystack).collect();
    let (allowed, relaxed) = admit(pool.works(), &hays, &steer.constraints, 5);
    assert!(!allowed.is_empty() && allowed.len() < 5);
    assert!(relaxed.is_empty());
    let picks = simulate_steered(&pool, &DjConfig::default(), 7, 24, Some(12), Some(&steer));
    assert_eq!(picks.len(), 24);
    for pick in picks {
        assert!(
            pick.work
                .year()
                .is_some_and(|year| (2020..2030).contains(&year))
        );
        assert!(pick.reason.relaxed.is_empty());
        assert_eq!(pick.movements.len(), pick.work.movements.len());
    }
}

#[test]
fn strict_exhaustion_expires_and_keeps_preference_failures_distinct() {
    let item = song_items().remove(0);
    let pool = works_of(std::slice::from_ref(&item));
    let steer = Steer {
        mood: None,
        constraints: strict(DjConstraints {
            include_genres: vec!["absent synthetic genre".into()],
            expires_at: Some(MIDNIGHT + 1),
            ..DjConstraints::default()
        }),
    };
    let mut context = PickContext {
        steer: Some(&steer),
        now: Some(MIDNIGHT),
        ..PickContext::default()
    };
    let mut rng = Rng::new(11);
    let before = format!("{rng:?}");
    assert!(pick_next(&pool, &context, &DjConfig::default(), &mut rng).is_none());
    assert_eq!(
        format!("{rng:?}"),
        before,
        "an empty selection consumes no randomness"
    );
    context.now = Some(MIDNIGHT + 1);
    assert!(pick_next(&pool, &context, &DjConfig::default(), &mut rng).is_some());
    assert_eq!(pool.why_nothing_plays(Some(&steer), context.now), None);

    let only_strict = Steer {
        mood: None,
        constraints: strict(DjConstraints::default()),
    };
    let context = PickContext {
        steer: Some(&only_strict),
        now: Some(MIDNIGHT),
        ..PickContext::default()
    };
    let avoided = works_of(std::slice::from_ref(&item)).with_preferences(
        Preferences {
            avoid: Taste {
                artists: item.artists.clone(),
                ..Taste::default()
            },
            ..Preferences::default()
        },
        &Moods::builtin(),
    );
    assert!(
        pick_next(&avoided, &context, &DjConfig::default(), &mut rng).is_some(),
        "strict steering does not change standing avoids"
    );
    let banned = WorkPool::from_works(pool.works().to_vec()).with_preferences(
        Preferences {
            ban: Items {
                tracks: vec![item.source_uri],
                ..Items::default()
            },
            ..Preferences::default()
        },
        &Moods::builtin(),
    );
    assert!(pick_next(&banned, &context, &DjConfig::default(), &mut rng).is_none());
    let why = banned
        .why_nothing_plays(Some(&only_strict), Some(MIDNIGHT))
        .unwrap();
    assert!(why.contains("banned") && !why.contains("strict"), "{why}");
}

#[test]
fn expired_unreadable_constraints_return_to_the_program_before_payload_decoding() {
    use fsonos_core::store::MemStore;

    let mut store = MemStore::default();
    let zone = "synthetic-expired-strict-zone";
    store
        .save_dj_session(&StoredDjSession {
            coordinator: zone.into(),
            mood: Some("deleted-synthetic-mood".into()),
            constraints: Some("{".into()),
            expires: MIDNIGHT + 60,
        })
        .unwrap();
    let moods = Moods::builtin();
    let local = Some((Weekday::Mon, NaiveTime::from_hms_opt(8, 0, 0).unwrap()));
    assert!(steering(&store, &moods, zone, MIDNIGHT, local).is_err());
    let resumed = steering(&store, &moods, zone, MIDNIGHT + 60, local)
        .unwrap()
        .unwrap();
    assert_eq!(resumed.mood.as_deref(), Some("bright"));
    assert!(!resumed.constraints.strict);
}
