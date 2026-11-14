//! The daemon durably consumes renderer recovery before touching a speaker.

use super::*;
use fsonos_core::HouseholdState;
use fsonos_core::clock::FakeClock;
use fsonos_core::playback::{Playback, PlaybackObservation};
use fsonos_core::store::MemStore;
use fsonos_proto::content::{QUEUE, browse_snapshot};
use fsonos_proto::control as soap;
use fsonos_proto::didl::{spotify_queue_uri, spotify_track_didl};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_spotify::dj::PickReason;
use fsonos_spotify::feed::{FeedSnapshot, Movement};
use fsonos_types::TransportState;
use serde_json::Value;
use std::cell::{Cell, RefCell};
use std::net::IpAddr;
use std::path::Path;

struct Rig {
    sim: SimHandle,
    households: Vec<HouseholdState>,
    coordinator: PlayerId,
    host: IpAddr,
    directory: tempfile::TempDir,
}

impl Rig {
    fn new(s2: bool) -> Self {
        let builder = SimHousehold::builder();
        let sim = if s2 {
            builder.s2([SimPlayerSpec::new("Den", SimModel::One)])
        } else {
            builder.s1([SimPlayerSpec::new("Den", SimModel::Play5Gen1)])
        }
        .spawn()
        .unwrap();
        let host = sim.player("Den").unwrap().ip;
        let coordinator = PlayerId(sim.player("Den").unwrap().uuid.clone());
        let mut house = HouseholdState::default();
        house.apply_topology(&get_zone_group_state(&sim.lan(), host).unwrap());
        Self {
            sim,
            households: vec![house],
            coordinator,
            host,
            directory: tempfile::tempdir().unwrap(),
        }
    }

    fn path(&self) -> PathBuf {
        self.directory.path().join(saved::FEEDS_FILE)
    }

    fn engine(&self) -> SpotifyDj {
        SpotifyDj::new(None).with_saved_feeds(Some(self.path()))
    }

    fn at<'a>(&'a self, transport: &'a dyn Transport) -> DjSpeakers<'a> {
        DjSpeakers {
            transport,
            households: &self.households,
            coordinator: &self.coordinator,
        }
    }

    fn start(&self) -> SpotifyDj {
        let lan = self.sim.lan();
        let params = control::spotify_params(&lan, &self.households, &self.coordinator)
            .unwrap()
            .unwrap();
        let works: Vec<_> = (1..=5)
            .map(|position| {
                let uri = format!("spotify:track:{position:022}");
                let title = format!("Recovery song {position}");
                soap::add_uri_to_queue(
                    &lan,
                    self.host,
                    &spotify_queue_uri(&uri),
                    &spotify_track_didl(&uri, &title, &params),
                    false,
                )
                .unwrap();
                QueuedWork {
                    title: title.clone(),
                    composer: "Synthetic artist".into(),
                    work_key: format!("recovery-song-{position}"),
                    movements: vec![Movement {
                        uri,
                        title,
                        position: Some(position),
                        added: true,
                    }],
                    reason: PickReason {
                        factors: Vec::new(),
                        relaxed: Vec::new(),
                        summary: "fixture".into(),
                    },
                }
            })
            .collect();
        control::play_queue_from_in_order(&lan, &self.households, &self.coordinator, 1).unwrap();
        let feed = QueueFeed::restore(
            FeedSnapshot {
                zone: self.coordinator.0.clone(),
                earlier: Vec::new(),
                lookahead: 1,
                queued: works,
                playing: Some(1),
                recorded: None,
                played: Vec::new(),
                recovery: Default::default(),
                dry_end: None,
            },
            DjConfig::default(),
            1,
        );
        let dj = self.engine();
        {
            let mut state = dj.state();
            state.feeds.insert(self.coordinator.clone(), feed);
            state
                .starters
                .insert(self.coordinator.clone(), vec![Client::McpStdio]);
            state.handover.remember(&self.households, &self.coordinator);
            dj.save_checked(&mut state).unwrap();
        }
        dj
    }

    fn playback(&self) -> PlayerPlayback {
        let lan = self.sim.lan();
        let observation = PlaybackObservation {
            transport: soap::get_transport_info(&lan, self.host).unwrap(),
            position: soap::get_position_info(&lan, self.host).unwrap(),
            media: soap::get_media_info(&lan, self.host).unwrap(),
        };
        let mut playback = Playback::default();
        playback.apply_observation(&self.coordinator, &observation, Instant::now());
        playback.of(&self.coordinator).unwrap().clone()
    }

