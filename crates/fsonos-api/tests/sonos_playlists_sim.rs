//! Native saved playlists across both households, through the policy and
//! action-log boundary and real localhost simulator sockets.

use fsonos_api::surface::queue::QueueRequest;
use fsonos_api::surface::sonos_playlists::{
    PlaySonosPlaylistRequest, SaveSonosPlaylistRequest, SonosPlaylistContentsRequest,
};
use fsonos_api::{ErrorCode, Surface};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control, inventory, resolve_room};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::control as soap;
use fsonos_proto::{ProtoError, Transport};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
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

fn contents(zone: &str, playlist: &str, limit: u32) -> SonosPlaylistContentsRequest {
    SonosPlaylistContentsRequest {
        zone: zone.into(),
        playlist: playlist.into(),
        offset: 0,
        limit,
        expected_revision: None,
    }
}

#[test]
fn saved_contents_retain_duplicate_occurrences_without_touching_playback() {
    let sim = sim();
    sim.retain_favorites(1, |_| false).unwrap();
    sim.retain_favorites(2, |_| false).unwrap();
    let houses = households(&sim);
    let s = surface(Box::new(sim.lan()), &houses, Policy::default());
    let mut revisions = Vec::new();
    for generation in ["S1", "S2"] {
        let zone = format!("Den@{generation}");
        let host = resolve_room(&houses, &zone).unwrap().coordinator.ip;
        soap::set_volume(&sim.lan(), host, 72).unwrap();
        soap::set_mute(&sim.lan(), host, true).unwrap();
        let before = soap::get_transport_info(&sim.lan(), host).unwrap();
        let page = s
            .sonos_playlist_contents(&Client::McpStdio, &contents(&zone, "SQ:7", 2))
            .unwrap();
        assert_eq!(page.playlist.id, "SQ:7");
        assert_eq!(page.total, 3);
        assert_eq!(page.offset, 0);
        assert_eq!(page.next_offset, Some(2));
        assert_eq!(
            page.items
                .iter()
                .map(|item| item.position)
                .collect::<Vec<_>>(),
            [1, 2]
        );
        assert!(page.items.iter().all(|item| item.uri.is_some()));
        assert!(!page.revision.is_empty());
        let next = SonosPlaylistContentsRequest {
            offset: page.next_offset.unwrap(),
            expected_revision: Some(page.revision.clone()),
            ..contents(&zone, &page.playlist.id, 2)
        };
        let last = s.sonos_playlist_contents(&Client::McpStdio, &next).unwrap();
        assert_eq!(last.revision, page.revision);
        assert_eq!(last.total, 3);
        assert_eq!(last.items.len(), 1);
        assert_eq!(last.items[0].position, 3);
        assert_eq!(last.items[0].uri, page.items[0].uri);
        assert_eq!(last.next_offset, None);
        assert_eq!(soap::get_transport_info(&sim.lan(), host).unwrap(), before);
        assert_eq!(soap::get_volume(&sim.lan(), host).unwrap(), 72);
        assert!(soap::get_mute(&sim.lan(), host).unwrap());
        assert!(queue(&sim, &houses, &zone).is_empty());
        revisions.push(page.revision);
    }
    assert_ne!(
        revisions[0], revisions[1],
        "equal native ids stay household-local"
    );
    assert!(
        s.recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap()
            .is_empty()
    );
}

