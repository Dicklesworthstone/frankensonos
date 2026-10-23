//! Durable queue scenes use the real localhost SOAP simulator on S1 and S2.
//! Staging never clears the old queue, repeats an uncertain write, or starts
//! playback after a failed ownership or queue-content proof.

use fsonos_core::HouseholdState;
use fsonos_core::scenes::{self, ApplyOptions, Scene, SceneOp, SceneSource};
use fsonos_core::snapshot;
use fsonos_core::store::MemStore;
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, PlayMode, PlayerId, TransportState};
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Duration;

const DUPLICATE: &str = "x-file-cifs://example.invalid/repeated.flac";
const LAST: &str = "x-file-cifs://example.invalid/last.flac";
const OLD: &str = "x-file-cifs://example.invalid/replacement.flac";
const RADIO: &str = "x-rincon-mp3radio://example.invalid/radio.mp3";

struct Fixture {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    coordinator: PlayerId,
    host: IpAddr,
}

impl Fixture {
    fn new(generation: u8) -> Self {
        let model = if generation == 1 {
            SimModel::Play5Gen1
        } else {
            SimModel::One
        };
        let players = [
            SimPlayerSpec::new("Alpha", model),
            SimPlayerSpec::new("Beta", model),
        ];
        let builder = SimHousehold::builder();
        let sim = if generation == 1 {
            builder.s1(players)
        } else {
            builder.s2(players)
        }
        .spawn()
        .unwrap();
        let lan = sim.lan();
        let host = sim.player("Alpha").unwrap().ip;
        let coordinator = PlayerId(sim.player("Alpha").unwrap().uuid.clone());
        soap::join_group(&lan, sim.player("Beta").unwrap().ip, &coordinator).unwrap();
        for uri in [DUPLICATE, DUPLICATE, LAST] {
            soap::add_uri_to_queue(&lan, host, uri, "", false).unwrap();
        }
        soap::play_from_queue(&lan, host, &coordinator).unwrap();
        soap::seek_track(&lan, host, 2).unwrap();
        soap::seek_position(&lan, host, 17).unwrap();
        set_play_mode(&lan, host, PlayMode::RepeatAll).unwrap();
        soap::play(&lan, host).unwrap();
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
        house.id = Some(HouseholdId(sim.player("Alpha").unwrap().household.clone()));
        Self {
            sim,
            lan,
            houses: vec![house],
            coordinator,
            host,
        }
    }

    fn capture(&self) -> Scene {
        scenes::capture_with_queue_backups(&self.lan, &self.houses, "Saved evening", &[], 1)
            .unwrap()
    }

    fn replace(&self) {
        soap::remove_all_tracks_from_queue(&self.lan, self.host).unwrap();
        soap::add_uri_to_queue(&self.lan, self.host, OLD, "", false).unwrap();
        soap::set_av_transport_uri(&self.lan, self.host, RADIO, "").unwrap();
        set_play_mode(&self.lan, self.host, PlayMode::Normal).unwrap();
        soap::play(&self.lan, self.host).unwrap();
    }

    fn apply<T: Transport + ?Sized>(&self, transport: &T, scene: &Scene) -> scenes::SceneReport {
        let snaps: Vec<_> = scenes::zones(scene, &self.houses)
            .iter()
            .map(|id| snapshot::capture(&self.lan, &self.houses, id, 2).unwrap())
            .collect();
        let plan = scenes::plan_apply(scene, &self.houses, &snaps).unwrap();
        scenes::apply(transport, &self.houses, &plan, &ApplyOptions::default())
    }

    fn queue(&self) -> Vec<String> {
        browse_snapshot(&self.lan, self.host, QUEUE)
            .unwrap()
            .objects
            .into_iter()
            .map(|item| item.res.unwrap().uri)
            .collect()
    }

    fn writes_since(&self, start: usize) -> Vec<String> {
        self.sim.soap_log()[start..]
            .iter()
            .filter(|call| !call.action.starts_with("Get") && call.action != "Browse")
            .map(|call| call.action.clone())
            .collect()
    }

    fn restored(&self, playing: bool) {
        assert_eq!(self.queue(), [DUPLICATE, DUPLICATE, LAST]);
        let position = soap::get_position_info(&self.lan, self.host).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(17)));
        assert_eq!(position.uri, DUPLICATE);
        assert_eq!(
            get_play_mode(&self.lan, self.host).unwrap(),
            PlayMode::RepeatAll
        );
        assert_eq!(
            soap::get_transport_info(&self.lan, self.host)
                .unwrap()
                .state
                == TransportState::Playing,
            playing
        );
    }
}

