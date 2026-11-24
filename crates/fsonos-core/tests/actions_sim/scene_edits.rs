//! Scene restoration retains its admitted observation across speaker I/O.

use super::*;
use fsonos_core::store::StoredScene;
use std::cell::{Cell, RefCell};

fn scene(name: &str, version: u8, updated: i64) -> StoredScene {
    StoredScene {
        name: name.into(),
        spec: format!(r#"{{"version":{version}}}"#),
        updated,
    }
}

#[test]
fn scene_undo_never_adopts_an_edit_arriving_during_speaker_restoration() {
    for force in [false, true] {
        for concurrent_edit in [false, true] {
            let sim = sim();
            let lan = sim.lan();
            let houses = houses(&sim, &lan);
            let kitchen = id(&houses, "Kitchen");
            control::set_volume(&lan, &houses, &kitchen, 20).unwrap();
            control::play_uri(&lan, &houses, &kitchen, STREAM, "").unwrap();
            let (snaps, missed) = capture_zones(&lan, &houses, std::slice::from_ref(&kitchen), 100);
            assert!(missed.is_empty(), "{missed:?}");
            control::set_volume(&lan, &houses, &kitchen, 45).unwrap();
            control::play_uri(&lan, &houses, &kitchen, OTHER, "").unwrap();

            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("scene-undo.db");
            let mut store = SqliteStore::open(&path).unwrap();
            let was = scene("evening", 1, 100);
            let left = scene("Evening", 2, 200);
            store.save_scene(&left).unwrap();
            let action = log(
                &mut store,
                "save scene and change room",
                Some(
                    serde_json::json!({
                        "zones": snaps,
                        "scenes": [{
                            "was": actions::SceneRow::from(was.clone()),
                            "left": actions::SceneRow::from(left.clone())
                        }]
                    })
                    .to_string(),
                ),
                200,
            );
            if force {
                // Force may accept this change, which predates admission.
                store.save_scene(&scene("Evening", 3, 201)).unwrap();
            }
            let other = RefCell::new(SqliteStore::open(&path).unwrap());
            let intervened = Cell::new(false);
            let newest = scene("Evening", 4, 202);
            let report = actions::undo_last_vetted(
                &lan,
                &houses,
                &mut store,
                None,
                &Client::Cli,
                "cli",
                300,
                force,
                &|op| {
                    if concurrent_edit && !intervened.replace(true) {
                        other.borrow_mut().save_scene(&newest).unwrap();
                    }
                    Ok(op.clone())
                },
            )
            .unwrap()
            .unwrap();
            assert_eq!(report.undone, action);
            assert!(report.failures.is_empty(), "{report:?}");
            assert_eq!(control::volume(&lan, &houses, &kitchen).unwrap(), 20);
            assert_eq!(uri(&lan, &houses, &kitchen), STREAM);
            let expected = if concurrent_edit {
                assert!(
                    intervened.get(),
                    "the edit occurred during real restoration"
                );
                assert!(report.configured.is_empty(), "{report:?}");
                assert_eq!(report.config_failures.len(), 1, "{report:?}");
                assert!(
                    report.config_failures[0]
                        .1
                        .contains("changed after undo was prepared"),
                    "{report:?}"
                );
                newest
            } else {
                assert!(report.config_failures.is_empty(), "{report:?}");
                assert_eq!(report.configured, [r#"the scene "evening""#]);
                was
            };
            assert_eq!(store.scenes().unwrap(), [expected.clone()]);
            other.into_inner().close().unwrap();
            store.close().unwrap();
            let reopened = SqliteStore::open(&path).unwrap();
            assert_eq!(reopened.scenes().unwrap(), [expected]);
            reopened.close().unwrap();
            sim.shutdown();
        }
    }
}
