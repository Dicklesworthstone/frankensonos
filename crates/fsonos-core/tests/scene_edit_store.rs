//! A reviewed scene change consumes one exact observation, including across
//! database connections. Case-only renames and removals cannot lose other edits.

use fsonos_core::scenes::{self, Scene, SceneError, SceneGroup, SceneSource};
use fsonos_core::store::{MemStore, SqliteStore, Store, StoredScene};
use std::collections::BTreeMap;
use std::sync::{Arc, Barrier};

fn row(name: &str, version: u8, updated: i64) -> StoredScene {
    StoredScene {
        name: name.into(),
        spec: format!(r#"{{"version":{version}}}"#),
        updated,
    }
}

fn lifecycle(store: &mut dyn Store) {
    let first = row("Evening", 1, 100);
    assert!(
        store
            .compare_and_swap_scene(" evening ", None, None)
            .unwrap()
    );
    assert!(
        store
            .compare_and_swap_scene(" evening ", None, Some(&first))
            .unwrap()
    );
    assert_eq!(
        scenes::observe(store, "EVENING").unwrap(),
        Some(first.clone())
    );
    assert!(
        !store
            .compare_and_swap_scene("evening", None, Some(&first))
            .unwrap()
    );
    assert!(
        store
            .compare_and_swap_scene("evening", Some(&first), Some(&first))
            .unwrap()
    );

    let revised = row("Evening", 2, first.updated);
    assert!(
        store
            .compare_and_swap_scene("evening", Some(&first), Some(&revised))
            .unwrap()
    );
    assert!(
        !store
            .compare_and_swap_scene("evening", Some(&first), Some(&first))
            .unwrap(),
        "a same-second edit invalidates the old observation"
    );
    assert!(
        !store
            .compare_and_swap_scene("evening", Some(&first), None)
            .unwrap()
    );
    assert_eq!(store.scene("Evening").unwrap(), Some(revised.clone()));

    let later = StoredScene {
        updated: revised.updated + 1,
        ..revised.clone()
    };
    store.save_scene(&later).unwrap();
    assert!(
        !store
            .compare_and_swap_scene("evening", Some(&revised), None)
            .unwrap(),
        "the timestamp is part of the exact observation"
    );

    let renamed = StoredScene {
        name: "EVENING".into(),
        ..later.clone()
    };
    assert!(
        store
            .compare_and_swap_scene("evening", Some(&later), Some(&renamed))
            .unwrap()
    );
    assert!(store.scene("Evening").unwrap().is_none());
    assert_eq!(store.scenes().unwrap(), [renamed.clone()]);
    assert!(
        !store
            .compare_and_swap_scene("evening", Some(&later), Some(&later))
            .unwrap(),
        "stored spelling is part of the exact observation"
    );

    let before = store.scenes().unwrap();
    let unrelated = row("Morning", 9, 200);
    assert!(
        store
            .compare_and_swap_scene("evening", Some(&renamed), Some(&unrelated))
            .is_err()
    );
    assert!(
        store
            .compare_and_swap_scene("morning", Some(&renamed), Some(&unrelated))
            .is_err()
    );
    assert!(store.compare_and_swap_scene(" ", None, None).is_err());
    assert_eq!(store.scenes().unwrap(), before);

    assert!(
        store
            .compare_and_swap_scene("evening", Some(&renamed), None)
            .unwrap()
    );
    assert!(
        !store
            .compare_and_swap_scene("evening", Some(&renamed), None)
            .unwrap()
    );
    assert!(
        !store
            .compare_and_swap_scene("evening", Some(&renamed), Some(&renamed))
            .unwrap(),
        "a stale edit cannot resurrect a deleted scene"
    );
    assert!(store.scenes().unwrap().is_empty());
    assert!(scenes::observe(store, "evening").unwrap().is_none());
}

fn ambiguous_names(store: &mut dyn Store) {
    let first = row("Evening", 1, 100);
    let duplicate = row("EVENING", 2, 101);
    // Older raw stores could hold both spellings. Neither is safe to pick.
    store.save_scene(&first).unwrap();
    store.save_scene(&duplicate).unwrap();
    let before = store.scenes().unwrap();
    for name in ["Evening", "EVENING", " evening "] {
        assert!(matches!(
            scenes::observe(store, name),
            Err(SceneError::AmbiguousName { .. })
        ));
        for expected in [None, Some(&first), Some(&duplicate)] {
            assert!(
                !store
                    .compare_and_swap_scene(name, expected, Some(&first))
                    .unwrap()
            );
            assert!(!store.compare_and_swap_scene(name, expected, None).unwrap());
            assert_eq!(store.scenes().unwrap(), before);
        }
    }

    let separate = row("Café", 3, 200);
    assert!(
        store
            .compare_and_swap_scene("café", None, Some(&separate))
            .unwrap(),
        "an unrelated ambiguous name does not block this scene"
    );
    assert_eq!(
        scenes::observe(store, "café").unwrap(),
        Some(separate.clone())
    );
    assert!(
        scenes::observe(store, "CAFÉ").unwrap().is_none(),
        "ASCII folding must agree with the mutation boundary"
    );
    assert!(
        store
            .compare_and_swap_scene(" café ", Some(&separate), None)
            .unwrap()
    );
    assert_eq!(store.scenes().unwrap(), before);
}

#[test]
fn memory_scene_changes_bind_the_complete_observation() {
    lifecycle(&mut MemStore::default());
    ambiguous_names(&mut MemStore::default());
}

#[test]
fn sqlite_scene_changes_bind_the_complete_observation() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    lifecycle(&mut store);
    ambiguous_names(&mut store);
    store.close().unwrap();
}

