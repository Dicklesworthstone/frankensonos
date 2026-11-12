//! Scene receipts restore complete queues and only the scene's level fields.
//! All speaker reads and writes use the real S1/S2 localhost simulator.

use super::*;
use fsonos_core::actions::PendingSceneQueueUndo;
use fsonos_core::fade::Fader;
use fsonos_core::scenes::{
    self, ApplyOptions, SceneHooks, SceneOp, ScenePlan, SceneReport, SonosPlaylistReference,
};
use fsonos_core::snapshot::RestoreOp;
use std::cell::RefCell;

fn fixture(generation: u8, playing: bool) -> Fixture {
    let f = Fixture::new(generation, playing);
    soap::set_mute(&f.lan, f.host, false).unwrap();
    let beta = f.sim.player("Beta").unwrap().ip;
    soap::set_volume(&f.lan, beta, 12).unwrap();
    soap::set_mute(&f.lan, beta, true).unwrap();
    f
}

fn plan(f: &Fixture, source: u8, start: bool) -> ScenePlan {
    let uri = |uri: &str| SceneOp::Uri {
        coordinator: f.coordinator.clone(),
        uri: uri.into(),
        metadata: String::new(),
        start,
    };
    let source = match source {
        0 => uri("spotify:album:0000000000000000000001"),
        1 => uri("spotify:playlist:0000000000000000000001"),
        2 => {
            let favorite = favorites::list(&f.lan, &f.houses, &f.coordinator)
                .unwrap()
                .into_iter()
                .find(|f| f.kind == FavoriteKind::Container)
                .unwrap();
            SceneOp::Favorite {
                coordinator: f.coordinator.clone(),
                name: favorite.title,
                start,
            }
        }
        3 => {
            let playlist = playlists::list(&f.lan, &f.houses, &f.coordinator)
                .unwrap()
                .remove(0);
            SceneOp::SonosPlaylist {
                coordinator: f.coordinator.clone(),
                playlist: SonosPlaylistReference::capture(
                    &f.lan,
                    &f.houses,
                    &f.coordinator,
                    &playlist.id,
                )
                .unwrap(),
                start,
            }
        }
        4 => uri("spotify:artist:0000000000000000000001"),
        _ => uri("spotify:show:0000000000000000000001"),
    };
    let beta = PlayerId(f.sim.player("Beta").unwrap().uuid.clone());
    ScenePlan {
        ops: vec![
            SceneOp::Volume {
                room: "Alpha".into(),
                player: f.coordinator.clone(),
                level: 21,
            },
            SceneOp::Volume {
                room: "Beta".into(),
                player: beta.clone(),
                level: 9,
            },
            SceneOp::Mute {
                room: "Alpha".into(),
                player: f.coordinator.clone(),
                mute: true,
            },
            SceneOp::Mute {
                room: "Beta".into(),
                player: beta,
                mute: false,
            },
            source,
        ],
        notes: Vec::new(),
    }
}

struct Receipts<'a> {
    f: &'a Fixture,
    takeover: Option<&'a Fader>,
    attempts: RefCell<Vec<PlayerId>>,
    receipts: RefCell<Vec<QueueReplacement>>,
}

impl<'a> Receipts<'a> {
    fn new(f: &'a Fixture) -> Self {
        Self {
            f,
            takeover: None,
            attempts: RefCell::new(Vec::new()),
            receipts: RefCell::new(Vec::new()),
        }
    }
}

