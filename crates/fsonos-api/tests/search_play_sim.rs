//! Search-to-play keeps the selected favorite, and shares guarded playback.
//! SOAP and rendering run through virtual S1/S2 players on real loopback sockets.

use fsonos_api::surface::{Surface, Survey};
use fsonos_api::{ErrorCode, SearchPlayRequest};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, LibraryEntry, MemStore, Store};
use fsonos_core::{favorites, resolve_room};
use fsonos_proto::control as soap;
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::topology::get_zone_group_state;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::Track;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

const RADIO: &str = "x-rincon-mp3radio://stream.example.invalid/sim.mp3";
const REPLACEMENT: &str = "x-rincon-mp3radio://stream.example.invalid/replacement.mp3";
const TRACKS: [&str; 2] = [
    "spotify:track:0000000000000000000001",
    "spotify:track:0000000000000000000002",
];

struct ChangedFavorites {
    inner: SimLan,
    reads: Arc<AtomicUsize>,
    replace: bool,
}

impl Transport for ChangedFavorites {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.inner.soap_post(host, path, action, body)?;
        if action.ends_with("#Browse\"") && body.contains("<ObjectID>FV:2</ObjectID>") {
            let previous = self.reads.fetch_add(1, Ordering::SeqCst);
            if previous > 0 && self.replace {
                // Same FV:2 ID, different renderer and metadata after search.
                return Ok(response
                    .replace(RADIO, REPLACEMENT)
                    .replace("Sim Radio", "Replacement Station"));
            }
        }
        Ok(response)
    }
}

fn sim(s2: bool) -> SimHandle {
    let model = if s2 {
        SimModel::One
    } else {
        SimModel::Play5Gen1
    };
    let players = [
        SimPlayerSpec::new("Room", model),
        SimPlayerSpec::new("Peer", model),
    ];
    let builder = SimHousehold::builder();
    let builder = if s2 {
        builder.s2(players)
    } else {
        builder.s1(players)
    };
    builder.spawn().unwrap()
}

fn house(sim: &SimHandle) -> HouseholdState {
    let mut house = HouseholdState::default();
    house.apply_topology(
        &get_zone_group_state(&sim.lan(), sim.player("Room").unwrap().ip).unwrap(),
    );
    house
}

fn surface(
    sim: &SimHandle,
    replace: bool,
    store: MemStore,
    policy: Policy,
) -> (Surface, Arc<AtomicUsize>) {
    let host = sim.player("Room").unwrap().ip;
    let survey: Survey = Box::new(move |transport| {
        let mut house = HouseholdState::default();
        house.apply_topology(
            &get_zone_group_state(transport, host).map_err(fsonos_core::CoreError::from)?,
        );
        Ok(vec![house])
    });
    let reads = Arc::new(AtomicUsize::new(0));
    let surface = Surface::new(
        Box::new(ChangedFavorites {
            inner: sim.lan(),
            reads: Arc::clone(&reads),
            replace,
        }),
        survey,
        policy,
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(store), "test");
    (surface, reads)
}

fn request(query: &str) -> SearchPlayRequest {
    SearchPlayRequest {
        zone: "Room".into(),
        query: query.into(),
        pick: None,
    }
}

fn library() -> MemStore {
    let mut store = MemStore::default();
    let entries: Vec<_> = TRACKS
        .iter()
        .enumerate()
        .map(|(index, uri)| LibraryEntry {
            track: Track {
                source_uri: (*uri).into(),
                title: format!("Recorded Selection {}", index + 1),
                artist: Some("Test Ensemble".into()),
                album: None,
                duration_secs: Some(180),
                uri: None,
            },
            ..LibraryEntry::default()
        })
        .collect();
    store.upsert_library(&entries).unwrap();
    store
}

fn writes(sim: &SimHandle) -> Vec<String> {
    sim.soap_log()
        .into_iter()
        .filter(|entry| !entry.action.starts_with("Get") && entry.action != "Browse")
        .map(|entry| entry.action)
        .collect()
}

#[test]
fn favorite_playback_keeps_the_searched_uri_and_didl_on_both_generations() {
    for s2 in [false, true] {
        let sim = sim(s2);
        let host = sim.player("Room").unwrap().ip;
        let houses = [house(&sim)];
        let coordinator = &resolve_room(&houses, "Room").unwrap().coordinator.id;
        let listed = favorites::list(&sim.lan(), &houses, coordinator).unwrap();
        let original = listed
            .iter()
            .find(|favorite| favorite.title == "Sim Radio")
            .unwrap();
        let (surface, reads) = surface(&sim, true, MemStore::default(), Policy::default());

        let done = surface
            .search_and_play(&Client::Cli, &request("Sim Radio"))
            .unwrap();

        assert!(done.changed);
        assert_eq!(
            reads.load(Ordering::SeqCst),
            1,
            "do not look an FV:2 ID up twice"
        );
        let media = soap::get_media_info(&sim.lan(), host).unwrap();
        assert_eq!(media.uri, RADIO);
        assert_eq!(media.uri_metadata, original.metadata);
        let log = surface
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        assert_eq!(log.len(), 1);
        assert!(log[0].action.intent.starts_with("play_favorite"));
        assert!(log[0].action.before_state.is_some());
        let history = surface
            .recent_plays(&Client::Cli, Some("Room"), 10)
            .unwrap();
        assert_eq!(history.len(), 1);
        assert_eq!(history[0].source_uri, RADIO);
        sim.shutdown();
    }
}