    fn saved(&self) -> Value {
        serde_json::from_str(&std::fs::read_to_string(self.path()).unwrap()).unwrap()
    }
}

fn clock() -> FakeClock {
    FakeClock::new(chrono::DateTime::parse_from_rfc3339("2026-10-12T12:00:00Z").unwrap())
}

#[derive(Clone, Copy)]
enum Reply {
    Refused,
    Lost,
}

/// At the first mutation, inspect the real durable marker. Optionally retain
/// that exact revision and obstruct the final save, as if a crash left it last.
struct Observed<'a> {
    inner: &'a SimLan,
    path: &'a Path,
    retained: Option<&'a Path>,
    reply: Option<Reply>,
    first: Cell<bool>,
    writes: RefCell<Vec<String>>,
}

impl<'a> Observed<'a> {
    fn new(inner: &'a SimLan, path: &'a Path) -> Self {
        Self {
            inner,
            path,
            retained: None,
            reply: None,
            first: Cell::new(false),
            writes: RefCell::new(Vec::new()),
        }
    }
}

impl Transport for Observed<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name != "Browse" && !name.starts_with("Get") {
            self.writes.borrow_mut().push(name.into());
            if !self.first.replace(true) {
                let saved: Value =
                    serde_json::from_str(&std::fs::read_to_string(self.path).unwrap()).unwrap();
                let recovery = &saved["feeds"][0]["recovery"];
                assert_eq!(recovery["in_flight"], true);
                assert!(!recovery["attempts"].as_array().unwrap().is_empty());
                if let Some(retained) = self.retained {
                    std::fs::rename(self.path, retained).unwrap();
                    std::fs::create_dir(self.path).unwrap();
                }
            }
        }
        if name == "Play"
            && let Some(reply) = self.reply
        {
            return match reply {
                Reply::Refused => Err(ProtoError::SoapFault {
                    code: 701,
                    reason: "synthetic recovery refusal".into(),
                }),
                Reply::Lost => {
                    self.inner.soap_post(host, path, action, body)?;
                    Err(ProtoError::Network {
                        target: "simulated recovery reply".into(),
                        detail: "Play succeeded but its reply was lost".into(),
                    })
                }
            };
        }
        self.inner.soap_post(host, path, action, body)
    }
}

#[test]
fn unavailable_checkpoint_defers_the_daemon_without_changing_playback() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let mut dj = rig.start();
        let path = rig.path();
        let original = std::fs::read_to_string(&path).unwrap();
        let before = dj.state().feeds[&rig.coordinator].snapshot();
        // A directory cannot be replaced by the checkpoint file.
        dj.feeds_file = Some(rig.directory.path().to_path_buf());
        rig.sim.playback_error("Den").unwrap();
        let event = rig.playback();
        let lan = rig.sim.lan();
        let observed = Observed::new(&lan, &path);
        dj.on_playback(
            rig.at(&observed),
            &mut MemStore::default(),
            &event,
            &clock(),
        );
        assert!(observed.writes.borrow().is_empty());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), original);
        let after = rig.playback();
        assert_eq!(after.queue_position, event.queue_position);
        assert_eq!(after.transport, event.transport);
        assert_eq!(after.transport_status, event.transport_status);
        let state = dj.state();
        assert_eq!(state.feeds[&rig.coordinator].snapshot(), before);
        assert!(state.refill_retries.contains_key(&rig.coordinator));
        assert!(state.problems[&rig.coordinator].contains("could not be saved"));
        rig.sim.shutdown();
    }
}

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("a restored in-flight recovery cannot touch a speaker")
    }
}