#[test]
fn saved_contents_survive_queue_edits_and_reboots_with_duplicate_occurrence_time_and_mode() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let bookmark = scenes::capture(&f.lan, &f.houses, "Bookmark", &[], 1).unwrap();
        assert!(matches!(
            bookmark.groups[0].source,
            SceneSource::Queue { .. }
        ));
        let scene = f.capture();
        let SceneSource::QueueBackup { backup } = &scene.groups[0].source else {
            panic!("{scene:?}")
        };
        assert_eq!(backup.entries.len(), 3);
        assert_eq!(backup.entries[0].uri, backup.entries[1].uri);
        assert_eq!(backup.reference.coordinator, f.coordinator);
        assert_eq!(backup.reference.track, 2);
        let mut store = MemStore::default();
        scenes::save(&mut store, &scene, 1).unwrap();
        let scene = scenes::load(&store, "Saved evening").unwrap();
        f.replace();
        f.sim.reboot("Alpha", Duration::ZERO).unwrap();
        let start = f.sim.soap_log().len();
        let report = f.apply(&f.lan, &scene);
        assert!(report.is_complete(), "{report:?}");
        f.restored(true);
        let writes = f.writes_since(start);
        assert!(!writes.iter().any(|name| name == "RemoveAllTracksFromQueue"));
        assert_eq!(
            writes
                .iter()
                .filter(|name| name.as_str() == "AddURIToQueue")
                .count(),
            3
        );
        assert_eq!(
            writes
                .iter()
                .filter(|name| name.as_str() == "RemoveTrackRangeFromQueue")
                .count(),
            1
        );
        let start = f.sim.soap_log().len();
        let again = f.apply(&f.lan, &scene);
        assert!(again.is_complete() && again.done.is_empty(), "{again:?}");
        assert!(f.writes_since(start).is_empty());
    }
}

#[test]
fn paused_queue_reconstruction_never_plays_even_when_the_previous_source_was_playing() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        soap::pause(&f.lan, f.host).unwrap();
        let scene = f.capture();
        f.replace();
        let start = f.sim.soap_log().len();
        let report = f.apply(&f.lan, &scene);
        assert!(report.is_complete(), "{report:?}");
        f.restored(false);
        assert!(!f.writes_since(start).iter().any(|name| name == "Play"));
    }
}

#[derive(Clone, Copy)]
enum Event {
    LostAppend,
    MalformedAppend,
    LostRemoval,
    MalformedRemoval,
    AppendNotApplied,
    QueueEdit,
    SourceChange,
    GroupLeave,
    Reboot,
    ModeChange,
    ProofUnavailable,
    SeekAfterPause,
    SeekAfterRemoval,
}

struct Interleaved<'a> {
    fixture: &'a Fixture,
    event: Event,
    fired: Cell<bool>,
}

impl Transport for Interleaved<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if matches!(self.event, Event::ProofUnavailable) && self.fired.get() && name == "Browse" {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected unavailable queue proof".into(),
            });
        }
        let trigger = match self.event {
            Event::LostRemoval | Event::MalformedRemoval | Event::SeekAfterRemoval => {
                name == "RemoveTrackRangeFromQueue"
            }
            Event::SeekAfterPause => name == "Pause",
            _ => name == "AddURIToQueue",
        };
        if host != self.fixture.host || !trigger || self.fired.replace(true) {
            return self.fixture.lan.soap_post(host, path, action, body);
        }
        if matches!(self.event, Event::AppendNotApplied) {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected append never reached player".into(),
            });
        }
        let result = self.fixture.lan.soap_post(host, path, action, body);
        let lan = &self.fixture.lan;
        match self.event {
            Event::LostAppend | Event::LostRemoval | Event::ProofUnavailable => {
                return Err(ProtoError::Network {
                    target: host.to_string(),
                    detail: "injected lost write reply".into(),
                });
            }
            Event::MalformedAppend | Event::MalformedRemoval => return Ok("<truncated".into()),
            Event::QueueEdit => {
                soap::add_uri_to_queue(
                    lan,
                    host,
                    "x-file-cifs://example.invalid/concurrent.flac",
                    "",
                    false,
                )?;
            }
            Event::SourceChange => {
                soap::set_av_transport_uri(lan, host, "http://example.invalid/new-owner.mp3", "")?;
            }
            Event::GroupLeave => {
                let member = self.fixture.sim.player("Beta").unwrap();
                soap::leave_group(lan, member.ip, &PlayerId(member.uuid.clone()))?;
            }
            Event::Reboot => {
                self.fixture.sim.reboot("Alpha", Duration::ZERO).unwrap();
            }
            Event::ModeChange => set_play_mode(lan, host, PlayMode::RepeatOne)?,
            Event::SeekAfterPause | Event::SeekAfterRemoval => soap::seek_position(lan, host, 99)?,
            Event::AppendNotApplied => unreachable!(),
        }
        result
    }
}

