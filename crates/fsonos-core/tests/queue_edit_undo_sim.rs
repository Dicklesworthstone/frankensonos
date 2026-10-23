//! Durable queue edit undo over real localhost SOAP, including restarted
//! action storage and controller changes at reconstruction write boundaries.

use fsonos_core::actions::{self, QueueUndo, UndoReport};
use fsonos_core::control::queue_removal;
use fsonos_core::policy::Client;
use fsonos_core::snapshot::{Aspect, RestoreOp};
use fsonos_core::store::{Action, MemStore, SqliteStore, Store};
use fsonos_core::{CoreError, HouseholdState};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::queue::reorder_tracks;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::{HouseholdId, PlayMode, PlayerId, TransportState};
use std::cell::Cell;
use std::net::IpAddr;
use std::time::Duration;

const DUPLICATE: &str = "x-file-cifs://example.invalid/repeated.flac";
const LAST: &str = "x-file-cifs://example.invalid/last.flac";
const OTHER: &str = "x-file-cifs://example.invalid/other.flac";

struct Fixture {
    sim: SimHandle,
    lan: SimLan,
    houses: Vec<HouseholdState>,
    coordinator: PlayerId,
    host: IpAddr,
}

impl Fixture {
    fn new(generation: u8, playing: bool) -> Self {
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
        soap::set_volume(&lan, host, 35).unwrap();
        soap::set_volume(&lan, sim.player("Beta").unwrap().ip, 25).unwrap();
        for uri in [DUPLICATE, DUPLICATE, LAST] {
            soap::add_uri_to_queue(&lan, host, uri, "", false).unwrap();
        }
        soap::play_from_queue(&lan, host, &coordinator).unwrap();
        soap::seek_track(&lan, host, 2).unwrap();
        soap::seek_position(&lan, host, 17).unwrap();
        set_play_mode(&lan, host, PlayMode::RepeatAll).unwrap();
        soap::play(&lan, host).unwrap();
        if !playing {
            soap::pause(&lan, host).unwrap();
        }
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

    fn remove(&self, first: u32, count: u32) -> QueueUndo {
        let before = queue_removal::read(&self.lan, self.host).unwrap();
        let pending =
            actions::capture_queue_undo(&self.lan, &self.houses, &self.coordinator, 100).unwrap();
        assert!(pending.matches_before(before.revision, &before.uris));
        let pending = pending.removal(first, count).unwrap();
        let mut expected = before.uris.clone();
        let start = usize::try_from(first - 1).unwrap();
        expected.drain(start..start + usize::try_from(count).unwrap());
        let done = queue_removal::remove(
            &self.lan,
            self.host,
            &self.coordinator,
            &before,
            first,
            count,
        )
        .unwrap();
        actions::finish_queue_undo(
            &self.lan,
            &self.houses,
            pending,
            done.revision,
            &expected,
            101,
        )
        .unwrap()
    }

    fn movement(&self) -> QueueUndo {
        let pending = actions::capture_queue_undo(&self.lan, &self.houses, &self.coordinator, 100)
            .unwrap()
            .movement(2, 1, 4)
            .unwrap();
        reorder_tracks(&self.lan, self.host, 2, 1, 4, pending.revision()).unwrap();
        let after = queue_removal::read(&self.lan, self.host).unwrap();
        actions::finish_queue_undo(
            &self.lan,
            &self.houses,
            pending,
            after.revision,
            &after.uris,
            101,
        )
        .unwrap()
    }

    fn undo<T: Transport + ?Sized, S: Store + ?Sized>(
        &self,
        transport: &T,
        store: &mut S,
        force: bool,
        vet: &dyn Fn(&RestoreOp) -> Result<RestoreOp, String>,
    ) -> Result<Option<UndoReport>, CoreError> {
        actions::undo_last_vetted(
            transport,
            &self.houses,
            store,
            Some(&Client::Cli),
            &Client::Cli,
            "test",
            103,
            force,
            vet,
        )
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

fn record<S: Store + ?Sized>(store: &mut S, saved: QueueUndo) -> i64 {
    let before = actions::with_queue_undo(None, &[saved]).unwrap();
    let parsed = actions::BeforeState::parse(&before).unwrap();
    assert_eq!(parsed.queue_undo.len(), 1);
    assert_eq!(
        parsed.zones.len(),
        1,
        "ordinary previews retain the affected zone"
    );
    actions::record(
        store,
        &Action {
            at: 101,
            client: Client::Cli.key().into(),
            surface: "test".into(),
            intent: "queue edit".into(),
            decision: "allow".into(),
            result: "confirmed native edit".into(),
            before_state: Some(before),
            undo_of: None,
        },
    )
    .unwrap()
}

fn complete(report: &UndoReport) {
    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!(report.zones.len(), 1);
    assert!(report.zones[0].1.skipped.is_empty(), "{report:?}");
}

#[test]
fn confirmed_removals_and_moves_restore_duplicate_occurrence_clock_mode_and_quiet_intent() {
    for generation in [1, 2] {
        for playing in [false, true] {
            for edit in 0..3 {
                let f = Fixture::new(generation, playing);
                let saved = match edit {
                    0 => f.remove(1, 1),
                    1 => f.remove(2, 1),
                    _ => f.movement(),
                };
                let mut store = MemStore::default();
                let id = record(&mut store, saved);
                let start = f.sim.soap_log().len();
                let report = f
                    .undo(&f.lan, &mut store, false, &|op| Ok(op.clone()))
                    .unwrap()
                    .unwrap();
                assert_eq!(report.undone, id);
                complete(&report);
                assert!(!report.zones[0].1.restored.contains(&Aspect::Volume));
                assert!(!report.zones[0].1.restored.contains(&Aspect::Mute));
                f.restored(playing);
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
                if !playing {
                    assert!(!writes.iter().any(|name| name == "Play"));
                }
            }
        }
    }
}

#[test]
fn naturally_advancing_playback_still_restores_the_original_bookmark() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, true);
        let mut store = MemStore::default();
        record(&mut store, f.remove(1, 1));
        f.sim.advance(Duration::from_secs(2));
        assert_eq!(
            soap::get_position_info(&f.lan, f.host)
                .unwrap()
                .position_secs,
            Some(19)
        );
        complete(
            &f.undo(&f.lan, &mut store, false, &|op| Ok(op.clone()))
                .unwrap()
                .unwrap(),
        );
        f.restored(true);
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
fn oversized_inactive_and_empty_queues_cannot_produce_partial_undo_backups() {
    let f = Fixture::new(2, false);
    let start = f.sim.soap_log().len();
    let error = actions::capture_queue_undo(&Oversized(&f.lan), &f.houses, &f.coordinator, 100)
        .unwrap_err();
    assert!(error.to_string().contains("1000"), "{error}");
    assert!(f.writes_since(start).is_empty());
    soap::set_av_transport_uri(&f.lan, f.host, OTHER, "").unwrap();
    let start = f.sim.soap_log().len();
    assert!(actions::capture_queue_undo(&f.lan, &f.houses, &f.coordinator, 100).is_err());
    assert!(f.writes_since(start).is_empty());
    soap::remove_all_tracks_from_queue(&f.lan, f.host).unwrap();
    soap::play_from_queue(&f.lan, f.host, &f.coordinator).unwrap();
    let start = f.sim.soap_log().len();
    assert!(actions::capture_queue_undo(&f.lan, &f.houses, &f.coordinator, 100).is_err());
    assert!(f.writes_since(start).is_empty());
}

#[test]
fn deleting_every_track_can_restore_the_previously_selected_queue() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        let saved = f.remove(1, 3);
        assert!(f.queue().is_empty());
        let mut store = MemStore::default();
        record(&mut store, saved);
        let start = f.sim.soap_log().len();
        complete(
            &f.undo(&f.lan, &mut store, false, &|op| Ok(op.clone()))
                .unwrap()
                .unwrap(),
        );
        f.restored(false);
        assert!(
            !f.writes_since(start)
                .iter()
                .any(|name| name == "Play" || name == "RemoveAllTracksFromQueue")
        );
    }
}