fn race(
    observed: Option<StoredScene>,
    first_change: Option<StoredScene>,
    second_change: Option<StoredScene>,
) {
    let directory = tempfile::tempdir().unwrap();
    let path = directory.path().join("scenes.db");
    let mut first = SqliteStore::open(&path).unwrap();
    if let Some(observed) = &observed {
        first.save_scene(observed).unwrap();
    }
    let mut second = SqliteStore::open(&path).unwrap();
    assert_eq!(scenes::observe(&second, "evening").unwrap(), observed);
    let ready = Arc::new(Barrier::new(2));
    let other_ready = Arc::clone(&ready);
    let other_observed = observed.clone();
    let replacement = first_change.clone();
    let editing = std::thread::spawn(move || {
        other_ready.wait();
        let changed = first
            .compare_and_swap_scene("evening", other_observed.as_ref(), replacement.as_ref())
            .unwrap();
        first.close().unwrap();
        changed
    });
    ready.wait();
    let second_won = second
        .compare_and_swap_scene("EVENING", observed.as_ref(), second_change.as_ref())
        .unwrap();
    let first_won = editing.join().unwrap();
    assert_ne!(first_won, second_won, "only one reviewed change can win");
    let expected = if first_won {
        first_change
    } else {
        second_change
    };
    assert_eq!(scenes::observe(&second, "Evening").unwrap(), expected);
    assert_eq!(
        second.scenes().unwrap(),
        expected.clone().into_iter().collect::<Vec<_>>()
    );
    if let Some(observed) = &observed {
        assert!(
            !second
                .compare_and_swap_scene("evening", Some(observed), Some(observed))
                .unwrap(),
            "the losing observation cannot overwrite the winner"
        );
    }
    second.close().unwrap();
    let reopened = SqliteStore::open(&path).unwrap();
    assert_eq!(scenes::observe(&reopened, "evening").unwrap(), expected);
    reopened.close().unwrap();
}

#[test]
fn concurrent_creates_cannot_publish_two_case_variants() {
    race(
        None,
        Some(row("Evening", 1, 100)),
        Some(row("EVENING", 2, 100)),
    );
}

#[test]
fn concurrent_same_second_edits_cannot_overwrite_one_another() {
    race(
        Some(row("Evening", 1, 100)),
        Some(row("EVENING", 2, 100)),
        Some(row("eVeNiNg", 3, 100)),
    );
}

#[test]
fn a_delete_and_a_case_only_rename_cannot_both_consume_the_same_scene() {
    race(
        Some(row("Evening", 1, 100)),
        Some(row("EVENING", 1, 100)),
        None,
    );
}

#[test]
fn validated_rows_decode_without_rereading_or_changing_the_observation() {
    let scene = Scene {
        name: "Evening".into(),
        groups: vec![SceneGroup {
            coordinator: "Den".into(),
            members: Vec::new(),
            source: SceneSource::Keep,
            playing: None,
        }],
        volumes: BTreeMap::from([("Den".into(), 20)]),
        mutes: BTreeMap::new(),
        writer: Some("tag:scene-editor".into()),
    };
    let observed = scenes::to_stored(&scene, 100).unwrap();
    let mut store = MemStore::default();
    store
        .compare_and_swap_scene("evening", None, Some(&observed))
        .unwrap();
    assert_eq!(scenes::from_stored(&observed).unwrap(), scene);
    let retained = scenes::observe(&store, " evening ").unwrap().unwrap();
    store.save_scene(&row("Evening", 9, 101)).unwrap();
    assert_eq!(retained, observed);
    assert_eq!(scenes::from_stored(&retained).unwrap(), scene);
    let corrupt = StoredScene {
        spec: "{".into(),
        ..observed
    };
    assert!(matches!(
        scenes::from_stored(&corrupt),
        Err(SceneError::Corrupt { .. })
    ));
}
