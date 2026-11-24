use super::*;
use crate::zones::fixtures::households;
use chrono::DateTime;
use fsonos_core::clock::FakeClock;
use fsonos_core::policy::Policy;
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::SimHousehold;
use serde_json::json;
use std::net::IpAddr;
use std::sync::{Arc, Mutex, mpsc};

struct NoSpeakerIo;

impl Transport for NoSpeakerIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("scene definition changes must not read or command speaker playback")
    }
}

fn clock() -> FakeClock {
    FakeClock::new(DateTime::parse_from_rfc3339("2026-10-08T12:00:00Z").unwrap())
}

fn fixture() -> Surface {
    Surface::new(
        Box::new(NoSpeakerIo),
        Box::new(|_| Ok(households())),
        Policy::default(),
        Box::new(clock()),
    )
    .with_action_log(Box::new(MemStore::default()), "scene-revisions")
}

fn authored(level: u8, expected: Option<&str>) -> SceneSaveRequest {
    SceneSaveRequest {
        volumes: BTreeMap::from([("Patio".into(), level)]),
        expected_revision: expected.map(str::to_string),
        ..SceneSaveRequest::default()
    }
}

fn stored(surface: &Surface, name: &str) -> Option<StoredScene> {
    surface
        .scene_store(|store| scenes::observe(store, name))
        .unwrap()
}

fn put(surface: &Surface, row: &StoredScene) {
    surface
        .with_store(|store| store.save_scene(row))
        .unwrap()
        .unwrap();
}

#[test]
fn revisions_bind_exact_private_rows_and_remain_consistent_across_reads() {
    let surface = fixture();
    let saved = surface
        .save_scene_with(&Client::Cli, "Evening", &authored(21, Some("absent")))
        .unwrap();
    assert!(saved.revision.starts_with("scene-v1:"));
    assert_eq!(surface.scene(&Client::Cli, "evening").unwrap(), saved);
    assert_eq!(surface.scenes(&Client::Cli).unwrap(), [saved.clone()]);

    let mut row = stored(&surface, "Evening").unwrap();
    let mut private = scenes::from_stored(&row).unwrap();
    private.writer = Some("tag:another-controller".into());
    row.spec = serde_json::to_string(&private).unwrap();
    put(&surface, &row);
    let changed = surface.scene(&Client::Cli, "Evening").unwrap();
    assert_eq!(changed.saved, saved.saved, "same-second edits are distinct");
    assert_eq!(changed.groups, saved.groups);
    assert_eq!(changed.volumes, saved.volumes);
    assert_ne!(
        changed.revision, saved.revision,
        "the hidden owner is bound"
    );
    assert!(
        !serde_json::to_string(&changed)
            .unwrap()
            .contains("another-controller")
    );

    row.spec = format!(" {} ", row.spec);
    put(&surface, &row);
    let raw_changed = surface.scene(&Client::Cli, "Evening").unwrap();
    assert_ne!(
        raw_changed.revision, changed.revision,
        "retain original serialized bytes"
    );
    row.updated += 1;
    put(&surface, &row);
    assert_ne!(
        surface.scene(&Client::Cli, "Evening").unwrap().revision,
        raw_changed.revision
    );

    let mut legacy = serde_json::to_value(saved).unwrap();
    legacy.as_object_mut().unwrap().remove("revision");
    assert!(
        serde_json::from_value::<SceneDto>(legacy)
            .unwrap()
            .revision
            .is_empty()
    );
}

#[test]
fn reviewed_overwrite_and_delete_preserve_newer_definitions_and_exact_receipts() {
    let surface = fixture();
    let first = surface
        .save_scene_with(&Client::Cli, "Evening", &authored(21, Some("absent")))
        .unwrap();
    let second = surface
        .save_scene_with(
            &Client::Cli,
            "EVENING",
            &authored(32, Some(&first.revision)),
        )
        .unwrap();
    assert_eq!(second.name, "EVENING");
    assert_eq!(surface.scenes(&Client::Cli).unwrap().len(), 1);
    assert_ne!(second.revision, first.revision);
    for expected in [&first.revision, "absent"] {
        assert_eq!(
            surface
                .save_scene_with(&Client::Cli, "Evening", &authored(44, Some(expected)))
                .unwrap_err()
                .code,
            ErrorCode::ConfigChanged
        );
        assert_eq!(
            surface
                .delete_scene_with(
                    &Client::Cli,
                    "evening",
                    &SceneDeleteRequest {
                        expected_revision: Some(expected.to_string()),
                    }
                )
                .unwrap_err()
                .code,
            ErrorCode::ConfigChanged
        );
    }
    assert_eq!(surface.scene(&Client::Cli, "Evening").unwrap(), second);
    let log = surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(
        log.len(),
        2,
        "conflicts do not create successful action receipts"
    );

    surface.undo(&Client::Cli, true).unwrap().unwrap();
    assert_eq!(
        surface.scene(&Client::Cli, "Evening").unwrap(),
        first,
        "undo retains the actual previous row including its timestamp and spelling"
    );
    surface
        .delete_scene_with(
            &Client::Cli,
            "evening",
            &SceneDeleteRequest {
                expected_revision: Some(first.revision),
            },
        )
        .unwrap();
    assert!(surface.scenes(&Client::Cli).unwrap().is_empty());
}