#[test]
fn queue_undo_contents_and_result_proof_survive_sqlite_close_and_reopen() {
    for moving in [false, true] {
        let f = Fixture::new(2, false);
        let saved = if moving { f.movement() } else { f.remove(1, 1) };
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("queue-actions.sqlite");
        let mut store = SqliteStore::open(&path).unwrap();
        let id = record(&mut store, saved);
        store.close().unwrap();
        let mut reopened = SqliteStore::open(&path).unwrap();
        let report = f
            .undo(&f.lan, &mut reopened, false, &|op| Ok(op.clone()))
            .unwrap()
            .unwrap();
        assert_eq!(report.undone, id);
        complete(&report);
        f.restored(false);
        reopened.close().unwrap();
    }
}

#[test]
fn completion_requires_the_bound_edit_and_its_exact_confirmed_result_and_playback() {
    for change in 0..4 {
        let f = Fixture::new(2, true);
        let pending = actions::capture_queue_undo(&f.lan, &f.houses, &f.coordinator, 100).unwrap();
        assert!(!pending.matches_before(pending.revision() + 1, pending.uris()));
        let pending = pending.movement(2, 1, 4).unwrap();
        reorder_tracks(&f.lan, f.host, 2, 1, 4, pending.revision()).unwrap();
        let after = queue_removal::read(&f.lan, f.host).unwrap();
        let mut revision = after.revision;
        let mut uris = after.uris;
        match change {
            0 => revision += 1,
            1 => uris.swap(0, 1),
            2 => soap::seek_position(&f.lan, f.host, 99).unwrap(),
            _ => soap::pause(&f.lan, f.host).unwrap(),
        }
        let start = f.sim.soap_log().len();
        assert!(
            actions::finish_queue_undo(&f.lan, &f.houses, pending, revision, &uris, 101).is_err()
        );
        assert!(f.writes_since(start).is_empty());
    }
}