#[test]
fn native_contents_resolve_member_rooms_and_initial_titles_but_refuse_stale_continuations() {
    let sim = sim();
    let initial = households(&sim);
    let lead = resolve_room(&initial, "Den@S2").unwrap();
    let member = resolve_room(&initial, "Office@S2").unwrap();
    soap::join_group(&sim.lan(), member.player.ip, &lead.coordinator.id).unwrap();
    let houses = households(&sim);
    let s = surface(Box::new(sim.lan()), &houses, Policy::default());
    let page = s
        .sonos_playlist_contents(&Client::McpStdio, &contents("Office@S2", "Saved Mix S2", 1))
        .unwrap();
    assert_eq!(page.zone, "Den");
    assert_eq!(page.playlist.id, "SQ:1");
    assert_eq!(page.total, 2);
    let same = s
        .sonos_playlist_contents(&Client::McpStdio, &contents("Den@S2", "SQ:1", 1))
        .unwrap();
    assert_eq!(same.revision, page.revision);

    for (zone, id, revision) in [
        ("Den@S2", "SQ:1", "stale revision".to_owned()),
        ("Den@S1", "SQ:1", page.revision.clone()),
        ("Den@S2", "SQ:7", page.revision.clone()),
    ] {
        let request = SonosPlaylistContentsRequest {
            offset: 1,
            expected_revision: Some(revision),
            ..contents(zone, id, 1)
        };
        let failure = s
            .sonos_playlist_contents(&Client::McpStdio, &request)
            .unwrap_err();
        assert_eq!(failure.code, ErrorCode::QueueChanged, "{failure:?}");
        assert!(failure.hint.contains("saved Sonos playlist"));
    }
    assert_eq!(
        s.sonos_playlist_contents(&Client::Cli, &contents("Den", "SQ:1", 1))
            .unwrap_err()
            .code,
        ErrorCode::AmbiguousRoom
    );
    assert_eq!(
        s.sonos_playlist_contents(&Client::Cli, &contents("Den@S2", "missing title", 1))
            .unwrap_err()
            .code,
        ErrorCode::NoMatch
    );
    assert!(queue(&sim, &houses, "Den@S2").is_empty());
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
        s.sonos_playlist_contents(&Client::McpStdio, &contents("Den", "SQ:7", 100))
            .unwrap_err()
            .code,
        ErrorCode::PolicyDenied
    );
    for request in [
        contents("Den", "SQ:7", 101),
        SonosPlaylistContentsRequest {
            offset: 1,
            ..contents("Den", "SQ:7", 100)
        },
        SonosPlaylistContentsRequest {
            expected_revision: Some("revision".into()),
            ..contents("Den", "Evening", 100)
        },
    ] {
        assert_eq!(
            s.sonos_playlist_contents(&Client::Cli, &request)
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }
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

fn save(zone: &str, title: &str) -> SaveSonosPlaylistRequest {
    SaveSonosPlaylistRequest {
        zone: zone.into(),
        title: title.into(),
    }
}

fn seed_native_queue(s: &Surface, zone: &str) {
    s.queue_request(
        &Client::McpStdio,
        &QueueRequest {
            zone: zone.into(),
            playlist: Some("SQ:7".into()),
            ..QueueRequest::default()
        },
    )
    .unwrap();
}

#[test]
fn saves_resolve_new_grouping_and_log_only_the_new_household_catalog_item() {
    for generation in ["S1", "S2"] {
        let sim = sim();
        sim.retain_favorites(1, |_| false).unwrap();
        sim.retain_favorites(2, |_| false).unwrap();
        let zone = format!("Den@{generation}");
        let member_zone = format!("Office@{generation}");
        let initial = households(&sim);
        let lead = resolve_room(&initial, &zone).unwrap();
        let member = resolve_room(&initial, &member_zone).unwrap();
        soap::join_group(&sim.lan(), member.player.ip, &lead.coordinator.id).unwrap();
        let mut policy = Policy::default();
        policy.defaults.max_volume = 20;
        // Deliberately supply topology from before Office joined Den. The save
        // must resolve Office's current coordinator before reading its queue.
        let s = surface(Box::new(sim.lan()), &initial, policy);
        seed_native_queue(&s, &zone);
        control::play_queue_from(&sim.lan(), &initial, &lead.coordinator.id, 2).unwrap();
        soap::seek_position(&sim.lan(), lead.coordinator.ip, 37).unwrap();
        soap::set_volume(&sim.lan(), lead.coordinator.ip, 85).unwrap();
        soap::set_volume(&sim.lan(), member.player.ip, 80).unwrap();
        let before = control::playback(&sim.lan(), &initial, &lead.coordinator.id).unwrap();
        let contents = queue(&sim, &initial, &zone);
        let undo = s.preview_undo(&Client::McpStdio).unwrap().unwrap().id;
        let start = sim.soap_log().len();
        let saved = s
            .save_sonos_playlist(
                &Client::McpStdio,
                &save(&member_zone, "  Evening & Friends  "),
            )
            .unwrap();
        assert!(saved.logged);
        assert_eq!(saved.zone, "Den");
        assert_eq!(saved.playlist.id, "SQ:8");
        assert_eq!(saved.playlist.title, "Evening & Friends");
        let writes: Vec<_> = sim.soap_log()[start..]
            .iter()
            .filter(|entry| !matches!(entry.action.as_str(), "GetZoneGroupState" | "Browse"))
            .cloned()
            .collect();
        assert_eq!(writes.len(), 1, "{writes:?}");
        assert_eq!(writes[0].action, "SaveQueue");
        assert_eq!(writes[0].player, lead.coordinator.id.0);
        assert_eq!(
            control::playback(&sim.lan(), &initial, &lead.coordinator.id).unwrap(),
            before
        );
        assert_eq!(queue(&sim, &initial, &zone), contents);
        assert_eq!(
            soap::get_volume(&sim.lan(), lead.coordinator.ip).unwrap(),
            85
        );
        assert_eq!(soap::get_volume(&sim.lan(), member.player.ip).unwrap(), 80);
        assert_eq!(s.preview_undo(&Client::McpStdio).unwrap().unwrap().id, undo);
        let log = s
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        let entry = log
            .iter()
            .find(|row| row.action.intent.starts_with("save_sonos_playlist:"))
            .unwrap();
        assert_eq!(entry.action.client, "mcp-stdio");
        assert_eq!(entry.action.decision, "allow");
        assert!(entry.action.before_state.is_none());
        assert!(entry.action.result.contains(&saved.playlist.id));
        let other = if generation == "S1" {
            "Den@S2"
        } else {
            "Den@S1"
        };
        assert_eq!(s.sonos_playlists(&Client::Cli, other).unwrap().len(), 2);
        // A new Surface still sees the playlist: the catalog lives on Sonos.
        let reopened = surface(Box::new(sim.lan()), &initial, Policy::default());
        assert!(
            reopened
                .sonos_playlists(&Client::Cli, &zone)
                .unwrap()
                .contains(&saved.playlist)
        );
    }
}

#[test]
fn save_permission_and_title_validation_precede_all_speaker_io() {
    for rule in [
        "deny = ['save_sonos_playlist']",
        "deny = ['house-config']",
        "allow = ['read']",
    ] {
        let policy = Policy::from_toml(&format!("[clients.\"mcp-stdio\"]\n{rule}\n")).unwrap();
        let s = surface(Box::new(NoSpeakerIo), &[], policy);
        let failure = s
            .save_sonos_playlist(&Client::McpStdio, &save("Den", "Evening"))
            .unwrap_err();
        assert_eq!(failure.code, ErrorCode::PolicyDenied);
        let log = s
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        assert_eq!(log.len(), 1);
        assert!(log[0].action.decision.starts_with("deny:"));
    }
    let s = surface(Box::new(NoSpeakerIo), &[], Policy::default());
    for request in [save("Den", ""), save("Den", "A\nB"), save(" ", "Evening")] {
        assert_eq!(
            s.save_sonos_playlist(&Client::Cli, &request)
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }
}

struct LoseSaveReply(SimLan);

impl Transport for LoseSaveReply {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let response = self.0.soap_post(host, path, action, body)?;
        if action.ends_with("#SaveQueue\"") {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "save reply lost after persisting".into(),
            });
        }
        Ok(response)
    }
}

