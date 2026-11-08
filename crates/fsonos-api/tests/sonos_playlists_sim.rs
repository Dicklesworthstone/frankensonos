//! Native saved playlists across both households, through the policy and
//! action-log boundary and real localhost simulator sockets.

use fsonos_api::surface::queue::QueueRequest;
use fsonos_api::surface::sonos_playlists::PlaySonosPlaylistRequest;
use fsonos_api::{ErrorCode, Surface};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, inventory, resolve_room};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::TransportState;
use std::net::IpAddr;
use std::time::Duration;

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([
            SimPlayerSpec::new("Den", SimModel::Play5Gen1),
            SimPlayerSpec::new("Office", SimModel::Play5Gen1),
        ])
        .s2([
            SimPlayerSpec::new("Den", SimModel::One),
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

fn surface(
    t: Box<dyn Transport + Send + Sync>,
    houses: &[HouseholdState],
    policy: Policy,
) -> Surface {
    let known = houses.to_vec();
    Surface::new(
        t,
        Box::new(move |_| Ok(known.clone())),
        policy,
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "http")
}

fn queue(sim: &SimHandle, houses: &[HouseholdState], zone: &str) -> Vec<String> {
    let target = resolve_room(houses, zone).unwrap();
    browse_all(&sim.lan(), target.coordinator.ip, QUEUE)
        .unwrap()
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

fn play(zone: &str, playlist: &str) -> PlaySonosPlaylistRequest {
    PlaySonosPlaylistRequest {
        zone: zone.into(),
        playlist: playlist.into(),
    }
}

#[test]
fn catalogs_stay_household_local_and_work_without_favorites_or_spotify_sign_in() {
    let sim = sim();
    sim.retain_favorites(1, |_| false).unwrap();
    sim.retain_favorites(2, |_| false).unwrap();
    let houses = households(&sim);
    let s = surface(Box::new(sim.lan()), &houses, Policy::default());
    for generation in ["S1", "S2"] {
        let zone = format!("Den@{generation}");
        assert!(s.favorites(&Client::McpStdio, &zone).unwrap().is_empty());
        let list = s.sonos_playlists(&Client::McpStdio, &zone).unwrap();
        assert_eq!(list.len(), 2);
        assert_eq!(list[0].id, "SQ:1");
        assert_eq!(list[0].title, format!("Saved Mix {generation}"));
        assert_eq!(list[1].id, "SQ:7");
        assert!(queue(&sim, &houses, &zone).is_empty());
    }
    assert_eq!(
        s.sonos_playlists(&Client::McpStdio, "Den")
            .unwrap_err()
            .code,
        ErrorCode::AmbiguousRoom
    );
    assert!(
        s.recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn native_play_and_next_resolve_members_and_share_caps_logging_and_undo() {
    for generation in ["S1", "S2"] {
        let sim = sim();
        let zone = format!("Den@{generation}");
        let member_zone = format!("Office@{generation}");
        let initial = households(&sim);
        let lead = resolve_room(&initial, &zone).unwrap();
        let member = resolve_room(&initial, &member_zone).unwrap();
        soap::join_group(&sim.lan(), member.player.ip, &lead.coordinator.id).unwrap();
        let host = lead.coordinator.ip;
        let member_host = member.player.ip;
        soap::set_volume(&sim.lan(), host, 85).unwrap();
        soap::set_volume(&sim.lan(), member_host, 80).unwrap();
        let houses = households(&sim);
        let mut policy = Policy::default();
        policy.defaults.max_volume = 30;
        let s = surface(Box::new(sim.lan()), &houses, policy);
        sim.retain_favorites(1, |_| false).unwrap();
        sim.retain_favorites(2, |_| false).unwrap();
        let played = s
            .play_sonos_playlist(&Client::McpStdio, &play(&member_zone, "SQ:7"))
            .unwrap();
        assert!(played.changed && played.done.contains("Sonos playlist"));
        let saved = queue(&sim, &houses, &zone);
        assert_eq!(saved.len(), 3);
        assert_eq!(
            saved[0], saved[2],
            "duplicate saved occurrences retain their order"
        );
        assert_ne!(saved[0], saved[1]);
        assert_eq!(
            soap::get_transport_info(&sim.lan(), host).unwrap().state,
            TransportState::Playing
        );
        assert_eq!(soap::get_volume(&sim.lan(), host).unwrap(), 30);
        assert_eq!(soap::get_volume(&sim.lan(), member_host).unwrap(), 30);
        let before_track = soap::get_position_info(&sim.lan(), host).unwrap().uri;
        let added = s
            .queue_request(
                &Client::McpStdio,
                &QueueRequest {
                    zone: member_zone.clone(),
                    playlist: Some("SQ:1".into()),
                    next: true,
                    ..QueueRequest::default()
                },
            )
            .unwrap();
        assert_eq!(
            (added.first, added.count, added.playing_queue),
            (2, 2, true)
        );
        assert_eq!(queue(&sim, &houses, &zone).len(), 5);
        assert_eq!(
            soap::get_position_info(&sim.lan(), host).unwrap().uri,
            before_track
        );
        let log = s
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        assert_eq!(log.len(), 2);
        assert!(log.iter().all(|row| row.action.client == "mcp-stdio"));
        assert!(
            log.iter()
                .all(|row| row.action.intent.contains("Sonos playlist"))
        );
        let undone = s.undo(&Client::McpStdio, true).unwrap().unwrap();
        assert!(undone.unqueue_failures.is_empty());
        assert_eq!(queue(&sim, &houses, &zone), saved);
        let other = if generation == "S1" {
            "Den@S2"
        } else {
            "Den@S1"
        };
        assert!(queue(&sim, &houses, other).is_empty());
    }
}

struct NoSpeakerIo;

impl Transport for NoSpeakerIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("denied or invalid native playlist requests must not touch speakers")
    }
}

#[test]
fn native_playlists_obey_existing_favorite_and_queue_permissions() {
    let policy = Policy::from_toml("[clients.\"mcp-stdio\"]\nallow = []\n").unwrap();
    let s = surface(Box::new(NoSpeakerIo), &[], policy);
    assert_eq!(
        s.sonos_playlists(&Client::McpStdio, "Den")
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    assert_eq!(
        s.play_sonos_playlist(&Client::McpStdio, &play("Den", "SQ:1"))
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    assert_eq!(
        s.queue_request(
            &Client::McpStdio,
            &QueueRequest {
                zone: "Den".into(),
                playlist: Some("SQ:1".into()),
                ..QueueRequest::default()
            }
        )
        .unwrap_err()
        .code,
        ErrorCode::PolicyDenied
    );
    assert_eq!(
        s.play_sonos_playlist(&Client::Cli, &play("Den", ""))
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
}

#[test]
fn unknown_native_selections_do_not_lower_volume_or_replace_the_queue() {
    let sim = sim();
    let houses = households(&sim);
    let target = resolve_room(&houses, "Den@S2").unwrap();
    soap::set_volume(&sim.lan(), target.player.ip, 90).unwrap();
    let s = surface(Box::new(sim.lan()), &houses, Policy::default());
    let failed = s
        .play_sonos_playlist(&Client::McpStdio, &play("Den@S2", "SQ:999"))
        .unwrap_err();
    assert_eq!(failed.code, ErrorCode::NoMatch);
    assert_eq!(soap::get_volume(&sim.lan(), target.player.ip).unwrap(), 90);
    assert!(queue(&sim, &houses, "Den@S2").is_empty());
    assert!(
        s.recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap()
            .is_empty()
    );
}
