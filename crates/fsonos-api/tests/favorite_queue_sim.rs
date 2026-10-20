//! Queue native favorites through the shared HTTP/MCP surface and real
//! simulator sockets, with household resolution, policy, action logging and undo.

use fsonos_api::surface::queue::QueueRequest;
use fsonos_api::{ErrorCode, Surface};
use fsonos_core::actions::BeforeState;
use fsonos_core::clock::SystemClock;
use fsonos_core::favorites::{self, Favorite, FavoriteKind};
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control, inventory, resolve_room};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::net::IpAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Den", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([
            SimPlayerSpec::pair("Den", SimModel::One),
            SimPlayerSpec::new("Office", SimModel::Play1),
        ])
        .spawn()
        .unwrap()
}

fn households(sim: &SimHandle) -> Vec<HouseholdState> {
    inventory::survey(&sim.lan(), &[], Duration::from_millis(500))
        .unwrap()
        .households
}

fn surface(t: Box<dyn Transport + Send + Sync>, houses: &[HouseholdState]) -> Surface {
    let known = houses.to_vec();
    Surface::new(
        t,
        Box::new(move |_| Ok(known.clone())),
        Policy::default(),
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "http")
}

fn queue(sim: &SimHandle, houses: &[HouseholdState], zone: &str) -> Vec<String> {
    let player = resolve_room(houses, zone).unwrap();
    browse_all(&sim.lan(), player.coordinator.ip, QUEUE)
        .unwrap()
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

fn favorite(
    sim: &SimHandle,
    houses: &[HouseholdState],
    zone: &str,
    kind: FavoriteKind,
) -> Favorite {
    let player = resolve_room(houses, zone).unwrap();
    favorites::list(&sim.lan(), houses, &player.coordinator.id)
        .unwrap()
        .into_iter()
        .find(|favorite| favorite.kind == kind)
        .unwrap()
}

fn request(zone: &str, favorite: &str, next: bool) -> QueueRequest {
    QueueRequest {
        zone: zone.into(),
        favorite: Some(favorite.into()),
        next,
        ..QueueRequest::default()
    }
}

fn seed(sim: &SimHandle, houses: &[HouseholdState], zone: &str) -> Vec<String> {
    let target = resolve_room(houses, zone).unwrap();
    control::queue_spotify_tracks(
        &sim.lan(),
        houses,
        &target.coordinator.id,
        &[
            ("spotify:track:0SimQueueA000000000001", "Original first"),
            ("spotify:track:0SimQueueB000000000002", "Original second"),
        ],
    )
    .unwrap();
    control::play_queue_from(&sim.lan(), houses, &target.coordinator.id, 1).unwrap();
    queue(sim, houses, zone)
}

#[test]
fn favorite_add_and_undo_leave_music_and_the_other_household_untouched() {
    for zone in ["Den@S1", "Den@S2"] {
        for next in [false, true] {
            let sim = sim();
            let houses = households(&sim);
            let original = seed(&sim, &houses, zone);
            let selected = favorite(&sim, &houses, zone, FavoriteKind::Container);
            let generation = if zone.ends_with("S1") { 1 } else { 2 };
            sim.retain_favorites(generation, |uri| uri.starts_with("x-rincon-cpcontainer:"))
                .unwrap();
            let s = surface(Box::new(sim.lan()), &houses);
            let added = s
                .queue_request(&Client::McpStdio, &request(zone, &selected.id, next))
                .unwrap();
            assert_eq!(added.first, if next { 2 } else { 3 });
            assert!(added.count > 1 && added.playing_queue);
            let target = resolve_room(&houses, zone).unwrap();
            let now = control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap();
            assert_eq!(now.transport.state, TransportState::Playing);
            assert_eq!(now.position.uri, original[0]);
            let other = if zone.ends_with("S1") {
                "Den@S2"
            } else {
                "Den@S1"
            };
            assert!(queue(&sim, &houses, other).is_empty());
            let log = s
                .recent_actions(&Client::Cli, &ActionFilter::default())
                .unwrap();
            assert_eq!(log.len(), 1);
            assert_eq!(log[0].action.client, "mcp-stdio");
            let before =
                BeforeState::parse(log[0].action.before_state.as_deref().unwrap()).unwrap();
            assert_eq!(before.queued.len(), 1);
            assert_eq!(before.queued[0].first, added.first);
            assert_eq!(before.queued[0].uris.len(), added.count as usize);
            let undo = s.undo(&Client::McpStdio, true).unwrap().unwrap();
            assert_eq!(undo.undone, log[0].id);
            assert!(undo.unqueue_failures.is_empty());
            assert_eq!(queue(&sim, &houses, zone), original);
        }
    }
}

#[test]
fn a_member_queue_add_resolves_fresh_grouping_instead_of_its_cached_coordinator() {
    let sim = sim();
    let houses = households(&sim);
    let original = seed(&sim, &houses, "Den@S2");
    let selected = favorite(&sim, &houses, "Den@S2", FavoriteKind::Track);
    let s = surface(Box::new(sim.lan()), &houses);
    s.households().unwrap();
    let lead = resolve_room(&houses, "Den@S2").unwrap();
    let member = resolve_room(&houses, "Office@S2").unwrap();
    soap::join_group(&sim.lan(), member.player.ip, &lead.coordinator.id).unwrap();
    let added = s
        .queue_request(&Client::McpStdio, &request("Office@S2", &selected.id, true))
        .unwrap();
    assert_eq!(added.zone, "Den");
    assert_eq!(added.first, 2);
    assert_eq!(added.count, 1);
    let current = households(&sim);
    let now = queue(&sim, &current, "Den@S2");
    assert_eq!(now.len(), 3);
    assert_eq!(now[0], original[0]);
    assert_eq!(now[2], original[1]);
    assert!(queue(&sim, &current, "Office@S1").is_empty());
}

#[test]
fn original_spotify_bodies_still_queue_through_the_unified_surface() {
    let sim = sim();
    let houses = households(&sim);
    let s = surface(Box::new(sim.lan()), &houses);
    let req: QueueRequest = serde_json::from_value(serde_json::json!({
        "zone": "Den@S1", "source_uri": "spotify:track:0SimQueueTrack00000021",
        "title": "Existing client", "next": false
    }))
    .unwrap();
    let added = s.queue_request(&Client::McpStdio, &req).unwrap();
    assert_eq!(
        (added.first, added.count, added.playing_queue),
        (1, 1, false)
    );
    assert_eq!(queue(&sim, &houses, "Den@S1").len(), 1);
}

struct NoSpeakerIo;

impl Transport for NoSpeakerIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("authorization and input validation must precede speaker I/O")
    }
}