#[test]
fn a_ranked_library_track_uses_the_households_renderer_and_is_recorded() {
    for s2 in [false, true] {
        let sim = sim(s2);
        let (surface, _) = surface(&sim, false, library(), Policy::default());
        let mut req = request("Recorded Selection");
        req.pick = Some(2);

        surface.search_and_play(&Client::Cli, &req).unwrap();

        let position =
            soap::get_position_info(&sim.lan(), sim.player("Room").unwrap().ip).unwrap();
        assert_eq!(
            spotify_uri_from_renderer_uri(&position.uri).as_deref(),
            Some(TRACKS[1])
        );
        let history = surface
            .recent_plays(&Client::Cli, Some("Room"), 10)
            .unwrap();
        assert_eq!(history[0].source_uri, TRACKS[1]);
        let log = surface
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        assert!(log[0].action.intent.starts_with("play:"));
        sim.shutdown();
    }
}

#[test]
fn a_missing_rank_never_substitutes_the_best_match_or_changes_playback() {
    let sim = sim(true);
    let (surface, _) = surface(&sim, false, library(), Policy::default());
    let mut req = request("Recorded Selection");
    req.pick = Some(3);
    let before = writes(&sim);
    assert_eq!(
        surface.search_and_play(&Client::Cli, &req).unwrap_err().code,
        ErrorCode::NoMatch
    );
    assert_eq!(writes(&sim), before);
    assert!(
        surface
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap()
            .is_empty()
    );
    sim.shutdown();
}

#[test]
fn search_play_preserves_group_caps_and_undo_for_a_member_request() {
    for s2 in [false, true] {
        let sim = sim(s2);
        let hosts = [
            sim.player("Room").unwrap().ip,
            sim.player("Peer").unwrap().ip,
        ];
        let houses = [house(&sim)];
        let coordinator = resolve_room(&houses, "Room").unwrap().coordinator.id.clone();
        soap::set_av_transport_uri(
            &sim.lan(),
            hosts[1],
            &format!("x-rincon:{}", coordinator.0),
            "",
        )
        .unwrap();
        soap::set_av_transport_uri(&sim.lan(), hosts[0], REPLACEMENT, "").unwrap();
        soap::play(&sim.lan(), hosts[0]).unwrap();
        for host in hosts {
            soap::set_volume(&sim.lan(), host, 85).unwrap();
        }
        let (surface, _) = surface(&sim, false, MemStore::default(), Policy::default());
        let mut req = request("Sim Radio");
        req.zone = "Peer".into();
        let done = surface.search_and_play(&Client::McpStdio, &req).unwrap();
        assert!(done.changed);
        assert!(!done.notes.is_empty());
        for host in hosts {
            assert_eq!(soap::get_volume(&sim.lan(), host).unwrap(), 70);
        }
        assert_eq!(
            soap::get_media_info(&sim.lan(), hosts[0]).unwrap().uri,
            RADIO
        );
        let undone = surface.undo(&Client::McpStdio, true).unwrap().unwrap();
        assert!(undone.failures.is_empty(), "{undone:?}");
        assert_eq!(
            soap::get_media_info(&sim.lan(), hosts[0]).unwrap().uri,
            REPLACEMENT
        );
        for host in hosts {
            assert!(soap::get_volume(&sim.lan(), host).unwrap() <= 70);
        }
        sim.shutdown();
    }
}

#[test]
fn permission_for_favorites_does_not_grant_library_playback_or_change_the_rank() {
    let sim = sim(true);
    let policy = Policy::from_toml("[clients.cli]\ndeny = ['play']\n").unwrap();
    let (surface, _) = surface(&sim, false, library(), policy);
    assert_eq!(
        surface
            .search_and_play(&Client::Cli, &request("Recorded Selection"))
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    assert!(writes(&sim).is_empty());
    surface
        .search_and_play(&Client::Cli, &request("Sim Radio"))
        .unwrap();
    sim.shutdown();
}

#[test]
fn permission_for_library_playback_does_not_grant_favorite_playback() {
    let sim = sim(false);
    let policy = Policy::from_toml("[clients.cli]\ndeny = ['play_favorite']\n").unwrap();
    let (surface, _) = surface(&sim, false, library(), policy);
    assert_eq!(
        surface
            .search_and_play(&Client::Cli, &request("Sim Radio"))
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    assert!(writes(&sim).is_empty());
    surface
        .search_and_play(&Client::Cli, &request("Recorded Selection"))
        .unwrap();
    sim.shutdown();
}

struct NoIo;
impl Transport for NoIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("denied or invalid search playback must not contact speakers")
    }
}

#[test]
fn read_only_callers_are_refused_and_logged_before_discovery() {
    let surface = Surface::new(
        Box::new(NoIo),
        Box::new(|_| panic!("read-only caller must not survey")),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "test");
    assert_eq!(
        surface
            .search_and_play(&Client::Unknown, &request("Sim Radio"))
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    let log = surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(log.len(), 1);
    assert_eq!(log[0].action.intent, "search_and_play");
    assert!(log[0].action.decision.starts_with("deny:"));
    assert!(log[0].action.before_state.is_none());
}

#[test]
fn malformed_requests_fail_without_discovery_or_speaker_io() {
    let surface = Surface::new(
        Box::new(NoIo),
        Box::new(|_| panic!("invalid request must not survey")),
        Policy::default(),
        Box::new(SystemClock),
    );
    for req in [
        SearchPlayRequest {
            zone: " ".into(),
            ..request("Radio")
        },
        request(" "),
        SearchPlayRequest {
            pick: Some(0),
            ..request("Radio")
        },
        SearchPlayRequest {
            pick: Some(51),
            ..request("Radio")
        },
    ] {
        assert_eq!(
            surface.search_and_play(&Client::Cli, &req).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
    }
}