impl SceneHooks for Receipts<'_> {
    fn queue_replacement_started(&self, coordinator: &PlayerId) -> bool {
        self.attempts.borrow_mut().push(coordinator.clone());
        true
    }

    fn queue_replacement_finished(&self, _: &PlayerId, receipt: QueueReplacement) {
        self.receipts.borrow_mut().push(receipt);
        if let Some(fader) = self.takeover {
            fader
                .set_volume(&self.f.lan, &self.f.houses, &self.f.coordinator, 21)
                .unwrap();
        }
    }

    fn load_catalog_source(
        &self,
        households: &[HouseholdState],
        coordinator: &PlayerId,
        uri: &str,
        start: bool,
    ) -> Result<(), String> {
        self.queue_replacement_started(coordinator);
        // The surface supplies these concrete, read-only catalog selections;
        // this core fixture exercises their real guarded queue dispatch.
        let tracks: &[(&str, &str)] = if uri.starts_with("spotify:show:") {
            &EPISODES
        } else {
            &TRACKS
        };
        let destination =
            control::TrackBatchDestination::read(&self.f.lan, self.f.host, coordinator)
                .map_err(|error| error.to_string())?;
        let receipt = if start {
            control::play_spotify_tracks_bound_with_receipt(
                &self.f.lan,
                households,
                coordinator,
                tracks,
                &destination,
            )
        } else {
            control::load_spotify_tracks_bound_with_receipt(
                &self.f.lan,
                households,
                coordinator,
                tracks,
                &destination,
            )
        }
        .map_err(|error| error.to_string())?;
        self.queue_replacement_finished(coordinator, receipt);
        Ok(())
    }

    fn start_dj(&self, _: &PlayerId, _: Option<&str>) -> Result<(), String> {
        Err("no DJ in this queue replacement fixture".into())
    }
}

fn apply(
    f: &Fixture,
    plan: &ScenePlan,
    fade: bool,
) -> (PendingSceneQueueUndo, QueueReplacement, SceneReport) {
    let pending =
        actions::capture_scene_queue_undo(&f.lan, &f.houses, &f.coordinator, plan, 100).unwrap();
    let hooks = Receipts::new(f);
    let fader = Fader::default();
    let report = scenes::apply(
        &f.lan,
        &f.houses,
        plan,
        &ApplyOptions {
            fade: fade.then_some((&fader, Duration::from_millis(1))),
            hooks: &hooks,
        },
    );
    assert!(report.is_complete(), "{report:?}");
    assert_eq!(*hooks.attempts.borrow(), [f.coordinator.clone()]);
    let mut receipts = hooks.receipts.take();
    assert_eq!(receipts.len(), 1);
    (pending, receipts.remove(0), report)
}

fn finish(
    f: &Fixture,
    applied: (PendingSceneQueueUndo, QueueReplacement, SceneReport),
) -> QueueUndo {
    let (pending, receipt, report) = applied;
    actions::finish_scene_queue_replacement(&f.lan, &f.houses, pending, &receipt, &report, 101)
        .unwrap()
}

/// Every restoration level write must address an inactive original queue.
/// Optionally make a real concurrent queue edit after the first such volume
/// write, proving that the later unmute and resume cannot follow it.
struct QuietLevels {
    lan: SimLan,
    host: IpAddr,
    level_writes: Cell<usize>,
    interfere: bool,
    changed: Cell<bool>,
}

impl QuietLevels {
    fn new(f: &Fixture, interfere: bool) -> Self {
        Self {
            lan: f.lan.clone(),
            host: f.host,
            level_writes: Cell::new(0),
            interfere,
            changed: Cell::new(false),
        }
    }
}

impl Transport for QuietLevels {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if matches!(name, "SetVolume" | "SetMute") {
            assert!(
                matches!(
                    soap::get_transport_info(&self.lan, self.host)?.state,
                    TransportState::Paused | TransportState::Stopped
                ),
                "scene music must not sound at restored levels"
            );
            let uris: Vec<_> = browse_snapshot(&self.lan, self.host, QUEUE)?
                .objects
                .into_iter()
                .map(|item| item.res.unwrap().uri)
                .collect();
            assert_eq!(uris, [DUPLICATE, DUPLICATE, LAST]);
            self.level_writes.set(self.level_writes.get() + 1);
        }
        let response = self.lan.soap_post(host, path, action, body)?;
        if self.interfere && name == "SetVolume" && !self.changed.replace(true) {
            soap::add_uri_to_queue(&self.lan, self.host, OTHER, "", false)?;
        }
        Ok(response)
    }
}