#[test]
fn failed_completion_save_cannot_replay_confirmed_refused_or_lost_recovery() {
    for reply in [None, Some(Reply::Refused), Some(Reply::Lost)] {
        let rig = Rig::new(true);
        let dj = rig.start();
        let path = rig.path();
        let retained = rig.directory.path().join("interrupted.json");
        let lan = rig.sim.lan();
        let queue = browse_snapshot(&lan, rig.host, QUEUE).unwrap();
        rig.sim.playback_error("Den").unwrap();
        let event = rig.playback();
        let observed = Observed {
            retained: Some(&retained),
            reply,
            ..Observed::new(&lan, &path)
        };
        dj.on_playback(
            rig.at(&observed),
            &mut MemStore::default(),
            &event,
            &clock(),
        );
        assert_eq!(*observed.writes.borrow(), ["Seek", "Play"]);
        assert!(path.is_dir(), "the completion save was obstructed");
        let saved: Value =
            serde_json::from_str(&std::fs::read_to_string(&retained).unwrap()).unwrap();
        assert_eq!(saved["feeds"][0]["recovery"]["in_flight"], true);
        assert_eq!(
            saved["feeds"][0]["recovery"]["attempts"]
                .as_array()
                .unwrap()
                .len(),
            1
        );
        assert_eq!(dj.feeds(&rig.coordinator), reply.is_none());
        let actual = rig.playback();
        let expected = if matches!(reply, Some(Reply::Refused)) {
            TransportState::Stopped
        } else {
            TransportState::Playing
        };
        assert_eq!(actual.transport, Some(expected));
        assert_eq!(browse_snapshot(&lan, rig.host, QUEUE).unwrap(), queue);

        let restarted = SpotifyDj::new(None).with_saved_feeds(Some(retained));
        assert!(!restarted.feeds(&rig.coordinator));
        assert!(!restarted.state().starters.contains_key(&rig.coordinator));
        restarted.on_playback(rig.at(&NoIo), &mut MemStore::default(), &actual, &clock());
        assert_eq!(browse_snapshot(&lan, rig.host, QUEUE).unwrap(), queue);
        rig.sim.shutdown();
    }
}

#[test]
fn completed_recoveries_keep_the_budget_across_real_saved_feed_reopens() {
    let rig = Rig::new(true);
    let mut dj = rig.start();
    let path = rig.path();
    let lan = rig.sim.lan();
    let queue = browse_snapshot(&lan, rig.host, QUEUE).unwrap();
    for attempt in 1..=3 {
        rig.sim.playback_error("Den").unwrap();
        let event = rig.playback();
        let observed = Observed::new(&lan, &path);
        dj.on_playback(
            rig.at(&observed),
            &mut MemStore::default(),
            &event,
            &clock(),
        );
        assert_eq!(*observed.writes.borrow(), ["Seek", "Play"]);
        assert!(dj.feeds(&rig.coordinator));
        let saved = rig.saved();
        assert_eq!(saved["feeds"][0]["recovery"]["in_flight"], false);
        assert_eq!(
            saved["feeds"][0]["recovery"]["attempts"]
                .as_array()
                .unwrap()
                .len(),
            attempt
        );
        drop(dj);
        dj = rig.engine();
        assert_eq!(
            dj.starter(&rig.coordinator),
            DjStarter::Known(vec![Client::McpStdio])
        );
    }
    rig.sim.playback_error("Den").unwrap();
    let event = rig.playback();
    let observed = Observed::new(&lan, &path);
    dj.on_playback(
        rig.at(&observed),
        &mut MemStore::default(),
        &event,
        &clock(),
    );
    assert!(
        observed.writes.borrow().is_empty(),
        "fourth recovery must not start"
    );
    assert!(!dj.feeds(&rig.coordinator));
    assert!(rig.saved()["feeds"].as_array().unwrap().is_empty());
    assert_eq!(browse_snapshot(&lan, rig.host, QUEUE).unwrap(), queue);
    rig.sim.shutdown();
}

struct UncertainStop<'a> {
    inner: &'a SimLan,
    writes: Cell<usize>,
}

impl Transport for UncertainStop<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name != "Browse" && !name.starts_with("Get") {
            self.writes.set(self.writes.get() + 1);
        }
        if name == "GetTransportInfo" {
            return Err(ProtoError::Network {
                target: "simulated stop read-back".into(),
                detail: "the stopped state could not be observed".into(),
            });
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if name == "Stop" {
            return Err(ProtoError::Network {
                target: "simulated stop reply".into(),
                detail: "Stop succeeded but its reply was lost".into(),
            });
        }
        Ok(response)
    }
}