fn owner_change(f: &Fixture, change: u8) {
    match change {
        0 => {
            soap::add_uri_to_queue(&f.lan, f.host, OTHER, "", false).unwrap();
        }
        1 => soap::seek_position(&f.lan, f.host, 99).unwrap(),
        2 => soap::pause(&f.lan, f.host).unwrap(),
        3 => set_play_mode(&f.lan, f.host, PlayMode::RepeatOne).unwrap(),
        4 => {
            soap::leave_group(
                &f.lan,
                f.sim.player("Beta").unwrap().ip,
                &PlayerId(f.sim.player("Beta").unwrap().uuid.clone()),
            )
            .unwrap();
        }
        5 => f.sim.reboot("Alpha", Duration::ZERO).unwrap(),
        6 => f.sim.reboot("Beta", Duration::ZERO).unwrap(),
        7 => {
            soap::add_uri_to_queue(&f.lan, f.host, OTHER, "", false).unwrap();
            let current = queue_removal::read(&f.lan, f.host).unwrap();
            queue_removal::remove(&f.lan, f.host, &f.coordinator, &current, 3, 1).unwrap();
            assert_eq!(
                f.queue(),
                [DUPLICATE, LAST],
                "identical order at a newer revision"
            );
        }
        _ => soap::set_av_transport_uri(&f.lan, f.host, OTHER, "").unwrap(),
    }
}

#[test]
fn changed_queue_source_selection_mode_group_or_any_boot_refuses_even_forced_undo() {
    for change in 0..9 {
        let f = Fixture::new(2, true);
        let saved = f.remove(1, 1);
        let mut store = MemStore::default();
        record(&mut store, saved);
        owner_change(&f, change);
        for force in [false, true] {
            let start = f.sim.soap_log().len();
            assert!(
                f.undo(&f.lan, &mut store, force, &|op| Ok(op.clone()))
                    .is_err()
            );
            assert!(f.writes_since(start).is_empty());
        }
    }
}

