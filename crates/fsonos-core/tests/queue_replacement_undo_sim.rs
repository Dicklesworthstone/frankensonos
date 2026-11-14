//! A successful source replacement can restore the previous selected queue,
//! including after the durable action store is closed and reopened.

use fsonos_core::actions::{self, PendingQueueUndo, QueueUndo};
use fsonos_core::control::{self, QueueReplacement};
use fsonos_core::favorites::{self, FavoriteKind};
use fsonos_core::policy::Client;
use fsonos_core::store::{Action, MemStore, SqliteStore, Store};
use fsonos_core::{CoreError, HouseholdState, playlists};
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::SpotifyContainer;
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
const OTHER: &str = "x-file-cifs://example.invalid/another-owner.flac";
const TRACKS: [(&str, &str); 4] = [
    ("spotify:track:0000000000000000000001", "First"),
    ("spotify:track:0000000000000000000002", "Second"),
    ("spotify:track:0000000000000000000001", "First again"),
    ("spotify:track:0000000000000000000003", "Last"),
];
const EPISODES: [(&str, &str); 2] = [
    ("spotify:episode:0000000000000000000001", "First episode"),
    ("spotify:episode:0000000000000000000002", "Second episode"),
];

#[path = "queue_replacement_undo_sim/scenes.rs"]
mod scene_undo;

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

    fn capture(&self) -> PendingQueueUndo {
        actions::capture_queue_undo(&self.lan, &self.houses, &self.coordinator, 100).unwrap()
    }

    fn tracks<T: Transport + ?Sized>(&self, t: &T) -> Result<QueueReplacement, CoreError> {
        control::play_spotify_tracks_with_receipt(t, &self.houses, &self.coordinator, &TRACKS)
    }

    fn replace(&self, source: u8) -> QueueReplacement {
        match source {
            0 | 1 => control::play_spotify_container_with_receipt(
                &self.lan,
                &self.houses,
                &self.coordinator,
                if source == 0 {
                    SpotifyContainer::Album
                } else {
                    SpotifyContainer::Playlist
                },
                if source == 0 {
                    "spotify:album:0000000000000000000001"
                } else {
                    "spotify:playlist:0000000000000000000001"
                },
                "Replacement",
            )
            .unwrap(),
            2 => self.tracks(&self.lan).unwrap(),
            3 => {
                let favorite = favorites::list(&self.lan, &self.houses, &self.coordinator)
                    .unwrap()
                    .into_iter()
                    .find(|f| f.kind == FavoriteKind::Container)
                    .unwrap();
                favorites::play_with_receipt(&self.lan, &self.houses, &self.coordinator, &favorite)
                    .unwrap()
                    .unwrap()
            }
            4 => {
                let playlist = playlists::list(&self.lan, &self.houses, &self.coordinator)
                    .unwrap()
                    .remove(0);
                favorites::play_with_receipt(&self.lan, &self.houses, &self.coordinator, &playlist)
                    .unwrap()
                    .unwrap()
            }
            _ => control::play_spotify_tracks_with_receipt(
                &self.lan,
                &self.houses,
                &self.coordinator,
                &EPISODES,
            )
            .unwrap(),
        }
    }

    fn finish(&self, pending: PendingQueueUndo, receipt: &QueueReplacement) -> QueueUndo {
        actions::finish_queue_replacement(&self.lan, &self.houses, pending, receipt, 101).unwrap()
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

    fn undo<S: Store + ?Sized>(&self, store: &mut S, force: bool) -> Result<(), CoreError> {
        let report = actions::undo_last_vetted(
            &self.lan,
            &self.houses,
            store,
            None,
            &Client::Cli,
            "test",
            102,
            force,
            &|op| Ok(op.clone()),
        )?
        .unwrap();
        assert!(report.failures.is_empty(), "{report:?}");
        assert_eq!(report.zones.len(), 1);
        assert!(report.zones[0].1.skipped.is_empty(), "{report:?}");
        Ok(())
    }
}

fn record<S: Store + ?Sized>(store: &mut S, saved: QueueUndo) {
    let before = actions::with_queue_undo(None, &[saved]).unwrap();
    let parsed = actions::BeforeState::parse(&before).unwrap();
    assert_eq!(parsed.queue_undo.len(), 1);
    assert_eq!(parsed.zones.len(), 1);
    actions::record(
        store,
        &Action {
            at: 101,
            client: Client::Cli.key().into(),
            surface: "test".into(),
            intent: "replace the queue".into(),
            decision: "allow".into(),
            result: "successful replacement".into(),
            before_state: Some(before),
            undo_of: None,
        },
    )
    .unwrap();
}