#[test]
fn an_unconfirmed_save_is_logged_without_retry_or_playback_undo_barriers() {
    let sim = sim();
    let houses = households(&sim);
    let s = surface(
        Box::new(LoseSaveReply(sim.lan())),
        &houses,
        Policy::default(),
    );
    seed_native_queue(&s, "Den@S2");
    let undo = s.preview_undo(&Client::McpStdio).unwrap().unwrap().id;
    let failure = s
        .save_sonos_playlist(&Client::McpStdio, &save("Den@S2", "Keep this set"))
        .unwrap_err();
    assert_eq!(failure.code, ErrorCode::OutcomeUncertain);
    assert!(!failure.retryable());
    assert!(failure.hint.contains("saved Sonos playlists"));
    assert_eq!(
        sim.soap_log()
            .iter()
            .filter(|entry| entry.action == "SaveQueue")
            .count(),
        1
    );
    let catalog = s.sonos_playlists(&Client::Cli, "Den@S2").unwrap();
    assert_eq!(
        catalog
            .iter()
            .filter(|item| item.title == "Keep this set")
            .count(),
        1
    );
    assert_eq!(s.preview_undo(&Client::McpStdio).unwrap().unwrap().id, undo);
    let log = s
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    let entry = log
        .iter()
        .find(|row| row.action.intent.starts_with("save_sonos_playlist:"))
        .unwrap();
    assert!(entry.action.result.starts_with("failed:"));
    assert!(entry.action.before_state.is_none());
}

#[test]
fn without_a_local_log_the_native_save_still_returns_its_confirmed_identity() {
    let sim = sim();
    let houses = households(&sim);
    let known = houses.clone();
    let s = Surface::new(
        Box::new(sim.lan()),
        Box::new(move |_| Ok(known.clone())),
        Policy::default(),
        Box::new(SystemClock),
    );
    seed_native_queue(&s, "Den@S1");
    let saved = s
        .save_sonos_playlist(&Client::Cli, &save("Den@S1", "Saved without a log"))
        .unwrap();
    assert!(!saved.logged);
    assert!(
        s.sonos_playlists(&Client::Cli, "Den@S1")
            .unwrap()
            .contains(&saved.playlist)
    );
    assert_eq!(
        sim.soap_log()
            .iter()
            .filter(|entry| entry.action == "SaveQueue")
            .count(),
        1
    );
}