#[test]
fn an_uncertain_stop_persists_release_and_cannot_restart_a_dry_queue_after_reopen() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let dj = rig.start();
        let lan = rig.sim.lan();
        let mut store = MemStore::default();
        dj.on_playback(rig.at(&lan), &mut store, &rig.playback(), &clock());
        {
            let mut state = dj.state();
            assert!(state.listening.contains_key(&rig.coordinator));
            let mut snapshot = state.feeds[&rig.coordinator].snapshot().unwrap();
            snapshot.dry_end = snapshot.queued.last().and_then(QueuedWork::last);
            assert!(snapshot.dry_end.is_some());
            state.feeds.insert(
                rig.coordinator.clone(),
                QueueFeed::restore(snapshot, DjConfig::default(), 1),
            );
            dj.save_checked(&mut state).unwrap();
        }
        let queue = browse_snapshot(&lan, rig.host, QUEUE).unwrap();
        let observed = UncertainStop {
            inner: &lan,
            writes: Cell::new(0),
        };
        assert!(
            dj.act_as(
                rig.at(&observed),
                &mut store,
                DjAction::Stop,
                &clock(),
                &Client::Cli,
            )
            .is_err()
        );
        assert_eq!(observed.writes.get(), 1);
        assert!(!dj.feeds(&rig.coordinator));
        assert!(!dj.state().feeds.contains_key(&rig.coordinator));
        assert!(!dj.state().listening.contains_key(&rig.coordinator));
        assert_eq!(dj.starter(&rig.coordinator), DjStarter::Missing);
        assert!(rig.saved()["feeds"].as_array().unwrap().is_empty());
        assert!(rig.saved().get("starters").is_none());
        assert_eq!(browse_snapshot(&lan, rig.host, QUEUE).unwrap(), queue);
        let stopped = rig.playback();
        assert_eq!(stopped.transport, Some(TransportState::Stopped));
        dj.on_playback(rig.at(&NoIo), &mut store, &stopped, &clock());
        drop(dj);

        let reopened = rig.engine();
        assert!(!reopened.feeds(&rig.coordinator));
        assert_eq!(reopened.starter(&rig.coordinator), DjStarter::Missing);
        reopened.on_playback(rig.at(&NoIo), &mut store, &stopped, &clock());
        assert_eq!(browse_snapshot(&lan, rig.host, QUEUE).unwrap(), queue);
        rig.sim.shutdown();
    }
}

#[test]
fn an_unwritable_stop_checkpoint_preserves_the_feed_without_touching_the_speaker() {
    for s2 in [false, true] {
        let rig = Rig::new(s2);
        let mut dj = rig.start();
        let before = rig.saved();
        let feed = dj.state().feeds[&rig.coordinator].snapshot();
        let starter = dj.starter(&rig.coordinator);
        let lan = rig.sim.lan();
        let queue = browse_snapshot(&lan, rig.host, QUEUE).unwrap();
        // Replacing a directory fails even when the tests run as root.
        dj.feeds_file = Some(rig.directory.path().to_path_buf());
        let error = dj
            .act_as(
                rig.at(&NoIo),
                &mut MemStore::default(),
                DjAction::Stop,
                &clock(),
                &Client::Cli,
            )
            .unwrap_err();
        assert_eq!(error.code, ErrorCode::Internal);
        assert_eq!(dj.state().feeds[&rig.coordinator].snapshot(), feed);
        assert_eq!(dj.starter(&rig.coordinator), starter);
        assert_eq!(rig.saved(), before);
        assert_eq!(browse_snapshot(&lan, rig.host, QUEUE).unwrap(), queue);
        assert_eq!(rig.playback().transport, Some(TransportState::Playing));
        rig.sim.shutdown();
    }
}

#[test]
fn stop_prerequisite_failures_and_definite_refusals_rearm_the_saved_feed() {
    for s2 in [false, true] {
        for action in ["GetMediaInfo", "Stop"] {
            let rig = Rig::new(s2);
            let dj = rig.start();
            let before = rig.saved();
            let feed = dj.state().feeds[&rig.coordinator].snapshot();
            let starter = dj.starter(&rig.coordinator);
            let lan = rig.sim.lan();
            let queue = browse_snapshot(&lan, rig.host, QUEUE).unwrap();
            rig.sim.upnp_fault("Den", action, 701).unwrap();
            assert!(
                dj.act_as(
                    rig.at(&lan),
                    &mut MemStore::default(),
                    DjAction::Stop,
                    &clock(),
                    &Client::Cli,
                )
                .is_err()
            );
            assert_eq!(dj.state().feeds[&rig.coordinator].snapshot(), feed);
            assert_eq!(dj.starter(&rig.coordinator), starter);
            assert_eq!(rig.saved(), before);
            rig.sim.clear_faults("Den").unwrap();
            assert_eq!(browse_snapshot(&lan, rig.host, QUEUE).unwrap(), queue);
            assert_eq!(rig.playback().transport, Some(TransportState::Playing));
            rig.sim.shutdown();
        }
    }
}