#[test]
fn lost_or_malformed_append_and_commit_replies_are_proven_without_repeating_writes() {
    for generation in [1, 2] {
        for event in [
            Event::LostAppend,
            Event::MalformedAppend,
            Event::LostRemoval,
            Event::MalformedRemoval,
        ] {
            let f = Fixture::new(generation);
            let scene = f.capture();
            f.replace();
            let transport = Interleaved {
                fixture: &f,
                event,
                fired: Cell::new(false),
            };
            let start = f.sim.soap_log().len();
            let report = f.apply(&transport, &scene);
            assert!(transport.fired.get());
            assert!(report.is_complete(), "{report:?}");
            f.restored(true);
            let writes = f.writes_since(start);
            assert_eq!(
                writes
                    .iter()
                    .filter(|name| name.as_str() == "AddURIToQueue")
                    .count(),
                3
            );
            assert_eq!(
                writes
                    .iter()
                    .filter(|name| name.as_str() == "RemoveTrackRangeFromQueue")
                    .count(),
                1
            );
        }
    }
}

#[test]
fn failed_staging_keeps_the_original_prefix_and_blocks_commit_and_playback() {
    for event in [
        Event::AppendNotApplied,
        Event::QueueEdit,
        Event::SourceChange,
        Event::GroupLeave,
        Event::Reboot,
        Event::ModeChange,
        Event::ProofUnavailable,
    ] {
        let f = Fixture::new(2);
        let scene = f.capture();
        f.replace();
        let transport = Interleaved {
            fixture: &f,
            event,
            fired: Cell::new(false),
        };
        let snaps = [snapshot::capture(&f.lan, &f.houses, &f.coordinator, 2).unwrap()];
        let mut plan = scenes::plan_apply(&scene, &f.houses, &snaps).unwrap();
        // Exercise execution dependency blocking, even for a manually supplied
        // extra Play after an unsuccessful source operation.
        plan.ops.push(SceneOp::Play {
            coordinator: f.coordinator.clone(),
        });
        let start = f.sim.soap_log().len();
        let report = scenes::apply(&transport, &f.houses, &plan, &ApplyOptions::default());
        assert!(transport.fired.get());
        assert!(!report.is_complete(), "{report:?}");
        assert!(
            report.failed[0].1.contains("staged suffix may remain"),
            "{report:?}"
        );
        assert_eq!(f.queue().first().map(String::as_str), Some(OLD));
        let writes = f.writes_since(start);
        assert!(
            !writes.iter().any(|name| name == "RemoveTrackRangeFromQueue"
                || name == "RemoveAllTracksFromQueue"
                || name == "Play"),
            "{writes:?}"
        );
        assert!(
            report
                .failed
                .iter()
                .any(|(op, _)| matches!(op, SceneOp::Play { .. })),
            "{report:?}"
        );
    }
}

#[test]
fn an_explicit_append_refusal_preserves_its_code_and_never_removes_the_old_queue() {
    let f = Fixture::new(2);
    let scene = f.capture();
    f.replace();
    f.sim.upnp_fault("Alpha", "AddURIToQueue", 800).unwrap();
    let start = f.sim.soap_log().len();
    let report = f.apply(&f.lan, &scene);
    assert!(!report.is_complete(), "{report:?}");
    assert!(report.failed[0].1.contains("800"), "{report:?}");
    assert_eq!(f.queue(), [OLD]);
    assert!(
        !f.writes_since(start)
            .iter()
            .any(|name| name == "RemoveTrackRangeFromQueue" || name == "Play")
    );
}

