use super::*;
use crate::dj::DjDirectionDto;
use crate::surface::announce::AnnounceRequest;
use crate::zones::fixtures::households;
use chrono::{DateTime, FixedOffset, TimeDelta};
use fsonos_core::clock::{Clock, FakeClock};
use fsonos_core::policy::Policy;
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_proto::{ProtoError, Transport};
use serde_json::json;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::Duration;

struct NoSpeakerIo;
impl Transport for NoSpeakerIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("editing schedules never controls or reads speaker playback")
    }
}

struct SharedClock(Arc<FakeClock>);
impl Clock for SharedClock {
    fn now(&self) -> DateTime<FixedOffset> {
        self.0.now()
    }
}

fn fixture(policy: Policy) -> (Arc<Surface>, Arc<FakeClock>) {
    let clock = Arc::new(FakeClock::new(
        DateTime::parse_from_rfc3339("2026-10-08T12:00:00Z").unwrap(),
    ));
    let surface = Surface::new(
        Box::new(NoSpeakerIo),
        Box::new(|_| Ok(households())),
        policy,
        Box::new(SharedClock(clock.clone())),
    )
    .with_action_log(Box::new(MemStore::default()), "schedule-edit");
    (Arc::new(surface), clock)
}

fn pause(when: &str) -> ScheduleRequest {
    ScheduleRequest {
        when: when.into(),
        action: "pause".into(),
        zone: Some("Patio".into()),
        ..ScheduleRequest::default()
    }
}

fn edit(observed: &ScheduleDto, definition: ScheduleRequest) -> UpdateScheduleRequest {
    UpdateScheduleRequest {
        id: observed.id,
        expected_revision: observed.revision.clone(),
        schedule: definition,
    }
}

fn row(surface: &Surface, id: i64) -> StoredSchedule {
    surface.observed_schedule(id).unwrap().1
}

fn definitions() -> [ScheduleRequest; 5] {
    [
        ScheduleRequest {
            when: "weekends 10:15".into(),
            action: "volume".into(),
            zone: Some("Patio".into()),
            volume: Some(23),
            ..ScheduleRequest::default()
        },
        ScheduleRequest {
            when: "daily 11:00".into(),
            action: "dj_start".into(),
            zone: Some("Patio".into()),
            direction: Some(DjDirectionDto::default()),
            ..ScheduleRequest::default()
        },
        ScheduleRequest {
            when: "daily 22:00".into(),
            action: "apply_scene".into(),
            scene: Some("evening".into()),
            ..ScheduleRequest::default()
        },
        ScheduleRequest {
            when: "in 45m".into(),
            action: "announce".into(),
            announcement: Some(AnnounceRequest {
                chime: Some("bell".into()),
                rooms: vec!["Patio".into()],
                volume: Some(15),
                ..AnnounceRequest::default()
            }),
            ..ScheduleRequest::default()
        },
        ScheduleRequest {
            fade_secs: Some(90),
            ..pause("daily 23:00")
        },
    ]
}

#[test]
fn all_action_kinds_edit_in_place_preserving_paused_identity_and_creator() {
    let (surface, clock) = fixture(Policy::default());
    let creator = Client::Tailnet("tag:creator".into());
    let added = surface
        .add_schedule(&creator, &pause("daily 09:30"))
        .unwrap();
    let mut previous = surface.pause_schedule(&creator, added.id, true).unwrap();
    let original = row(&surface, added.id);
    let scene = fsonos_core::scenes::Scene {
        name: "evening".into(),
        groups: vec![fsonos_core::scenes::SceneGroup {
            coordinator: "Patio".into(),
            members: Vec::new(),
            source: Default::default(),
            playing: Some(false),
        }],
        volumes: Default::default(),
        mutes: Default::default(),
        writer: None,
    };
    surface
        .with_store(|store| {
            fsonos_core::scenes::save(store, &scene, clock.now().timestamp())
                .map_err(|error| fsonos_core::store::StoreError::Backend(error.to_string()))
        })
        .unwrap();
    for definition in definitions() {
        clock.advance(TimeDelta::seconds(1));
        let changed = surface
            .update_schedule(&Client::Cli, &edit(&previous, definition.clone()))
            .unwrap();
        let stored = row(&surface, added.id);
        assert_eq!(
            (stored.id, &stored.creator, stored.created, stored.enabled),
            (original.id, &original.creator, original.created, false)
        );
        assert_eq!(stored.last_fired, Some(clock.now().timestamp()));
        assert_eq!(
            serde_json::from_str::<super::super::ScheduleAction>(&stored.action).unwrap(),
            definition.action().unwrap()
        );
        assert_ne!(changed.revision, previous.revision);
        assert!(!changed.enabled && changed.next.is_none());
        assert_eq!(surface.schedules(&Client::Cli).unwrap().len(), 1);
        assert_eq!(
            surface
                .update_schedule(&Client::Cli, &edit(&previous, definition))
                .unwrap_err()
                .code,
            ErrorCode::ConfigChanged
        );
        previous = changed;
    }
    let actions = surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    let edits: Vec<_> = actions
        .iter()
        .filter(|action| action.action.intent.starts_with("update_schedule:"))
        .collect();
    assert_eq!(edits.len(), 5);
    assert!(edits.iter().all(|action| {
        action.action.client == "cli"
            && action
                .action
                .before_state
                .as_deref()
                .unwrap()
                .contains("not_undoable")
    }));
}