#[test]
fn scene_sources_restore_both_queue_and_changed_levels_without_sounding_the_replacement() {
    for generation in [1, 2] {
        for start in [false, true] {
            for source in 0..6 {
                let f = fixture(generation, !start);
                let plan = plan(&f, source, start);
                let before = f.sim.soap_log().len();
                let saved = finish(&f, apply(&f, &plan, false));
                assert_eq!(serde_json::to_value(&saved).unwrap()["version"], 3);
                if !start {
                    assert!(!f.writes_since(before).iter().any(|write| write == "Play"));
                }
                let mut store = MemStore::default();
                record(&mut store, saved);
                let transport = QuietLevels::new(&f, false);
                let before = f.sim.soap_log().len();
                let report = actions::undo_last(
                    &transport,
                    &f.houses,
                    &mut store,
                    None,
                    &Client::Cli,
                    "test",
                    102,
                )
                .unwrap()
                .unwrap();
                assert!(report.failures.is_empty(), "{report:?}");
                assert_eq!(transport.level_writes.get(), 4);
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
                    usize::from(!start)
                );
            }
        }
    }
}

#[test]
fn a_faded_scene_survives_sqlite_reopen_and_keeps_the_original_queue_quiet() {
    for generation in [1, 2] {
        let f = fixture(generation, false);
        let saved = finish(&f, apply(&f, &plan(&f, 3, true), true));
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("scene-undo.sqlite");
        let mut store = SqliteStore::open(&path).unwrap();
        record(&mut store, saved);
        store.close().unwrap();
        let mut reopened = SqliteStore::open(&path).unwrap();
        f.undo(&mut reopened, false).unwrap();
        f.restored(false);
        assert_eq!(soap::get_volume(&f.lan, f.host).unwrap(), 35);
        assert!(!soap::get_mute(&f.lan, f.host).unwrap());
        reopened.close().unwrap();
    }
}

#[test]
fn scene_undo_applies_current_caps_and_does_not_restore_unmentioned_level_fields() {
    let f = fixture(2, false);
    // This is the acknowledged pre-apply policy reduction, captured as the
    // scene's baseline. Undo must never regain the earlier volume of 35.
    soap::set_volume(&f.lan, f.host, 15).unwrap();
    let mut plan = plan(&f, 0, true);
    plan.ops.retain(|op| !matches!(op, SceneOp::Mute { .. }));
    let saved = finish(&f, apply(&f, &plan, false));
    let mut store = MemStore::default();
    record(&mut store, saved);
    let before = f.sim.soap_log().len();
    let report = actions::undo_last_vetted(
        &f.lan,
        &f.houses,
        &mut store,
        None,
        &Client::Cli,
        "test",
        102,
        false,
        &|op| {
            Ok(match op {
                RestoreOp::SetVolume { player, level } => RestoreOp::SetVolume {
                    player: player.clone(),
                    level: (*level).min(8),
                },
                _ => op.clone(),
            })
        },
    )
    .unwrap()
    .unwrap();
    assert!(report.failures.is_empty(), "{report:?}");
    assert_eq!(soap::get_volume(&f.lan, f.host).unwrap(), 8);
    assert_eq!(
        soap::get_volume(&f.lan, f.sim.player("Beta").unwrap().ip).unwrap(),
        8
    );
    assert!(
        !f.writes_since(before)
            .iter()
            .any(|write| write == "SetMute")
    );
    f.restored(false);
}

#[test]
fn an_owner_queue_edit_during_quiet_level_restoration_prevents_unmute_and_resume() {
    let f = fixture(2, true);
    let saved = finish(&f, apply(&f, &plan(&f, 1, true), false));
    let mut store = MemStore::default();
    record(&mut store, saved);
    let transport = QuietLevels::new(&f, true);
    let before = f.sim.soap_log().len();
    let report = actions::undo_last(
        &transport,
        &f.houses,
        &mut store,
        None,
        &Client::Cli,
        "test",
        102,
    )
    .unwrap()
    .unwrap();
    assert!(transport.changed.get());
    assert_eq!(report.failures.len(), 1, "{report:?}");
    assert!(!f.writes_since(before).iter().any(|write| write == "Play"));
    assert!(soap::get_mute(&f.lan, f.host).unwrap());
    assert_eq!(f.queue(), [DUPLICATE, DUPLICATE, LAST, OTHER]);
}