#[test]
fn successful_sources_restore_the_original_duplicate_bookmark_mode_and_quiet_intent() {
    for generation in [1, 2] {
        for playing in [false, true] {
            for source in 0..6 {
                let f = Fixture::new(generation, playing);
                let pending = f.capture();
                let receipt = f.replace(source);
                if source == 2 {
                    assert_eq!(
                        f.queue().len(),
                        4,
                        "the replacement can exceed the old length"
                    );
                    assert_eq!(get_play_mode(&f.lan, f.host).unwrap(), PlayMode::Normal);
                }
                let saved = f.finish(pending, &receipt);
                let mut store = MemStore::default();
                record(&mut store, saved);
                let start = f.sim.soap_log().len();
                f.undo(&mut store, false).unwrap();
                f.restored(playing);
                let writes = f.writes_since(start);
                assert!(!writes.iter().any(|name| name == "RemoveAllTracksFromQueue"));
                assert_eq!(
                    writes
                        .iter()
                        .filter(|name| *name == "AddURIToQueue")
                        .count(),
                    3
                );
                assert_eq!(
                    writes
                        .iter()
                        .filter(|name| *name == "RemoveTrackRangeFromQueue")
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
fn replacement_backup_and_exact_result_survive_sqlite_close_and_reopen() {
    for generation in [1, 2] {
        let f = Fixture::new(generation, false);
        let pending = f.capture();
        let receipt = f.tracks(&f.lan).unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("replacement-actions.sqlite");
        let mut store = SqliteStore::open(&path).unwrap();
        record(&mut store, f.finish(pending, &receipt));
        store.close().unwrap();
        let mut reopened = SqliteStore::open(&path).unwrap();
        f.undo(&mut reopened, false).unwrap();
        f.restored(false);
        reopened.close().unwrap();
    }
}

struct RefuseRendererOnce<'a> {
    inner: &'a SimLan,
    refused: Cell<bool>,
    adds: Cell<usize>,
}

impl Transport for RefuseRendererOnce<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name == "AddURIToQueue" {
            self.adds.set(self.adds.get() + 1);
            if !self.refused.replace(true) {
                return Err(ProtoError::SoapFault {
                    code: 800,
                    reason: "injected stale renderer parameters".into(),
                });
            }
        }
        self.inner.soap_post(host, path, action, body)
    }
}

#[test]
fn enqueue_recovery_keeps_the_original_playback_intent_for_undo() {
    for generation in [1, 2] {
        for playing in [false, true] {
            for autoplay in [false, true] {
                let f = Fixture::new(generation, playing);
                let pending = f.capture();
                let replies = RefuseRendererOnce {
                    inner: &f.lan,
                    refused: Cell::new(false),
                    adds: Cell::new(0),
                };
                let replace = if autoplay {
                    control::play_spotify_container_with_receipt
                } else {
                    control::load_spotify_container_with_receipt
                };
                let receipt = replace(
                    &replies,
                    &f.houses,
                    &f.coordinator,
                    SpotifyContainer::Album,
                    "spotify:album:0000000000000000000001",
                    "Replacement",
                )
                .unwrap();
                assert_eq!(replies.adds.get(), 2, "one retry after the refused enqueue");
                let saved = f.finish(pending, &receipt);
                let mut store = MemStore::default();
                record(&mut store, saved);
                f.undo(&mut store, false).unwrap();
                f.restored(playing);
            }
        }
    }
}

#[test]
fn exact_policy_reductions_are_retained_and_unattributed_changes_refuse_completion() {
    for declared in [false, true] {
        let f = Fixture::new(2, false);
        let mut pending = f.capture();
        soap::set_volume(&f.lan, f.host, 15).unwrap();
        if declared {
            pending = pending.volume_caps(&[(f.coordinator.clone(), 15)]).unwrap();
            pending.confirm(&f.lan, &f.houses, 100).unwrap();
        } else {
            assert!(pending.confirm(&f.lan, &f.houses, 100).is_err());
        }
        let receipt = f.tracks(&f.lan).unwrap();
        let saved = actions::finish_queue_replacement(&f.lan, &f.houses, pending, &receipt, 101);
        if declared {
            let mut store = MemStore::default();
            record(&mut store, saved.unwrap());
            f.undo(&mut store, false).unwrap();
            f.restored(false);
            assert_eq!(soap::get_volume(&f.lan, f.host).unwrap(), 15);
        } else {
            assert!(saved.is_err());
        }
    }
    let f = Fixture::new(2, false);
    for caps in [
        vec![(f.coordinator.clone(), 36)],
        vec![(PlayerId("RINCON_UNKNOWN".into()), 10)],
        vec![(f.coordinator.clone(), 10), (f.coordinator.clone(), 9)],
    ] {
        assert!(f.capture().volume_caps(&caps).is_err());
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
        _ => soap::set_volume(&f.lan, f.host, 40).unwrap(),
    }
}

#[test]
fn force_never_restores_over_later_queue_playback_group_boot_or_level_changes() {
    for change in 0..7 {
        let f = Fixture::new(2, false);
        let pending = f.capture();
        let receipt = f.tracks(&f.lan).unwrap();
        let mut store = MemStore::default();
        record(&mut store, f.finish(pending, &receipt));
        owner_change(&f, change);
        let start = f.sim.soap_log().len();
        assert!(f.undo(&mut store, true).is_err(), "change {change}");
        assert!(f.writes_since(start).is_empty(), "change {change}");
    }
}

#[test]
fn changed_baseline_and_changes_before_logging_cannot_claim_a_replacement() {
    for before_execution in [false, true] {
        for change in [0, 1, 3] {
            let f = Fixture::new(2, false);
            let pending = f.capture();
            if before_execution {
                owner_change(&f, change);
            }
            let receipt = f.tracks(&f.lan).unwrap();
            if !before_execution {
                owner_change(&f, change);
            }
            let start = f.sim.soap_log().len();
            assert!(
                actions::finish_queue_replacement(&f.lan, &f.houses, pending, &receipt, 101,)
                    .is_err()
            );
            assert!(f.writes_since(start).is_empty());
        }
    }
}

#[derive(Clone, Copy)]
enum Fault {
    LostRemoval,
    LostPlay,
    RefusedPlay,
    UnreadableOwner,
    SeekDuringAppend,
}

struct Interrupted {
    lan: SimLan,
    fault: Cell<Option<Fault>>,
}

impl Transport for Interrupted {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        let fault = self.fault.get();
        let applies = matches!(
            (fault, name),
            (Some(Fault::LostRemoval), "RemoveTrackRangeFromQueue")
                | (Some(Fault::LostPlay | Fault::RefusedPlay), "Play")
                | (Some(Fault::UnreadableOwner), "GetPositionInfo")
                | (Some(Fault::SeekDuringAppend), "AddURIToQueue")
        );
        if !applies {
            return self.lan.soap_post(host, path, action, body);
        }
        self.fault.set(None);
        if matches!(fault, Some(Fault::RefusedPlay)) {
            return Err(ProtoError::SoapFault {
                code: 501,
                reason: "injected Play refusal".into(),
            });
        }
        if matches!(fault, Some(Fault::SeekDuringAppend)) {
            soap::seek_position(&self.lan, host, 99)?;
            return self.lan.soap_post(host, path, action, body);
        }
        if !matches!(fault, Some(Fault::UnreadableOwner)) {
            self.lan.soap_post(host, path, action, body)?;
        }
        Err(ProtoError::Network {
            target: host.to_string(),
            detail: "injected lost reply".into(),
        })
    }
}

#[test]
fn confirmed_lost_replies_keep_receipts_without_repeating_removal_or_play() {
    for generation in [1, 2] {
        for fault in [Fault::LostRemoval, Fault::LostPlay] {
            let f = Fixture::new(generation, false);
            let pending = f.capture();
            let t = Interrupted {
                lan: f.sim.lan(),
                fault: Cell::new(Some(fault)),
            };
            let start = f.sim.soap_log().len();
            let receipt = f.tracks(&t).unwrap();
            let writes = f.writes_since(start);
            assert_eq!(
                writes
                    .iter()
                    .filter(|name| *name == "RemoveTrackRangeFromQueue")
                    .count(),
                1
            );
            assert_eq!(writes.iter().filter(|name| *name == "Play").count(), 1);
            let mut store = MemStore::default();
            record(&mut store, f.finish(pending, &receipt));
            f.undo(&mut store, false).unwrap();
            f.restored(false);
        }
    }
}

#[test]
fn failed_or_unowned_replacements_never_produce_undo_from_a_later_arbitrary_read() {
    for fault in [
        Fault::RefusedPlay,
        Fault::UnreadableOwner,
        Fault::SeekDuringAppend,
    ] {
        let f = Fixture::new(2, false);
        let pending = f.capture();
        let t = Interrupted {
            lan: f.sim.lan(),
            fault: Cell::new(Some(fault)),
        };
        let receipt = f.tracks(&t);
        if matches!(fault, Fault::RefusedPlay) {
            assert!(receipt.is_err());
        } else {
            assert!(
                actions::finish_queue_replacement(
                    &f.lan,
                    &f.houses,
                    pending,
                    &receipt.unwrap(),
                    101,
                )
                .is_err()
            );
            assert_eq!(
                soap::get_transport_info(&f.lan, f.host).unwrap().state,
                TransportState::Playing,
                "missing undo evidence must not stop successful Play"
            );
        }
    }
}
