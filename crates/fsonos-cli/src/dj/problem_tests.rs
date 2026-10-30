//! What the DJ last ran into feeding a group shows in its status.

use super::SpotifyDj;
use fsonos_api::dj::{DjEngine, DjSpeakers};
use fsonos_api::plan::DjAction;
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::store::MemStore;
use fsonos_proto::control;
use fsonos_proto::didl::{spotify_queue_uri, spotify_track_didl};
use fsonos_proto::topology::get_zone_group_state;
use fsonos_sim::{SimHousehold, SimModel, SimPlayerSpec};
use fsonos_spotify::dj::{DjConfig, PickReason};
use fsonos_spotify::feed::{FeedSnapshot, Movement, QueueFeed, QueuedWork};
use fsonos_types::{PlayerId, TransportState};

const TRACKS: [&str; 2] = [
    "spotify:track:0000000000000000000001",
    "spotify:track:0000000000000000000002",
];

/// A feed on Den's queue: one two-movement work, the second playing.
fn fed(den: &PlayerId) -> FeedSnapshot {
    FeedSnapshot {
        zone: den.0.clone(),
        earlier: Vec::new(),
        lookahead: 1,
        queued: vec![QueuedWork {
            title: "Synthetic work".into(),
            composer: "Synthetic composer".into(),
            work_key: "synthetic-work".into(),
            movements: TRACKS
                .iter()
                .enumerate()
                .map(|(i, uri)| Movement {
                    uri: (*uri).into(),
                    title: format!("Movement {i}"),
                    position: Some(u32::try_from(i + 1).unwrap()),
                    added: true,
                })
                .collect(),
            reason: PickReason {
                factors: Vec::new(),
                relaxed: Vec::new(),
                summary: "fixture".into(),
            },
        }],
        playing: Some(2),
        recorded: None,
        played: Vec::new(),
    }
}

#[test]
fn a_top_up_that_fails_says_why_in_the_status_until_a_command() {
    let sim = SimHousehold::builder()
        .s2([SimPlayerSpec::new("Den", SimModel::One)])
        .spawn()
        .unwrap();
    let lan = sim.lan();
    let den = PlayerId(sim.player("Den").unwrap().uuid.clone());
    let host = sim.player("Den").unwrap().ip;
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    let households = vec![house];
    let params = fsonos_core::control::spotify_params(&lan, &households, &den)
        .unwrap()
        .unwrap();
    for uri in TRACKS {
        let didl = spotify_track_didl(uri, "Movement", &params);
        control::add_uri_to_queue(&lan, host, &spotify_queue_uri(uri), &didl, false).unwrap();
    }
    // No library: the next work can't be picked, so the top-up fails.
    let dj = SpotifyDj::new(None);
    dj.state().feeds.insert(
        den.clone(),
        QueueFeed::restore(fed(&den), DjConfig::default(), 1),
    );
    let at = DjSpeakers {
        transport: &lan,
        households: &households,
        coordinator: &den,
    };
    let (mut store, clock) = (MemStore::default(), SystemClock);
    let mut playback = PlayerPlayback::default();
    playback.transport = Some(TransportState::Playing);
    playback.media_uri = Some(format!("x-rincon-queue:{}#0", den.0));
    playback.queue_position = Some(2);
    playback.queue_length = Some(2);
    dj.on_playback(at, &mut store, &playback, &clock);
    let status = dj.status(at, &store, Some(2), &clock).unwrap();
    assert!(status.running, "{status:?}");
    let problem = status.problem.clone().expect("the failed top-up");
    assert!(problem.contains("couldn't top up the queue"), "{problem}");
    assert!(status.summary().contains(&problem), "{}", status.summary());
    // An explicit command starts over.
    dj.act(at, &mut store, DjAction::Stop, &clock).unwrap();
    let status = dj.status(at, &store, Some(2), &clock).unwrap();
    assert_eq!((status.running, status.problem), (false, None));
}

#[test]
fn a_start_with_nothing_to_play_leaves_no_feed_behind() {
    let sim = SimHousehold::builder()
        .s2([SimPlayerSpec::new("Den", SimModel::One)])
        .spawn()
        .unwrap();
    let lan = sim.lan();
    let den = PlayerId(sim.player("Den").unwrap().uuid.clone());
    let host = sim.player("Den").unwrap().ip;
    let mut house = HouseholdState::default();
    house.apply_topology(&get_zone_group_state(&lan, host).unwrap());
    let households = vec![house];
    let at = DjSpeakers {
        transport: &lan,
        households: &households,
        coordinator: &den,
    };
    let (mut store, clock) = (MemStore::default(), SystemClock);
    // No library yet: the start fails before anything is queued.
    let dj = SpotifyDj::new(None);
    let failed = dj.act(at, &mut store, DjAction::Start, &clock).unwrap_err();
    assert_eq!(failed.code, fsonos_api::ErrorCode::SpotifyAuthRequired);
    assert!(dj.state().feeds.is_empty(), "no feed is left to retry");
    // Later playback in the room is the owner's, not the DJ's to top up.
    let mut playback = PlayerPlayback::default();
    playback.transport = Some(TransportState::Playing);
    playback.media_uri = Some(format!("x-rincon-queue:{}#0", den.0));
    playback.queue_position = Some(1);
    playback.queue_length = Some(1);
    dj.on_playback(at, &mut store, &playback, &clock);
    let status = dj.status(at, &store, Some(1), &clock).unwrap();
    assert_eq!((status.running, status.problem), (false, None));
}