#[test]
fn saved_uuid_household_and_resource_validation_happen_before_any_write() {
    let f = Fixture::new(2);
    let original = f.capture();
    f.replace();
    for mutation in 0..4 {
        let mut scene = original.clone();
        let SceneSource::QueueBackup { backup } = &mut scene.groups[0].source else {
            panic!("missing backup")
        };
        match mutation {
            0 => backup.reference.coordinator = PlayerId("RINCON_REPLACED".into()),
            1 => backup.reference.household = Some(HouseholdId("Sonos_other".into())),
            2 => backup.entries[1].metadata.clear(),
            _ => backup.entries[1].uri = "x-rincon-queue:RINCON_OTHER#0".into(),
        }
        let start = f.sim.soap_log().len();
        let plan = scenes::ScenePlan {
            ops: vec![SceneOp::QueueBackup {
                coordinator: f.coordinator.clone(),
                backup: backup.clone(),
                start: true,
            }],
            notes: Vec::new(),
        };
        let report = scenes::apply(&f.lan, &f.houses, &plan, &ApplyOptions::default());
        assert!(!report.is_complete(), "{report:?}");
        assert!(f.writes_since(start).is_empty());
        assert_eq!(f.queue(), [OLD]);
    }
}

#[test]
fn a_cleared_queue_is_rebuilt_and_unknown_mode_is_not_replaced_with_normal() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let mut scene = f.capture();
        let SceneSource::QueueBackup { backup } = &mut scene.groups[0].source else {
            panic!("missing backup")
        };
        backup.reference.play_mode = None;
        soap::remove_all_tracks_from_queue(&f.lan, f.host).unwrap();
        set_play_mode(&f.lan, f.host, PlayMode::ShuffleNoRepeat).unwrap();
        let start = f.sim.soap_log().len();
        let report = f.apply(&f.lan, &scene);
        assert!(report.is_complete(), "{report:?}");
        assert_eq!(f.queue(), [DUPLICATE, DUPLICATE, LAST]);
        assert_eq!(soap::get_position_info(&f.lan, f.host).unwrap().track, 2);
        assert_eq!(
            get_play_mode(&f.lan, f.host).unwrap(),
            PlayMode::ShuffleNoRepeat
        );
        assert!(
            !f.writes_since(start)
                .iter()
                .any(|name| name == "SetPlayMode" || name == "RemoveTrackRangeFromQueue")
        );
    }
}

#[test]
fn spotify_backup_resolves_replay_descriptors_and_reconstructs_both_generations() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        soap::remove_all_tracks_from_queue(&f.lan, f.host).unwrap();
        fsonos_core::control::queue_spotify_tracks(
            &f.lan,
            &f.houses,
            &f.coordinator,
            &[
                ("spotify:track:0000000000000000000001", "One"),
                ("spotify:track:0000000000000000000002", "Two"),
            ],
        )
        .unwrap()
        .unwrap();
        soap::play_from_queue(&f.lan, f.host, &f.coordinator).unwrap();
        soap::seek_track(&f.lan, f.host, 2).unwrap();
        soap::seek_position(&f.lan, f.host, 17).unwrap();
        soap::play(&f.lan, f.host).unwrap();
        let saved = f.queue();
        let scene = f.capture();
        let SceneSource::QueueBackup { backup } = &scene.groups[0].source else {
            panic!("missing backup")
        };
        for entry in &backup.entries {
            let objects = fsonos_proto::didl::parse_didl(&entry.metadata).unwrap();
            assert_eq!(objects[0].desc.as_ref().unwrap().id, "cdudn");
            assert!(!objects[0].id.starts_with("Q:0/"));
        }
        f.replace();
        let report = f.apply(&f.lan, &scene);
        assert!(report.is_complete(), "{report:?}");
        assert_eq!(f.queue(), saved);
        assert_eq!(soap::get_position_info(&f.lan, f.host).unwrap().track, 2);
    }
}

struct Oversized<'a>(&'a SimLan);

impl Transport for Oversized<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.0.soap_post(host, path, action, body)?;
        if action.contains("#Browse") && body.contains("<ObjectID>Q:0</ObjectID>") {
            Ok(response.replace(
                "<TotalMatches>3</TotalMatches>",
                "<TotalMatches>1001</TotalMatches>",
            ))
        } else {
            Ok(response)
        }
    }
}