/// Inspect the retired revision before Stop is sent, then make the original
/// store path unwritable after the real speaker accepted it. The saved copy
/// represents the last durable revision a restarted daemon could recover.
struct CheckpointedStop<'a> {
    inner: &'a SimLan,
    path: &'a Path,
    retained: &'a Path,
    uncertain: bool,
    writes: Cell<usize>,
}

impl Transport for CheckpointedStop<'_> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let name = action.trim_matches('"').rsplit('#').next().unwrap();
        if name != "Browse" && !name.starts_with("Get") {
            self.writes.set(self.writes.get() + 1);
        }
        if name == "GetTransportInfo" && self.uncertain {
            return Err(ProtoError::Network {
                target: "simulated stop read-back".into(),
                detail: "the stopped state could not be observed".into(),
            });
        }
        if name == "Stop" {
            let saved: Value =
                serde_json::from_str(&std::fs::read_to_string(self.path).unwrap()).unwrap();
            assert!(saved["feeds"].as_array().unwrap().is_empty());
            assert!(saved.get("starters").is_none());
            assert!(saved.get("groups").is_none());
        }
        let response = self.inner.soap_post(host, path, action, body)?;
        if name == "Stop" {
            std::fs::rename(self.path, self.retained).unwrap();
            std::fs::create_dir(self.path).unwrap();
            if self.uncertain {
                return Err(ProtoError::Network {
                    target: "simulated stop reply".into(),
                    detail: "Stop succeeded but its reply was lost".into(),
                });
            }
        }
        Ok(response)
    }
}

#[test]
fn a_stop_remains_retired_after_store_loss_and_daemon_restart() {
    for s2 in [false, true] {
        for uncertain in [false, true] {
            let rig = Rig::new(s2);
            let dj = rig.start();
            {
                let mut state = dj.state();
                let mut snapshot = state.feeds[&rig.coordinator].snapshot().unwrap();
                snapshot.dry_end = snapshot.queued.last().and_then(QueuedWork::last);
                assert!(snapshot.dry_end.is_some());
                state.feeds.insert(
                    rig.coordinator.clone(),
                    QueueFeed::restore(snapshot, DjConfig::default(), 1),
                );
                dj.save_checked(&mut state).unwrap();
            }
            let path = rig.path();
            let retained = rig.directory.path().join("last-durable-feed.json");
            let lan = rig.sim.lan();
            let queue = browse_snapshot(&lan, rig.host, QUEUE).unwrap();
            let observed = CheckpointedStop {
                inner: &lan,
                path: &path,
                retained: &retained,
                uncertain,
                writes: Cell::new(0),
            };
            let result = dj.act_as(
                rig.at(&observed),
                &mut MemStore::default(),
                DjAction::Stop,
                &clock(),
                &Client::Cli,
            );
            assert_eq!(result.is_err(), uncertain);
            assert_eq!(observed.writes.get(), 1);
            assert!(path.is_dir(), "the original store is no longer writable");
            assert!(!dj.feeds(&rig.coordinator));
            assert_eq!(dj.starter(&rig.coordinator), DjStarter::Missing);
            let stopped = rig.playback();
            assert_eq!(stopped.transport, Some(TransportState::Stopped));
            assert_eq!(browse_snapshot(&lan, rig.host, QUEUE).unwrap(), queue);
            dj.on_playback(
                rig.at(&NoIo),
                &mut MemStore::default(),
                &stopped,
                &clock(),
            );
            drop(dj);

            let reopened = SpotifyDj::new(None).with_saved_feeds(Some(retained));
            assert!(!reopened.feeds(&rig.coordinator));
            assert_eq!(reopened.starter(&rig.coordinator), DjStarter::Missing);
            reopened.on_playback(
                rig.at(&NoIo),
                &mut MemStore::default(),
                &stopped,
                &clock(),
            );
            assert_eq!(browse_snapshot(&lan, rig.host, QUEUE).unwrap(), queue);
            rig.sim.shutdown();
        }
    }
}