#[test]
fn regrouping_and_incomplete_scene_receipts_cannot_make_a_partial_action_undoable() {
    let f = fixture(2, false);
    let mut regroup = plan(&f, 0, true);
    regroup.ops.insert(
        0,
        SceneOp::Leave {
            room: "Beta".into(),
            player: PlayerId(f.sim.player("Beta").unwrap().uuid.clone()),
        },
    );
    let before = f.sim.soap_log().len();
    assert!(
        actions::capture_scene_queue_undo(&f.lan, &f.houses, &f.coordinator, &regroup, 100)
            .is_err()
    );
    assert_eq!(f.sim.soap_log().len(), before);
    let plan = plan(&f, 0, true);
    let pending =
        actions::capture_scene_queue_undo(&f.lan, &f.houses, &f.coordinator, &plan, 100).unwrap();
    let fader = Fader::default();
    let mut hooks = Receipts::new(&f);
    hooks.takeover = Some(&fader);
    let report = scenes::apply(
        &f.lan,
        &f.houses,
        &plan,
        &ApplyOptions {
            fade: Some((&fader, Duration::from_millis(1))),
            hooks: &hooks,
        },
    );
    assert!(report.is_complete(), "{report:?}");
    assert!(
        report
            .notes
            .iter()
            .any(|note| note.contains("fade stopped")),
        "{report:?}"
    );
    assert_eq!(
        soap::get_volume(&f.lan, f.host).unwrap(),
        21,
        "even a takeover that lands on the planned value remains the newer owner's intent"
    );
    let receipt = hooks.receipts.take().remove(0);
    let before = f.sim.soap_log().len();
    assert!(
        actions::finish_scene_queue_replacement(&f.lan, &f.houses, pending, &receipt, &report, 101)
            .is_err()
    );
    assert!(f.writes_since(before).is_empty());
}

struct LostSceneLevel {
    lan: SimLan,
    host: IpAddr,
    lost: Cell<bool>,
}

impl Transport for LostSceneLevel {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.lan.soap_post(host, path, action, body)?;
        if host == self.host && action.ends_with("#SetVolume\"") && !self.lost.replace(true) {
            return Err(ProtoError::Network {
                target: "synthetic room".into(),
                detail: "lost scene volume reply".into(),
            });
        }
        Ok(response)
    }
}

#[test]
fn a_lost_scene_level_reply_stays_a_barrier_even_when_every_final_value_matches() {
    let f = fixture(2, false);
    let plan = plan(&f, 0, true);
    let pending =
        actions::capture_scene_queue_undo(&f.lan, &f.houses, &f.coordinator, &plan, 100).unwrap();
    let transport = LostSceneLevel {
        lan: f.lan.clone(),
        host: f.sim.player("Beta").unwrap().ip,
        lost: Cell::new(false),
    };
    let hooks = Receipts::new(&f);
    let report = scenes::apply(
        &transport,
        &f.houses,
        &plan,
        &ApplyOptions {
            fade: None,
            hooks: &hooks,
        },
    );
    assert!(transport.lost.get());
    assert_eq!(report.failed.len(), 1);
    assert_eq!(soap::get_volume(&f.lan, f.host).unwrap(), 21);
    assert_eq!(soap::get_volume(&f.lan, transport.host).unwrap(), 9);
    let receipt = hooks.receipts.take().remove(0);
    let before = f.sim.soap_log().len();
    assert!(
        actions::finish_scene_queue_replacement(&f.lan, &f.houses, pending, &receipt, &report, 101)
            .is_err()
    );
    assert_eq!(f.sim.soap_log().len(), before);
}

#[test]
fn silent_scene_receipts_do_not_relax_direct_play_undo() {
    let f = fixture(2, false);
    let direct = f.capture();
    let mut plan = plan(&f, 0, false);
    plan.ops
        .retain(|op| !matches!(op, SceneOp::Volume { .. } | SceneOp::Mute { .. }));
    let (pending, receipt, report) = apply(&f, &plan, false);
    assert!(actions::finish_queue_replacement(&f.lan, &f.houses, direct, &receipt, 101).is_err());
    assert!(
        actions::finish_scene_queue_replacement(&f.lan, &f.houses, pending, &receipt, &report, 101)
            .is_ok()
    );
}

#[test]
fn changed_scene_results_refuse_even_force_without_restore_writes() {
    let f = fixture(2, false);
    let saved = finish(&f, apply(&f, &plan(&f, 4, true), false));
    let mut store = MemStore::default();
    record(&mut store, saved);
    soap::set_volume(&f.lan, f.host, 22).unwrap();
    for force in [false, true] {
        let before = f.sim.soap_log().len();
        assert!(f.undo(&mut store, force).is_err());
        assert!(f.writes_since(before).is_empty());
        assert!(store.last_undoable_action(None).unwrap().is_some());
    }
}