#[test]
fn malformed_proofs_and_another_writers_revision_cannot_authorize_a_change() {
    let surface = fixture();
    let owner = Client::Tailnet("tag:first-controller".into());
    let other = Client::Tailnet("tag:second-controller".into());
    let saved = surface
        .save_scene_with(&owner, "Evening", &authored(21, None))
        .unwrap();
    for revision in ["", "  ", "bad\nrevision"] {
        assert_eq!(
            surface
                .save_scene_with(&owner, "Evening", &authored(22, Some(revision)))
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
        assert_eq!(
            surface
                .delete_scene_with(
                    &owner,
                    "Evening",
                    &SceneDeleteRequest {
                        expected_revision: Some(revision.into()),
                    }
                )
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }
    assert_eq!(
        surface
            .save_scene_with(&other, "Evening", &authored(22, Some(&saved.revision)))
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    assert_eq!(
        surface
            .delete_scene_with(
                &other,
                "Evening",
                &SceneDeleteRequest {
                    expected_revision: Some(saved.revision.clone()),
                }
            )
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    assert_eq!(surface.scene(&Client::Cli, "Evening").unwrap(), saved);
}

#[test]
fn stale_capture_proofs_fail_before_reading_speakers() {
    let surface = fixture();
    surface
        .save_scene_with(&Client::Cli, "Evening", &authored(21, None))
        .unwrap();
    for backup_queues in [false, true] {
        let request = SceneSaveRequest {
            expected_revision: Some("absent".into()),
            backup_queues,
            ..SceneSaveRequest::default()
        };
        assert_eq!(
            surface
                .save_scene_with(&Client::Cli, "Evening", &request)
                .unwrap_err()
                .code,
            ErrorCode::ConfigChanged
        );
    }
}

#[test]
fn legacy_capture_backup_and_authored_calls_fence_changes_during_household_reads() {
    let sim = SimHousehold::standard().spawn().unwrap();
    let house = fsonos_core::inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households;
    for mode in ["capture", "backup", "authored"] {
        for existed in [false, true] {
            let (arrive, arrived) = mpsc::sync_channel(1);
            let (release, waiting) = mpsc::sync_channel(1);
            let waiting = Mutex::new(waiting);
            let house = house.clone();
            let surface = Arc::new(
                Surface::new(
                    Box::new(sim.lan()),
                    Box::new(move |_| {
                        arrive.send(()).unwrap();
                        waiting
                            .lock()
                            .unwrap()
                            .recv_timeout(Duration::from_secs(10))
                            .unwrap();
                        Ok(house.clone())
                    }),
                    Policy::default(),
                    Box::new(clock()),
                )
                .with_action_log(Box::new(MemStore::default()), "scene-race"),
            );
            let initial = Scene {
                name: "Evening".into(),
                groups: Vec::new(),
                volumes: BTreeMap::from([("Kitchen".into(), 21)]),
                mutes: BTreeMap::new(),
                writer: Some("tag:first-controller".into()),
            };
            let initial = scenes::to_stored(&initial, surface.now()).unwrap();
            if existed {
                put(&surface, &initial);
            }
            let definition: SceneDefinition =
                serde_json::from_value(json!({"volumes":{"Kitchen":22}})).unwrap();
            let mut replacement = scenes::from_stored(&initial).unwrap();
            replacement.writer = Some("tag:second-controller".into());
            replacement.volumes.insert("Kitchen".into(), 35);
            let replacement = scenes::to_stored(&replacement, initial.updated).unwrap();
            std::thread::scope(|scope| {
                let worker = scope.spawn(|| {
                    let client = Client::Tailnet("tag:first-controller".into());
                    match mode {
                        "capture" => surface.save_scene(&client, "Evening"),
                        "backup" => surface.save_scene_with_queue_backups(&client, "Evening"),
                        _ => surface.save_scene_from(&client, "Evening", &definition),
                    }
                });
                arrived.recv_timeout(Duration::from_secs(10)).unwrap();
                put(&surface, &replacement);
                release.send(()).unwrap();
                let failure = worker.join().unwrap().unwrap_err();
                assert_eq!(
                    failure.code,
                    ErrorCode::ConfigChanged,
                    "{mode}, existed={existed}: {failure:?}"
                );
            });
            assert_eq!(stored(&surface, "Evening").unwrap(), replacement);
            assert!(
                surface
                    .recent_actions(&Client::Cli, &ActionFilter::default())
                    .unwrap()
                    .is_empty()
            );
        }
    }
    sim.shutdown();
}