#[derive(Clone, Copy)]
enum Event {
    LostAppend,
    BadAppend,
    LostRemoval,
    BadRemoval,
    SeekAfterPause,
    SeekAfterAppend,
    SeekAfterCap,
    RefusedAppend,
    RefusedRemoval,
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
        let trigger = match self.event {
            Event::LostRemoval | Event::BadRemoval | Event::RefusedRemoval => {
                name == "RemoveTrackRangeFromQueue"
            }
            Event::SeekAfterPause => name == "Pause",
            Event::SeekAfterCap => name == "SetVolume",
            _ => name == "AddURIToQueue",
        };
        if host != self.fixture.host || !trigger || self.fired.replace(true) {
            return self.fixture.lan.soap_post(host, path, action, body);
        }
        if matches!(self.event, Event::RefusedAppend | Event::RefusedRemoval) {
            return Err(ProtoError::SoapFault {
                code: 800,
                reason: "injected definite refusal".into(),
            });
        }
        let result = self.fixture.lan.soap_post(host, path, action, body);
        match self.event {
            Event::LostAppend | Event::LostRemoval => Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected lost reply".into(),
            }),
            Event::BadAppend | Event::BadRemoval => Ok("<truncated".into()),
            Event::SeekAfterPause | Event::SeekAfterAppend | Event::SeekAfterCap => {
                soap::seek_position(&self.fixture.lan, host, 99)?;
                result
            }
            Event::RefusedAppend | Event::RefusedRemoval => unreachable!(),
        }
    }
}

#[test]
fn reconstruction_proves_lost_and_malformed_replies_without_repeating_writes() {
    for generation in [1, 2] {
        for event in [
            Event::LostAppend,
            Event::BadAppend,
            Event::LostRemoval,
            Event::BadRemoval,
        ] {
            let f = Fixture::new(generation, true);
            let mut store = MemStore::default();
            record(&mut store, f.remove(1, 1));
            let t = Interleaved {
                fixture: &f,
                event,
                fired: Cell::new(false),
            };
            let start = f.sim.soap_log().len();
            complete(
                &f.undo(&t, &mut store, false, &|op| Ok(op.clone()))
                    .unwrap()
                    .unwrap(),
            );
            assert!(t.fired.get());
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

/// Change only the native revision after removal's complete read-back, at
/// its final coordinator read. Identical URI contents cannot transfer the
/// confirmed result's ownership to another controller's newer queue.
struct RevisionAfterCommit<'a> {
    fixture: &'a Fixture,
    removed: Cell<bool>,
    injected: Cell<bool>,
}

impl Transport for RevisionAfterCommit<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.fixture.lan.soap_post(host, path, action, body)?;
        if host == self.fixture.host {
            if action.contains("#RemoveTrackRangeFromQueue") {
                self.removed.set(true);
            }
            if self.removed.get()
                && action.contains("#GetZoneGroupState")
                && !self.injected.replace(true)
            {
                soap::add_uri_to_queue(&self.fixture.lan, host, OTHER, "", false)?;
                let after = queue_removal::read(&self.fixture.lan, host)?;
                soap::remove_track_range_from_queue(&self.fixture.lan, host, after.revision, 4, 1)?;
            }
        }
        Ok(response)
    }
}

#[test]
fn an_identical_queue_at_a_new_revision_after_commit_cannot_authorize_playback() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, true);
        let mut store = MemStore::default();
        record(&mut store, f.remove(1, 1));
        let transport = RevisionAfterCommit {
            fixture: &f,
            removed: Cell::new(false),
            injected: Cell::new(false),
        };
        let start = f.sim.soap_log().len();
        let report = f
            .undo(&transport, &mut store, false, &|op| Ok(op.clone()))
            .unwrap()
            .unwrap();
        assert!(transport.injected.get());
        assert!(
            report
                .failures
                .iter()
                .any(|(_, error)| error.contains("confirmed revision")),
            "{report:?}"
        );
        assert_eq!(f.queue(), [DUPLICATE, DUPLICATE, LAST]);
        assert!(
            !f.writes_since(start)
                .iter()
                .any(|name| name == "Play" || name == "SetAVTransportURI")
        );
        assert_eq!(soap::get_position_info(&f.lan, f.host).unwrap().track, 1);
    }
}