#[test]
fn an_unchanged_definition_preserves_revision_and_consumed_one_shots_need_a_future_time() {
    let (surface, clock) = fixture(Policy::default());
    let added = surface.add_schedule(&Client::Cli, &pause("in 5m")).unwrap();
    let request = edit(&added, pause(&added.when));
    assert_eq!(
        surface.update_schedule(&Client::Cli, &request).unwrap(),
        added
    );
    let before = row(&surface, added.id);
    clock.advance(TimeDelta::minutes(10));
    surface
        .with_store(|store| store.mark_schedule_fired(added.id, clock.now().timestamp()))
        .unwrap();
    let consumed = surface.schedules(&Client::Cli).unwrap().remove(0);
    assert!(consumed.next.is_none());
    let unchanged = surface
        .update_schedule(&Client::Cli, &edit(&consumed, pause(&consumed.when)))
        .unwrap();
    assert_eq!(unchanged, consumed);
    let changed = ScheduleRequest {
        fade_secs: Some(10),
        ..pause(&consumed.when)
    };
    assert_eq!(
        surface
            .update_schedule(&Client::Cli, &edit(&consumed, changed))
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    let future = surface
        .update_schedule(&Client::Cli, &edit(&consumed, pause("in 20m")))
        .unwrap();
    let at = DateTime::parse_from_rfc3339(future.next.as_ref().unwrap()).unwrap();
    assert_eq!(at.timestamp(), clock.now().timestamp() + 20 * 60);
    assert_eq!(row(&surface, added.id).created, before.created);
    assert_eq!(
        surface
            .update_schedule(&Client::Cli, &request)
            .unwrap_err()
            .code,
        ErrorCode::ConfigChanged
    );
}

#[test]
fn editing_checks_ownership_and_both_editors_and_original_creators_action_rights() {
    let policy = Policy::from_toml("[clients.\"tag:creator\"]\ndeny = ['announce', 'dj_steer']\n[clients.\"tag:manager\"]\nmanage_others = true\ndeny = ['set_volume']\n").unwrap();
    let (surface, _) = fixture(policy);
    let creator = Client::Tailnet("tag:creator".into());
    let added = surface
        .add_schedule(&creator, &pause("daily 10:00"))
        .unwrap();
    let original = row(&surface, added.id);
    let other = Client::Tailnet("tag:other".into());
    assert_eq!(
        surface
            .update_schedule(&other, &edit(&added, pause("daily 11:00")))
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    for definition in [
        ScheduleRequest {
            when: "daily 10:30".into(),
            action: "announce".into(),
            announcement: Some(AnnounceRequest {
                chime: Some("bell".into()),
                ..AnnounceRequest::default()
            }),
            ..ScheduleRequest::default()
        },
        ScheduleRequest {
            when: "daily 10:30".into(),
            action: "dj_start".into(),
            zone: Some("Patio".into()),
            direction: Some(DjDirectionDto::default()),
            ..ScheduleRequest::default()
        },
    ] {
        assert_eq!(
            surface
                .update_schedule(&Client::Cli, &edit(&added, definition))
                .unwrap_err()
                .code,
            ErrorCode::PolicyDenied
        );
    }
    let volume = ScheduleRequest {
        when: "daily 10:30".into(),
        action: "volume".into(),
        zone: Some("Patio".into()),
        volume: Some(20),
        ..ScheduleRequest::default()
    };
    assert_eq!(
        surface
            .update_schedule(
                &Client::Tailnet("tag:manager".into()),
                &edit(&added, volume)
            )
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    assert_eq!(row(&surface, added.id), original);
    assert!(
        surface
            .update_schedule(
                &Client::Tailnet("tag:manager".into()),
                &edit(&added, pause("daily 11:00"))
            )
            .is_ok()
    );
}

#[test]
fn invalid_missing_unknown_and_obsolete_edits_never_modify_a_schedule() {
    let (surface, _) = fixture(Policy::default());
    let observed = surface
        .add_schedule(&Client::Cli, &pause("daily 10:00"))
        .unwrap();
    let original = row(&surface, observed.id);
    for (id, revision) in [(0, "revision"), (observed.id, ""), (observed.id, "a\nb")] {
        assert_eq!(
            surface
                .update_schedule(
                    &Client::Cli,
                    &UpdateScheduleRequest {
                        id,
                        expected_revision: revision.into(),
                        schedule: pause("daily 11:00")
                    }
                )
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }
    let mut missing = edit(&observed, pause("daily 11:00"));
    missing.id += 99;
    assert_eq!(
        surface
            .update_schedule(&Client::Cli, &missing)
            .unwrap_err()
            .code,
        ErrorCode::UnknownSchedule
    );
    let mut unknown = pause("daily 11:00");
    unknown.zone = Some("Unlisted Room".into());
    assert_eq!(
        surface
            .update_schedule(&Client::Cli, &edit(&observed, unknown))
            .unwrap_err()
            .code,
        ErrorCode::UnknownRoom
    );
    for body in [
        json!({"id":observed.id,"schedule":pause("daily 11:00")}),
        json!({"id":observed.id,"expected_revision":"r","schedule":pause("daily 11:00"),"creator":"cli"}),
    ] {
        assert!(serde_json::from_value::<UpdateScheduleRequest>(body).is_err());
    }
    assert_eq!(row(&surface, observed.id), original);
    let mut legacy = serde_json::to_value(&observed).unwrap();
    legacy.as_object_mut().unwrap().remove("revision");
    assert!(
        serde_json::from_value::<ScheduleDto>(legacy)
            .unwrap()
            .revision
            .is_empty()
    );
    let paused = surface
        .pause_schedule(&Client::Cli, observed.id, true)
        .unwrap();
    assert_ne!(paused.revision, observed.revision);
    assert_eq!(
        surface
            .update_schedule(&Client::Cli, &edit(&observed, pause("daily 11:00")))
            .unwrap_err()
            .code,
        ErrorCode::ConfigChanged
    );
}

#[test]
fn a_scheduler_claim_between_observation_and_commit_refuses_the_edit() {
    let clock = Arc::new(FakeClock::new(
        DateTime::parse_from_rfc3339("2026-10-08T12:00:00Z").unwrap(),
    ));
    let armed = Arc::new(AtomicBool::new(false));
    let (arrive, arrived) = mpsc::sync_channel(1);
    let (release, waiting) = mpsc::sync_channel(1);
    let gate = armed.clone();
    let waiting = Mutex::new(waiting);
    let surface = Arc::new(
        Surface::new(
            Box::new(NoSpeakerIo),
            Box::new(move |_| {
                if gate.swap(false, Ordering::SeqCst) {
                    arrive.send(()).unwrap();
                    waiting
                        .lock()
                        .unwrap()
                        .recv_timeout(Duration::from_secs(5))
                        .unwrap();
                }
                Ok(households())
            }),
            Policy::default(),
            Box::new(SharedClock(clock.clone())),
        )
        .with_action_log(Box::new(MemStore::default()), "schedule-race"),
    );
    let added = surface
        .add_schedule(&Client::Cli, &pause("daily 10:00"))
        .unwrap();
    let observed = row(&surface, added.id);
    armed.store(true, Ordering::SeqCst);
    std::thread::scope(|scope| {
        let worker = scope
            .spawn(|| surface.update_schedule(&Client::Cli, &edit(&added, pause("daily 11:00"))));
        arrived.recv_timeout(Duration::from_secs(5)).unwrap();
        assert!(
            surface
                .with_store(|store| store.claim_schedule_fired(&observed, clock.now().timestamp()))
                .unwrap()
                .unwrap()
        );
        release.send(()).unwrap();
        assert_eq!(
            worker.join().unwrap().unwrap_err().code,
            ErrorCode::ConfigChanged
        );
    });
    let after = row(&surface, added.id);
    assert_eq!(after.spec, observed.spec);
    assert_eq!(after.last_fired, Some(clock.now().timestamp()));
}
