//! Durable scene recalls retain their own native reconstruction proof. A
//! restored occurrence may differ from the first item without weakening the
//! older direct-play and container-scene action formats.

use super::*;
use fsonos_core::scenes::{QueueBackup, QueueRestoration, SceneSource};

fn backup(f: &Fixture) -> QueueBackup {
    let scene =
        scenes::capture_with_queue_backups(&f.lan, &f.houses, "Durable queue", &[], 90).unwrap();
    let SceneSource::QueueBackup { backup } = &scene.groups[0].source else {
        panic!("{scene:?}");
    };
    backup.clone()
}

fn backup_plan(f: &Fixture, start: bool, changed_contents: bool) -> ScenePlan {
    let mut saved = backup(f);
    if changed_contents {
        // Keep the complete real renderer metadata, but give this persisted
        // scene its own order and bookmark, independent of today's queue.
        saved.entries.rotate_right(1);
        saved.reference.track = 3;
        saved.reference.position_secs = 29;
        saved.reference.play_mode = Some(PlayMode::ShuffleNoRepeat);
    }
    let mut plan = super::plan(f, 0, start);
    *plan.ops.last_mut().unwrap() = SceneOp::QueueBackup {
        coordinator: f.coordinator.clone(),
        backup: saved,
        start,
    };
    plan
}

#[derive(Default)]
struct BackupReceipts {
    attempts: RefCell<Vec<PlayerId>>,
    receipts: RefCell<Vec<QueueRestoration>>,
}

impl SceneHooks for BackupReceipts {
    fn queue_replacement_started(&self, coordinator: &PlayerId) -> bool {
        self.attempts.borrow_mut().push(coordinator.clone());
        true
    }

    fn queue_backup_finished(&self, _: &PlayerId, receipt: QueueRestoration) {
        self.receipts.borrow_mut().push(receipt);
    }

    fn start_dj(&self, _: &PlayerId, _: Option<&str>) -> Result<(), String> {
        Err("no DJ in this durable queue fixture".into())
    }
}

fn recall<T: Transport + ?Sized>(
    f: &Fixture,
    transport: &T,
    plan: &ScenePlan,
) -> (QueueRestoration, SceneReport) {
    let hooks = BackupReceipts::default();
    let report = scenes::apply(
        transport,
        &f.houses,
        plan,
        &ApplyOptions {
            fade: None,
            hooks: &hooks,
        },
    );
    assert_eq!(*hooks.attempts.borrow(), [f.coordinator.clone()]);
    let mut receipts = hooks.receipts.take();
    assert_eq!(receipts.len(), 1, "{report:?}");
    (receipts.remove(0), report)
}

fn finish_backup(
    f: &Fixture,
    pending: PendingSceneQueueUndo,
    receipt: &QueueRestoration,
    report: &SceneReport,
) -> Result<QueueUndo, CoreError> {
    actions::finish_scene_queue_restoration(&f.lan, &f.houses, pending, receipt, report, 101)
}

#[test]
fn durable_scene_queue_undo_keeps_duplicate_occurrence_and_intent_after_sqlite_reopen() {
    for generation in [1, 2] {
        for start in [false, true] {
            let f = fixture(generation, !start);
            let plan = backup_plan(&f, start, true);
            let pending =
                actions::capture_scene_queue_undo(&f.lan, &f.houses, &f.coordinator, &plan, 100)
                    .unwrap();
            let (receipt, report) = recall(&f, &f.lan, &plan);
            assert!(report.is_complete(), "{report:?}");
            assert_eq!(f.queue(), [LAST, DUPLICATE, DUPLICATE]);
            let selected = soap::get_position_info(&f.lan, f.host).unwrap();
            assert_eq!((selected.track, selected.position_secs), (3, Some(29)));
            assert_eq!(
                get_play_mode(&f.lan, f.host).unwrap(),
                PlayMode::ShuffleNoRepeat
            );
            let saved = finish_backup(&f, pending, &receipt, &report).unwrap();
            let serialized = serde_json::to_value(&saved).unwrap();
            assert_eq!(serialized["version"], 4);
            assert_eq!(serialized["after"]["playback"]["track"], 3);

            // Arbitrary selected occurrences belong only to the restoration
            // format. Older records still require a first-track replacement.
            for version in [2, 3] {
                let mut older = serialized.clone();
                older["version"] = version.into();
                if version == 2 {
                    older.as_object_mut().unwrap().remove("scene");
                }
                let older: QueueUndo = serde_json::from_value(older).unwrap();
                let before = actions::with_queue_undo(None, &[older]).unwrap();
                assert!(
                    actions::BeforeState::parse(&before)
                        .unwrap()
                        .not_undoable
                        .is_some()
                );
            }
            for track in [0, 4] {
                let mut invalid = serialized.clone();
                invalid["after"]["playback"]["track"] = track.into();
                let invalid: QueueUndo = serde_json::from_value(invalid).unwrap();
                let before = actions::with_queue_undo(None, &[invalid]).unwrap();
                assert!(
                    actions::BeforeState::parse(&before)
                        .unwrap()
                        .not_undoable
                        .is_some()
                );
            }

            let directory = tempfile::tempdir().unwrap();
            let path = directory.path().join("durable-scene-undo.sqlite");
            let mut store = SqliteStore::open(&path).unwrap();
            record(&mut store, saved);
            store.close().unwrap();
            let mut reopened = SqliteStore::open(&path).unwrap();
            let quiet = QuietLevels::new(&f, false);
            let before = f.sim.soap_log().len();
            let report = actions::undo_last(
                &quiet,
                &f.houses,
                &mut reopened,
                None,
                &Client::Cli,
                "test",
                102,
            )
            .unwrap()
            .unwrap();
            assert!(report.failures.is_empty(), "{report:?}");
            assert_eq!(quiet.level_writes.get(), 4);
            f.restored(!start);
            assert_eq!(soap::get_volume(&f.lan, f.host).unwrap(), 35);
            assert!(!soap::get_mute(&f.lan, f.host).unwrap());
            let beta = f.sim.player("Beta").unwrap().ip;
            assert_eq!(soap::get_volume(&f.lan, beta).unwrap(), 12);
            assert!(soap::get_mute(&f.lan, beta).unwrap());
            let writes = f.writes_since(before);
            assert!(
                !writes
                    .iter()
                    .any(|write| write == "RemoveAllTracksFromQueue")
            );
            assert_eq!(
                writes.iter().filter(|write| *write == "Play").count(),
                usize::from(!start),
            );
            reopened.close().unwrap();
        }
    }
}