#[test]
fn failed_reconstruction_never_falls_through_to_the_old_snapshot_or_play() {
    for event in [
        Event::SeekAfterPause,
        Event::SeekAfterAppend,
        Event::RefusedAppend,
        Event::RefusedRemoval,
    ] {
        let f = Fixture::new(2, true);
        let mut store = MemStore::default();
        record(&mut store, f.remove(1, 1));
        let t = Interleaved {
            fixture: &f,
            event,
            fired: Cell::new(false),
        };
        let start = f.sim.soap_log().len();
        let report = f
            .undo(&t, &mut store, false, &|op| Ok(op.clone()))
            .unwrap()
            .unwrap();
        assert!(t.fired.get());
        assert!(!report.failures.is_empty(), "{report:?}");
        assert_eq!(&f.queue()[..2], [DUPLICATE, LAST]);
        let writes = f.writes_since(start);
        assert!(
            !writes.iter().any(|name| name == "Play"
                || name == "SetAVTransportURI"
                || name == "RemoveAllTracksFromQueue"),
            "{writes:?}"
        );
        if matches!(event, Event::SeekAfterPause | Event::SeekAfterAppend) {
            assert_eq!(
                soap::get_position_info(&f.lan, f.host)
                    .unwrap()
                    .position_secs,
                Some(99)
            );
            assert!(
                !writes
                    .iter()
                    .any(|name| name == "RemoveTrackRangeFromQueue")
            );
        }
    }
}

#[test]
fn policy_caps_are_applied_before_reconstruction_and_remain_part_of_the_owner_proof() {
    for seek_after_cap in [false, true] {
        let f = Fixture::new(2, true);
        let mut store = MemStore::default();
        record(&mut store, f.remove(1, 1));
        let t = Interleaved {
            fixture: &f,
            event: Event::SeekAfterCap,
            fired: Cell::new(!seek_after_cap),
        };
        let start = f.sim.soap_log().len();
        let report = f
            .undo(&t, &mut store, false, &|op| {
                Ok(match op {
                    RestoreOp::SetVolume { player, level } => RestoreOp::SetVolume {
                        player: player.clone(),
                        level: (*level).min(10),
                    },
                    other => other.clone(),
                })
            })
            .unwrap()
            .unwrap();
        let writes = f.writes_since(start);
        assert_eq!(writes[0], "SetVolume");
        if seek_after_cap {
            assert!(!report.failures.is_empty());
            assert!(
                !writes
                    .iter()
                    .any(|name| name == "AddURIToQueue" || name == "Play")
            );
            assert_eq!(
                soap::get_position_info(&f.lan, f.host)
                    .unwrap()
                    .position_secs,
                Some(99)
            );
        } else {
            complete(&report);
            assert!(report.zones[0].1.restored.contains(&Aspect::Volume));
            assert!(!report.zones[0].1.restored.contains(&Aspect::Mute));
            assert_eq!(writes[1], "SetVolume");
            f.restored(true);
            assert_eq!(soap::get_volume(&f.lan, f.host).unwrap(), 10);
            assert_eq!(
                soap::get_volume(&f.lan, f.sim.player("Beta").unwrap().ip).unwrap(),
                10
            );
        }
    }
}

#[test]
fn denied_queue_permission_or_volume_preflight_prevents_every_reconstruction_write() {
    for deny_volume in [false, true] {
        let f = Fixture::new(2, true);
        let mut store = MemStore::default();
        record(&mut store, f.remove(1, 1));
        let start = f.sim.soap_log().len();
        let report = f
            .undo(&f.lan, &mut store, false, &|op| {
                if (!deny_volume && matches!(op, RestoreOp::RestoreQueue { .. }))
                    || (deny_volume && matches!(op, RestoreOp::SetVolume { .. }))
                {
                    Err("denied for this caller".into())
                } else {
                    Ok(op.clone())
                }
            })
            .unwrap()
            .unwrap();
        assert!(!report.failures.is_empty());
        assert!(f.writes_since(start).is_empty());
    }
}