#[derive(Clone, Copy, Debug)]
enum SilentProofChange {
    Seek,
    Occurrence,
    Mode,
    Unreadable,
}

struct ChangedSilentOwner {
    lan: SimLan,
    after: &'static str,
    change: SilentProofChange,
    armed: Cell<bool>,
    changed: Cell<bool>,
}

impl Transport for ChangedSilentOwner {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name == "GetPositionInfo"
            && self.armed.get()
            && matches!(self.change, SilentProofChange::Unreadable)
            && !self.changed.replace(true)
        {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected missing silent execution proof".into(),
            });
        }
        let response = self.lan.soap_post(host, path, action, body)?;
        if name == self.after {
            self.armed.set(true);
            if !matches!(self.change, SilentProofChange::Unreadable) && !self.changed.replace(true)
            {
                match self.change {
                    SilentProofChange::Seek => soap::seek_position(&self.lan, host, 99)?,
                    // The same URI occurs twice: a different occurrence is
                    // still an ownership change even when its URI matches.
                    SilentProofChange::Occurrence => soap::seek_track(&self.lan, host, 1)?,
                    SilentProofChange::Mode => set_play_mode(&self.lan, host, PlayMode::Normal)?,
                    SilentProofChange::Unreadable => unreachable!(),
                }
            }
        }
        Ok(response)
    }
}

#[test]
fn silent_scene_receipts_require_owned_pause_and_commit_before_publishing_undo() {
    for generation in [1, 2] {
        for ordered in [false, true] {
            for after in ["Pause", "AddURIToQueue"] {
                for change in [
                    SilentProofChange::Seek,
                    SilentProofChange::Occurrence,
                    SilentProofChange::Mode,
                    SilentProofChange::Unreadable,
                ] {
                    let f = fixture(generation, true);
                    let mut plan = plan(&f, if ordered { 4 } else { 0 }, false);
                    plan.ops.retain(|op| matches!(op, SceneOp::Uri { .. }));
                    let pending = actions::capture_scene_queue_undo(
                        &f.lan,
                        &f.houses,
                        &f.coordinator,
                        &plan,
                        100,
                    )
                    .unwrap();
                    let t = ChangedSilentOwner {
                        lan: f.lan.clone(),
                        after,
                        change,
                        armed: Cell::new(false),
                        changed: Cell::new(false),
                    };
                    let before = f.sim.soap_log().len();
                    let receipt = if ordered {
                        let destination =
                            control::TrackBatchDestination::read(&t, f.host, &f.coordinator)
                                .unwrap();
                        control::load_spotify_tracks_bound_with_receipt(
                            &t,
                            &f.houses,
                            &f.coordinator,
                            &TRACKS,
                            &destination,
                        )
                    } else {
                        control::load_spotify_container_with_receipt(
                            &t,
                            &f.houses,
                            &f.coordinator,
                            SpotifyContainer::Album,
                            "spotify:album:0000000000000000000001",
                            "Replacement",
                        )
                    }
                    .unwrap();
                    assert!(t.changed.get(), "{after}: {change:?}");
                    let writes = f.writes_since(before);
                    assert!(!writes.iter().any(|name| name == "Play"));
                    assert_eq!(
                        writes
                            .iter()
                            .filter(|name| *name == "AddURIToQueue")
                            .count(),
                        if ordered { TRACKS.len() } else { 1 },
                    );
                    let report = SceneReport {
                        done: plan.ops.clone(),
                        failed: Vec::new(),
                        notes: plan.notes.clone(),
                        faded: 0,
                    };
                    let before = f.sim.soap_log().len();
                    let error = actions::finish_scene_queue_replacement(
                        &f.lan, &f.houses, pending, &receipt, &report, 101,
                    )
                    .unwrap_err();
                    assert!(
                        error
                            .to_string()
                            .contains("no stable original playback proof"),
                        "{after}: {change:?}: {error}",
                    );
                    assert!(f.writes_since(before).is_empty());
                }
            }
        }
    }
}