#[test]
fn oversized_or_unplayable_backups_cannot_be_captured_or_saved_as_partial_scenes() {
    let f = Fixture::new(2);
    let start = f.sim.soap_log().len();
    let error =
        scenes::capture_with_queue_backups(&Oversized(&f.lan), &f.houses, "Too large", &[], 1)
            .unwrap_err();
    assert!(error.to_string().contains("1000"), "{error}");
    assert!(f.writes_since(start).is_empty());
    let original = f.capture();
    for mutation in 0..4 {
        let mut scene = original.clone();
        let SceneSource::QueueBackup { backup } = &mut scene.groups[0].source else {
            panic!("missing backup")
        };
        match mutation {
            0 => backup.entries.resize(
                scenes::MAX_QUEUE_BACKUP_ENTRIES + 1,
                backup.entries[0].clone(),
            ),
            1 => backup.entries[0].metadata = "x".repeat(scenes::MAX_QUEUE_BACKUP_BYTES + 1),
            2 => backup.entries[1].metadata.clear(),
            _ => backup.reference.track = 4,
        }
        let mut store = MemStore::default();
        assert!(scenes::save(&mut store, &scene, 1).is_err());
        assert!(scenes::list(&store).unwrap().is_empty());
    }
}

#[test]
fn a_refused_commit_keeps_both_the_old_prefix_and_the_complete_staged_suffix() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let scene = f.capture();
        f.replace();
        f.sim
            .upnp_fault("Alpha", "RemoveTrackRangeFromQueue", 1028)
            .unwrap();
        let start = f.sim.soap_log().len();
        let report = f.apply(&f.lan, &scene);
        assert!(!report.is_complete(), "{report:?}");
        assert!(report.failed[0].1.contains("1028"), "{report:?}");
        assert_eq!(f.queue(), [OLD, DUPLICATE, DUPLICATE, LAST]);
        let writes = f.writes_since(start);
        assert_eq!(
            writes
                .iter()
                .filter(|name| name.as_str() == "RemoveTrackRangeFromQueue")
                .count(),
            1
        );
        assert!(
            !writes
                .iter()
                .any(|name| name == "Play" || name == "RemoveAllTracksFromQueue")
        );
    }
}

#[test]
fn another_controllers_seek_after_pause_or_inactive_queue_commit_is_not_adopted() {
    for generation in [1, 2] {
        for event in [Event::SeekAfterPause, Event::SeekAfterRemoval] {
            let f = Fixture::new(generation);
            let scene = f.capture();
            f.replace();
            soap::set_av_transport_uri(&f.lan, f.host, OLD, "").unwrap();
            soap::seek_position(&f.lan, f.host, 40).unwrap();
            soap::play(&f.lan, f.host).unwrap();
            let transport = Interleaved {
                fixture: &f,
                event,
                fired: Cell::new(false),
            };
            let start = f.sim.soap_log().len();
            let report = f.apply(&transport, &scene);
            assert!(transport.fired.get());
            assert!(!report.is_complete(), "{report:?}");
            assert_eq!(soap::get_media_info(&f.lan, f.host).unwrap().uri, OLD);
            assert_eq!(
                soap::get_position_info(&f.lan, f.host)
                    .unwrap()
                    .position_secs,
                Some(99)
            );
            let writes = f.writes_since(start);
            assert!(
                !writes
                    .iter()
                    .any(|name| name == "Play" || name == "SetAVTransportURI"),
                "{writes:?}"
            );
            if matches!(event, Event::SeekAfterPause) {
                assert_eq!(f.queue(), [OLD]);
                assert!(!writes.iter().any(|name| name == "AddURIToQueue"));
            } else {
                assert_eq!(f.queue(), [DUPLICATE, DUPLICATE, LAST]);
            }
        }
    }
}

#[test]
fn rebuilding_the_active_queue_confirms_the_native_prefix_removal_selection_change() {
    for generation in [1, 2] {
        let f = Fixture::new(generation);
        let scene = f.capture();
        soap::add_uri_to_queue(&f.lan, f.host, OLD, "", false).unwrap();
        let start = f.sim.soap_log().len();
        let report = f.apply(&f.lan, &scene);
        assert!(report.is_complete(), "{report:?}");
        f.restored(true);
        assert!(
            !f.writes_since(start)
                .iter()
                .any(|name| name == "RemoveAllTracksFromQueue")
        );
    }
}