#[test]
fn a_durable_recall_receipt_cannot_adopt_another_original_queue_owner() {
    for generation in [1, 2] {
        for change in [0, 1, 3, 7] {
            let f = fixture(generation, false);
            let plan = backup_plan(&f, true, true);
            let pending =
                actions::capture_scene_queue_undo(&f.lan, &f.houses, &f.coordinator, &plan, 100)
                    .unwrap();
            if change == 7 {
                // Equal URIs are distinct occurrences even before recall.
                soap::seek_track(&f.lan, f.host, 1).unwrap();
            } else {
                owner_change(&f, change);
            }
            let (receipt, report) = recall(&f, &f.lan, &plan);
            assert!(report.is_complete(), "{report:?}");
            let before = f.sim.soap_log().len();
            assert!(
                finish_backup(&f, pending, &receipt, &report).is_err(),
                "change {change}"
            );
            assert!(f.writes_since(before).is_empty());
        }
    }
}

#[test]
fn a_durable_recall_cannot_publish_after_its_completed_result_changes() {
    for generation in [1, 2] {
        for change in 0..8 {
            let f = fixture(generation, false);
            let plan = backup_plan(&f, true, true);
            let pending =
                actions::capture_scene_queue_undo(&f.lan, &f.houses, &f.coordinator, &plan, 100)
                    .unwrap();
            let (receipt, report) = recall(&f, &f.lan, &plan);
            assert!(report.is_complete(), "{report:?}");
            if change == 7 {
                // The adjacent occurrence has the same URI, but is not the
                // saved third occurrence this request actually selected.
                soap::seek_track(&f.lan, f.host, 2).unwrap();
            } else {
                owner_change(&f, change);
            }
            let before = f.sim.soap_log().len();
            assert!(
                finish_backup(&f, pending, &receipt, &report).is_err(),
                "change {change}"
            );
            assert!(f.writes_since(before).is_empty());
        }
    }
}

#[test]
fn unchanged_contents_and_incomplete_durable_scenes_remain_queue_undo_barriers() {
    for generation in [1, 2] {
        for incomplete in [false, true] {
            let f = fixture(generation, false);
            let plan = backup_plan(&f, true, incomplete);
            let pending =
                actions::capture_scene_queue_undo(&f.lan, &f.houses, &f.coordinator, &plan, 100)
                    .unwrap();
            let before = f.sim.soap_log().len();
            let (receipt, report) = if incomplete {
                let lost = LostSceneLevel {
                    lan: f.lan.clone(),
                    host: f.sim.player("Beta").unwrap().ip,
                    lost: Cell::new(false),
                };
                let result = recall(&f, &lost, &plan);
                assert!(lost.lost.get());
                assert_eq!(result.1.failed.len(), 1);
                result
            } else {
                let result = recall(&f, &f.lan, &plan);
                assert!(result.1.is_complete(), "{:?}", result.1);
                let writes = f.writes_since(before);
                assert!(!writes.iter().any(|name| {
                    matches!(name.as_str(), "AddURIToQueue" | "RemoveTrackRangeFromQueue")
                }));
                result
            };
            let before = f.sim.soap_log().len();
            assert!(finish_backup(&f, pending, &receipt, &report).is_err());
            assert!(f.writes_since(before).is_empty());
        }
    }
}