#[test]
fn denied_callers_are_logged_and_invalid_source_choices_do_no_speaker_io() {
    let s = surface(Box::new(NoSpeakerIo), &[]);
    let error = s
        .queue_request(&Client::Unknown, &request("Den@S2", "FV:2/1", true))
        .unwrap_err();
    assert_eq!(error.code, ErrorCode::PolicyDenied);
    let log = s
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(log.len(), 1);
    assert!(log[0].action.decision.starts_with("deny:"));
    assert!(log[0].action.before_state.is_none());
    let mut req = request("Den@S2", "FV:2/1", false);
    req.source_uri = Some("spotify:track:0SimQueueTrack00000021".into());
    assert_eq!(
        s.queue_request(&Client::McpStdio, &req).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
}

#[test]
fn ambiguous_rooms_unknown_favorites_and_stations_cannot_modify_a_queue() {
    let sim = sim();
    let houses = households(&sim);
    let s = surface(Box::new(sim.lan()), &houses);
    assert_eq!(
        s.queue_request(&Client::McpStdio, &request("Den", "FV:2/1", false))
            .unwrap_err()
            .code,
        ErrorCode::AmbiguousRoom
    );
    assert_eq!(
        s.queue_request(&Client::McpStdio, &request("Den@S1", "FV:2/999999", false))
            .unwrap_err()
            .code,
        ErrorCode::UnknownFavorite
    );
    let station = favorite(&sim, &houses, "Den@S2", FavoriteKind::Stream);
    assert_eq!(
        s.queue_request(&Client::McpStdio, &request("Den@S2", &station.id, false))
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
    for zone in ["Den@S1", "Den@S2", "Office@S1", "Office@S2"] {
        assert!(queue(&sim, &houses, zone).is_empty());
    }
    let log = s
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(log.len(), 3);
    assert!(log.iter().all(|entry| entry.action.before_state.is_none()));
}

struct LostAdd {
    lan: SimLan,
    adds: Arc<AtomicUsize>,
}

impl Transport for LostAdd {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.lan.soap_post(host, path, action, body)?;
        if action.contains("#AddURIToQueue") {
            self.adds.fetch_add(1, Ordering::SeqCst);
            Err(ProtoError::Network {
                target: host.to_string(),
                detail: "injected lost add reply".into(),
            })
        } else {
            Ok(response)
        }
    }
}

#[test]
fn uncertain_container_adds_are_not_replayed_or_given_a_fabricated_undo_range() {
    let sim = sim();
    let houses = households(&sim);
    let selected = favorite(&sim, &houses, "Den@S2", FavoriteKind::Container);
    let adds = Arc::new(AtomicUsize::new(0));
    let s = surface(
        Box::new(LostAdd {
            lan: sim.lan(),
            adds: Arc::clone(&adds),
        }),
        &houses,
    );
    let failure = s
        .queue_request(&Client::McpStdio, &request("Den@S2", &selected.id, false))
        .unwrap_err();
    assert!(!failure.retryable());
    assert!(failure.detail.contains("uncertain"));
    assert_eq!(adds.load(Ordering::SeqCst), 1);
    assert!(!queue(&sim, &houses, "Den@S2").is_empty());
    let log = s
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(log.len(), 1);
    assert!(log[0].action.result.contains("uncertain"));
    // No fabricated undo range, and undo does not reach past it either.
    assert!(log[0].action.before_state.is_some());
    assert!(!fsonos_core::actions::undoable(
        log[0].action.before_state.as_deref()
    ));
}
