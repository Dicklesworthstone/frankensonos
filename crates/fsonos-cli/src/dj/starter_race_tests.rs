//! A playback attempt must use the policy identities of the feed it advances.

use super::SpotifyDj;
use fsonos_api::dj::{DjEngine, DjSpeakers, DjStarter};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::policy::Client;
use fsonos_core::store::{ActionFilter, MemStore, Store};
use fsonos_proto::control;
use fsonos_proto::didl::{spotify_queue_uri, spotify_track_didl};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_spotify::dj::{DjConfig, PickReason};
use fsonos_spotify::feed::{FeedSnapshot, Movement, QueueFeed, QueuedWork};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;

const TRACK: &str = "spotify:track:0000000000000000000041";

struct Running {
    sim: SimHandle,
    households: Vec<HouseholdState>,
    coordinator: PlayerId,
    dj: SpotifyDj,
    playback: PlayerPlayback,
}

fn running() -> Running {
    let sim = SimHousehold::builder()
        .s2([SimPlayerSpec::new("Den", SimModel::One)])
        .spawn()
        .unwrap();
    let lan = sim.lan();
    let host = sim.player("Den").unwrap().ip;
    let coordinator = PlayerId(sim.player("Den").unwrap().uuid.clone());
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    let households = vec![house];
    let params = fsonos_core::control::spotify_params(&lan, &households, &coordinator)
        .unwrap()
        .unwrap();
    control::add_uri_to_queue(
        &lan,
        host,
        &spotify_queue_uri(TRACK),
        &spotify_track_didl(TRACK, "Owned movement", &params),
        false,
    )
    .unwrap();
    fsonos_core::control::play_queue_from_in_order(&lan, &households, &coordinator, 1).unwrap();
    let snapshot = FeedSnapshot {
        zone: coordinator.0.clone(),
        earlier: Vec::new(),
        lookahead: 1,
        queued: vec![QueuedWork {
            title: "Owned work".into(),
            composer: "Synthetic composer".into(),
            work_key: "starter-race-fixture".into(),
            movements: vec![Movement {
                uri: TRACK.into(),
                title: "Owned movement".into(),
                position: Some(1),
                added: true,
            }],
            reason: PickReason {
                factors: Vec::new(),
                relaxed: Vec::new(),
                summary: "fixture".into(),
            },
        }],
        playing: Some(1),
        recorded: None,
        played: Vec::new(),
        dry_end: None,
        recovery: Default::default(),
    };
    let dj = SpotifyDj::new(None);
    dj.state().feeds.insert(
        coordinator.clone(),
        QueueFeed::restore(snapshot, DjConfig::default(), 1),
    );
    let mut playback = PlayerPlayback::default();
    playback.transport = Some(TransportState::Playing);
    playback.media_uri = Some(format!("x-rincon-queue:{}#0", coordinator.0));
    playback.track_uri = Some(control::get_position_info(&lan, host).unwrap().uri);
    playback.queue_position = Some(1);
    playback.queue_length = Some(1);
    Running {
        sim,
        households,
        coordinator,
        dj,
        playback,
    }
}

struct NoIo;

impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("a stale policy identity cannot perform speaker I/O")
    }

    fn http_get(&self, _: &str) -> Result<String, ProtoError> {
        panic!("a stale policy identity cannot perform HTTP I/O")
    }
}

#[test]
fn a_stale_starter_defers_without_changing_the_feed_or_history() {
    let rig = running();
    let mut store = MemStore::default();
    let expected = DjStarter::Known(vec![Client::Cli]);
    let before = rig.dj.state().feeds[&rig.coordinator].snapshot();
    // An explicit restart can replace the caller or add a further limit;
    // a handoff from a legacy feed can remove known provenance altogether.
    for current in [
        Some(vec![Client::McpStdio]),
        Some(vec![Client::Cli, Client::McpStdio]),
        None,
    ] {
        {
            let mut state = rig.dj.state();
            state.starters.remove(&rig.coordinator);
            if let Some(current) = &current {
                state
                    .starters
                    .insert(rig.coordinator.clone(), current.clone());
            }
        }
        let at = DjSpeakers {
            transport: &NoIo,
            households: &rig.households,
            coordinator: &rig.coordinator,
        };
        rig.dj
            .on_playback_with_starter(at, &mut store, &rig.playback, &SystemClock, &expected);
        let state = rig.dj.state();
        assert_eq!(state.feeds[&rig.coordinator].snapshot(), before);
        assert_eq!(state.starters.get(&rig.coordinator), current.as_ref());
        assert!(state.refill_retries.is_empty());
        assert!(state.problems.is_empty());
        assert!(state.listening.is_empty());
        assert!(state.plan_check_at.is_none());
        assert!(state.logged.is_empty());
        assert!(store.recent_plays(None, 10).unwrap().is_empty());
        assert!(
            store
                .recent_actions(&ActionFilter::default())
                .unwrap()
                .is_empty()
        );
    }
    rig.sim.shutdown();
}

#[test]
fn matching_starter_still_observes_owned_playback() {
    let rig = running();
    let clients = vec![Client::Cli, Client::McpStdio];
    rig.dj
        .state()
        .starters
        .insert(rig.coordinator.clone(), clients.clone());
    let lan = rig.sim.lan();
    let at = DjSpeakers {
        transport: &lan,
        households: &rig.households,
        coordinator: &rig.coordinator,
    };
    let mut store = MemStore::default();
    let since = rig.sim.soap_log().len();
    rig.dj.on_playback_with_starter(
        at,
        &mut store,
        &rig.playback,
        &SystemClock,
        &DjStarter::Known(clients),
    );
    assert!(rig.sim.soap_log().len() > since);
    assert!(rig.dj.state().feeds[&rig.coordinator].accepted_playback());
    let plays = store.recent_plays(None, 10).unwrap();
    assert_eq!(plays.len(), 1, "the current movement is still observed");
    assert_eq!(plays[0].source_uri, TRACK);
    rig.sim.shutdown();
}

#[test]
fn missing_provenance_keeps_stopped_and_failed_feeds_available_for_explicit_start() {
    let rig = running();
    let mut store = MemStore::default();
    let before = rig.dj.state().feeds[&rig.coordinator].snapshot();
    let at = DjSpeakers {
        transport: &NoIo,
        households: &rig.households,
        coordinator: &rig.coordinator,
    };
    for (transport, status) in [
        (TransportState::Stopped, None),
        (TransportState::Playing, Some("ERROR_OCCURRED")),
    ] {
        let mut playback = rig.playback.clone();
        playback.transport = Some(transport);
        playback.transport_status = status.map(str::to_owned);
        rig.dj.on_playback_with_starter(
            at,
            &mut store,
            &playback,
            &SystemClock,
            &DjStarter::Missing,
        );
        {
            let state = rig.dj.state();
            assert_eq!(state.feeds[&rig.coordinator].snapshot(), before);
            assert!(state.starters.is_empty());
            assert!(state.refill_retries.is_empty());
            assert!(state.plan_check_at.is_none());
            assert!(state.logged.is_empty());
        }
        assert!(store.recent_plays(None, 10).unwrap().is_empty());
        assert!(
            store
                .recent_actions(&ActionFilter::default())
                .unwrap()
                .is_empty()
        );
        let status = rig.dj.status(at, &store, Some(1), &SystemClock).unwrap();
        assert!(status.running);
        assert!(status.problem.as_deref().is_some_and(|problem| {
            problem.contains("no initiating policy client")
                && problem.contains("explicitly start the DJ")
        }));
    }
    rig.sim.shutdown();
}
