//! Compose native saved playlists through the policy, group and action-log
//! boundary, using the simulator's real sockets on both Sonos generations.

use fsonos_api::surface::queue::QueueTrackInput;
use fsonos_api::surface::sonos_playlists::{
    AddTracksToSonosPlaylistRequest, AppendSonosPlaylistTracksRequest, PlaySonosPlaylistRequest,
    RenameSonosPlaylistRequest, SonosPlaylistContentsDto, SonosPlaylistContentsRequest,
};
use fsonos_api::{ErrorCode, Surface};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control, inventory, playlists, resolve_room};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_sim::{SimHandle, SimHousehold, SimModel, SimPlayerSpec};
use fsonos_types::PlayMode;
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
    transport: Box<dyn Transport + Send + Sync>,
    houses: &[HouseholdState],
    policy: Policy,
) -> Surface {
    let known = houses.to_vec();
    Surface::new(
        transport,
        Box::new(move |_| Ok(known.clone())),
        policy,
        Box::new(SystemClock),
    )
    .with_action_log(Box::new(MemStore::default()), "playlist-composition")
}

fn contents(s: &Surface, zone: &str, playlist: &str) -> SonosPlaylistContentsDto {
    s.sonos_playlist_contents(
        &Client::McpStdio,
        &SonosPlaylistContentsRequest {
            zone: zone.into(),
            playlist: playlist.into(),
            offset: 0,
            limit: 100,
            expected_revision: None,
        },
    )
    .unwrap()
}

fn resources(page: &SonosPlaylistContentsDto) -> Vec<String> {
    page.items
        .iter()
        .map(|item| item.uri.clone().unwrap())
        .collect()
}

fn play(s: &Surface, zone: &str) {
    s.play_sonos_playlist(
        &Client::McpStdio,
        &PlaySonosPlaylistRequest {
            zone: zone.into(),
            playlist: "SQ:7".into(),
        },
    )
    .unwrap();
}

#[test]
#[allow(clippy::too_many_lines)] // Keep successive source/destination revisions in one household.
fn composition_from_a_group_member_preserves_playback_undo_and_the_other_household() {
    for generation in ["S1", "S2"] {
        let sim = sim();
        let initial = households(&sim);
        let zone = format!("Den@{generation}");
        let member_zone = format!("Office@{generation}");
        let other = if generation == "S1" {
            "Den@S2"
        } else {
            "Den@S1"
        };
        let lead = resolve_room(&initial, &zone).unwrap();
        let member = resolve_room(&initial, &member_zone).unwrap();
        soap::join_group(&sim.lan(), member.player.ip, &lead.coordinator.id).unwrap();
        let houses = households(&sim);
        let target = resolve_room(&houses, &member_zone).unwrap();
        let host = target.coordinator.ip;
        let mut policy = Policy::default();
        policy.defaults.max_volume = 25;
        let s = surface(Box::new(sim.lan()), &houses, policy);
        play(&s, &zone);
        let undo = s.preview_undo(&Client::McpStdio).unwrap().unwrap().id;
        soap::pause(&sim.lan(), host).unwrap();
        soap::seek_position(&sim.lan(), host, 17).unwrap();
        set_play_mode(&sim.lan(), host, PlayMode::ShuffleRepeatOne).unwrap();
        soap::set_volume(&sim.lan(), host, 80).unwrap();
        let queue = browse_all(&sim.lan(), host, QUEUE).unwrap();
        let playback = control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap();
        let other_before = contents(&s, other, "SQ:7");
        let other_target = resolve_room(&houses, other).unwrap();
        let other_queue = browse_all(&sim.lan(), other_target.coordinator.ip, QUEUE).unwrap();
        let start = sim.soap_log().len();

        // A queue, another saved playlist, and the destination itself are
        // all concrete native sources; self-copy must freeze its original positions.
        for source_playlist in [None, Some("SQ:1"), Some("SQ:7")] {
            let (source_revision, source_items) = if let Some(playlist) = source_playlist {
                let source = contents(&s, &member_zone, playlist);
                (
                    source.revision,
                    source
                        .items
                        .into_iter()
                        .map(|item| (item.uri.unwrap(), item.title))
                        .collect::<Vec<_>>(),
                )
            } else {
                let source = s.queue(&Client::McpStdio, &member_zone, 0, 100).unwrap();
                (
                    source.revision.unwrap(),
                    source
                        .items
                        .into_iter()
                        .map(|item| (item.uri.unwrap(), item.title))
                        .collect::<Vec<_>>(),
                )
            };
            let before = contents(&s, &member_zone, "SQ:7");
            let positions = vec![2, 1, 2];
            let mut expected = resources(&before);
            let titles: Vec<_> = positions
                .iter()
                .map(|position| {
                    let item = &source_items[usize::try_from(*position - 1).unwrap()];
                    expected.push(item.0.clone());
                    item.1.clone()
                })
                .collect();
            let receipt = s
                .append_sonos_playlist_tracks(
                    &Client::McpStdio,
                    &AppendSonosPlaylistTracksRequest {
                        zone: member_zone.clone(),
                        playlist: before.playlist.id.clone(),
                        expected_revision: before.revision.clone(),
                        source_playlist: source_playlist.map(str::to_owned),
                        source_revision,
                        positions: positions.clone(),
                    },
                )
                .unwrap();
            let after = contents(&s, &member_zone, "SQ:7");
            assert_eq!(
                receipt.zone, "Den",
                "member requests resolve to their coordinator"
            );
            assert_eq!(receipt.playlist, before.playlist);
            assert_eq!(receipt.positions, positions);
            assert_eq!(receipt.titles, titles);
            assert_eq!(
                (receipt.previous_total, receipt.total, receipt.added),
                (before.total, before.total + 3, 3)
            );
            assert_eq!(receipt.revision, after.revision);
            assert_ne!(receipt.revision, before.revision);
            assert!(receipt.logged && receipt.text().contains(&receipt.revision));
            assert_eq!(resources(&after), expected);
            if source_playlist == Some("SQ:1") {
                assert_eq!(
                    resources(&contents(&s, &member_zone, "SQ:1")),
                    source_items
                        .into_iter()
                        .map(|item| item.0)
                        .collect::<Vec<_>>()
                );
            }
        }

        let calls = sim.soap_log();
        let writes: Vec<_> = calls[start..]
            .iter()
            .filter(|entry| entry.action != "Browse" && !entry.action.starts_with("Get"))
            .collect();
        assert_eq!(writes.len(), 9);
        assert!(writes.iter().all(|entry| {
            entry.action == "AddURIToSavedQueue" && entry.player == target.coordinator.id.0
        }));
        assert_eq!(browse_all(&sim.lan(), host, QUEUE).unwrap(), queue);
        assert_eq!(
            control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap(),
            playback
        );
        assert_eq!(
            get_play_mode(&sim.lan(), host).unwrap(),
            PlayMode::ShuffleRepeatOne
        );
        assert_eq!(soap::get_volume(&sim.lan(), host).unwrap(), 80);
        assert_eq!(contents(&s, other, "SQ:7"), other_before);
        assert_eq!(
            browse_all(&sim.lan(), other_target.coordinator.ip, QUEUE).unwrap(),
            other_queue
        );
        assert_eq!(s.preview_undo(&Client::McpStdio).unwrap().unwrap().id, undo);
        let actions = s
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        let appends: Vec<_> = actions
            .iter()
            .filter(|row| {
                row.action
                    .intent
                    .starts_with("append_sonos_playlist_tracks:")
            })
            .collect();
        assert_eq!(appends.len(), 3);
        assert!(appends.iter().all(|row| row.action.client == "mcp-stdio"
            && row.action.decision == "allow"
            && row.action.before_state.is_none()));
    }
}

#[test]
fn stale_queue_saved_source_and_destination_proofs_cannot_append() {
    let sim = sim();
    let houses = households(&sim);
    let s = surface(Box::new(sim.lan()), &houses, Policy::default());
    let zone = "Den@S2";
    let target = resolve_room(&houses, zone).unwrap();
    play(&s, zone);
    let original_queue = s.queue(&Client::McpStdio, zone, 0, 100).unwrap();
    let original_destination = contents(&s, zone, "SQ:1");
    let original_source = contents(&s, zone, "SQ:7");
    let source_track = playlists::select_track(
        &sim.lan(),
        &houses,
        &target.coordinator.id,
        "SQ:7",
        1,
        &original_source.revision,
    )
    .unwrap();
    soap::add_uri_to_queue(
        &sim.lan(),
        target.coordinator.ip,
        source_track.uri.as_deref().unwrap(),
        &source_track.metadata,
        false,
    )
    .unwrap();
    s.rename_sonos_playlist(
        &Client::McpStdio,
        &RenameSonosPlaylistRequest {
            zone: zone.into(),
            playlist: "SQ:1".into(),
            expected_revision: original_destination.revision.clone(),
            title: "Reviewed destination".into(),
        },
    )
    .unwrap();
    let destination = contents(&s, zone, "SQ:1");
    let queue = s.queue(&Client::McpStdio, zone, 0, 100).unwrap();
    let foreign = contents(&s, "Den@S1", "SQ:7");
    let request = AppendSonosPlaylistTracksRequest {
        zone: zone.into(),
        playlist: "SQ:1".into(),
        expected_revision: destination.revision.clone(),
        source_playlist: None,
        source_revision: queue.revision.clone().unwrap(),
        positions: vec![2, 1, 2],
    };
    let start = sim.soap_log().len();
    for invalid in [
        AppendSonosPlaylistTracksRequest {
            source_revision: original_queue.revision.unwrap(),
            ..request.clone()
        },
        AppendSonosPlaylistTracksRequest {
            expected_revision: original_destination.revision,
            ..request.clone()
        },
        AppendSonosPlaylistTracksRequest {
            source_playlist: Some("SQ:7".into()),
            source_revision: original_source.revision,
            ..request.clone()
        },
        AppendSonosPlaylistTracksRequest {
            source_playlist: Some("SQ:7".into()),
            source_revision: foreign.revision,
            ..request.clone()
        },
    ] {
        assert_eq!(
            s.append_sonos_playlist_tracks(&Client::McpStdio, &invalid)
                .unwrap_err()
                .code,
            ErrorCode::QueueChanged,
        );
    }
    assert!(
        sim.soap_log()[start..]
            .iter()
            .all(|entry| entry.action == "Browse" || entry.action.starts_with("Get"))
    );
    assert_eq!(contents(&s, zone, "SQ:1"), destination);
    assert_eq!(s.queue(&Client::McpStdio, zone, 0, 100).unwrap(), queue);
}

struct NoSpeakerIo;

impl Transport for NoSpeakerIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("saved composition must authorize before reading native occurrences")
    }
}

#[test]
fn composition_requires_catalog_permission_for_queue_and_saved_sources_before_io() {
    for rule in [
        "deny = ['append_sonos_playlist_tracks']",
        "deny = ['house-config']",
        "allow = ['read']",
    ] {
        for source_playlist in [None, Some("SQ:1")] {
            let policy = Policy::from_toml(&format!("[clients.\"mcp-stdio\"]\n{rule}\n")).unwrap();
            let s = surface(Box::new(NoSpeakerIo), &[], policy);
            let denied = s
                .append_sonos_playlist_tracks(
                    &Client::McpStdio,
                    &AppendSonosPlaylistTracksRequest {
                        zone: "Den@S2".into(),
                        playlist: "SQ:7".into(),
                        expected_revision: "observed-destination".into(),
                        source_playlist: source_playlist.map(str::to_owned),
                        source_revision: "observed-source".into(),
                        positions: vec![2, 1, 2],
                    },
                )
                .unwrap_err();
            assert_eq!(denied.code, ErrorCode::PolicyDenied);
        }
    }
}

fn spotify_selection() -> Vec<QueueTrackInput> {
    vec![
        QueueTrackInput {
            source_uri: "https://open.spotify.com/track/0000000000000000000001?si=fixture".into(),
            title: Some("  First & last  ".into()),
        },
        QueueTrackInput {
            source_uri: "spotify:episode:0000000000000000000002".into(),
            title: Some("Episode <two>".into()),
        },
        QueueTrackInput {
            source_uri: "spotify:track:0000000000000000000001".into(),
            title: Some("First & last".into()),
        },
    ]
}

#[test]
#[allow(clippy::too_many_lines)] // Keep the preserved playback and native append proof together.
fn explicit_spotify_selections_compose_saved_playlists_without_staging_in_the_live_queue() {
    for generation in ["S1", "S2"] {
        let sim = sim();
        let initial = households(&sim);
        let zone = format!("Den@{generation}");
        let member_zone = format!("Office@{generation}");
        let other = if generation == "S1" {
            "Den@S2"
        } else {
            "Den@S1"
        };
        let lead = resolve_room(&initial, &zone).unwrap();
        let member = resolve_room(&initial, &member_zone).unwrap();
        soap::join_group(&sim.lan(), member.player.ip, &lead.coordinator.id).unwrap();
        let houses = households(&sim);
        let target = resolve_room(&houses, &member_zone).unwrap();
        let host = target.coordinator.ip;
        let mut policy = Policy::default();
        policy.defaults.max_volume = 25;
        let s = surface(Box::new(sim.lan()), &houses, policy);
        play(&s, &zone);
        let undo = s.preview_undo(&Client::McpStdio).unwrap().unwrap().id;
        soap::pause(&sim.lan(), host).unwrap();
        soap::seek_position(&sim.lan(), host, 17).unwrap();
        set_play_mode(&sim.lan(), host, PlayMode::ShuffleRepeatOne).unwrap();
        soap::set_volume(&sim.lan(), host, 80).unwrap();
        let queue = browse_all(&sim.lan(), host, QUEUE).unwrap();
        let playback = control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap();
        let before = contents(&s, &member_zone, "SQ:7");
        let other_before = contents(&s, other, "SQ:7");
        let start = sim.soap_log().len();
        let receipt = s
            .add_tracks_to_sonos_playlist(
                &Client::McpStdio,
                &AddTracksToSonosPlaylistRequest {
                    zone: member_zone.clone(),
                    playlist: before.playlist.id.clone(),
                    expected_revision: before.revision.clone(),
                    tracks: spotify_selection(),
                },
            )
            .unwrap();
        let after = contents(&s, &member_zone, "SQ:7");
        assert_eq!(receipt.zone, "Den");
        assert_eq!(receipt.playlist, before.playlist);
        assert_eq!(
            (receipt.previous_total, receipt.total, receipt.added),
            (before.total, before.total + 3, 3)
        );
        assert_eq!(receipt.revision, after.revision);
        assert_ne!(receipt.revision, before.revision);
        assert_eq!(
            receipt.source_uris,
            [
                "spotify:track:0000000000000000000001",
                "spotify:episode:0000000000000000000002",
                "spotify:track:0000000000000000000001",
            ]
        );
        assert_eq!(
            receipt.titles,
            ["First & last", "Episode <two>", "First & last"]
        );
        assert!(receipt.logged && receipt.text().contains(&receipt.revision));
        assert!(after.items.starts_with(&before.items));
        let appended = &after.items[before.items.len()..];
        let identities: Vec<_> = appended
            .iter()
            .map(|item| {
                item.uri
                    .as_deref()
                    .unwrap()
                    .strip_prefix("x-sonos-spotify:")
                    .unwrap()
                    .split('?')
                    .next()
                    .unwrap()
                    .replace("%3a", ":")
                    .replace("%3A", ":")
            })
            .collect();
        assert_eq!(identities, receipt.source_uris);
        assert_eq!(
            appended
                .iter()
                .map(|item| item.title.as_str())
                .collect::<Vec<_>>(),
            receipt.titles
        );
        let calls = sim.soap_log();
        let writes: Vec<_> = calls[start..]
            .iter()
            .filter(|entry| entry.action != "Browse" && !entry.action.starts_with("Get"))
            .collect();
        assert_eq!(writes.len(), 3);
        assert!(writes.iter().all(|entry| {
            entry.action == "AddURIToSavedQueue" && entry.player == target.coordinator.id.0
        }));
        assert_eq!(browse_all(&sim.lan(), host, QUEUE).unwrap(), queue);
        assert_eq!(
            control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap(),
            playback
        );
        assert_eq!(
            get_play_mode(&sim.lan(), host).unwrap(),
            PlayMode::ShuffleRepeatOne
        );
        assert_eq!(soap::get_volume(&sim.lan(), host).unwrap(), 80);
        assert_eq!(contents(&s, other, "SQ:7"), other_before);
        assert_eq!(s.preview_undo(&Client::McpStdio).unwrap().unwrap().id, undo);
        let actions = s
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        let action = actions
            .iter()
            .find(|row| {
                row.action
                    .intent
                    .starts_with("add_tracks_to_sonos_playlist:")
            })
            .unwrap();
        assert_eq!(action.action.client, "mcp-stdio");
        assert_eq!(action.action.decision, "allow");
        assert!(action.action.before_state.is_none());
    }
}

#[test]
fn explicit_selection_requires_current_destination_and_house_config_permission() {
    let sim = sim();
    let houses = households(&sim);
    let s = surface(Box::new(sim.lan()), &houses, Policy::default());
    let before = contents(&s, "Den@S2", "SQ:7");
    let foreign = contents(&s, "Den@S1", "SQ:7");
    let request = AddTracksToSonosPlaylistRequest {
        zone: "Den@S2".into(),
        playlist: before.playlist.id,
        expected_revision: before.revision,
        tracks: spotify_selection(),
    };
    s.add_tracks_to_sonos_playlist(&Client::McpStdio, &request)
        .unwrap();
    let current = contents(&s, "Den@S2", "SQ:7");
    let start = sim.soap_log().len();
    for expected_revision in [request.expected_revision.clone(), foreign.revision] {
        let failure = s
            .add_tracks_to_sonos_playlist(
                &Client::McpStdio,
                &AddTracksToSonosPlaylistRequest {
                    expected_revision,
                    ..request.clone()
                },
            )
            .unwrap_err();
        assert_eq!(failure.code, ErrorCode::QueueChanged);
    }
    assert!(
        sim.soap_log()[start..]
            .iter()
            .all(|call| call.action == "Browse" || call.action.starts_with("Get"))
    );
    assert_eq!(contents(&s, "Den@S2", "SQ:7"), current);
    let catalog_only = Policy::from_toml(
        "[clients.\"mcp-stdio\"]\nallow = ['house-config', 'read']\ndeny = ['play', 'add_to_queue', 'set_volume']\n",
    )
    .unwrap();
    let composer = surface(Box::new(sim.lan()), &houses, catalog_only);
    let added = composer
        .add_tracks_to_sonos_playlist(
            &Client::McpStdio,
            &AddTracksToSonosPlaylistRequest {
                expected_revision: current.revision,
                ..request.clone()
            },
        )
        .unwrap();
    assert_eq!(added.added, 3);
    for rule in [
        "deny = ['add_tracks_to_sonos_playlist']",
        "deny = ['house-config']",
        "allow = ['read']",
    ] {
        let policy = Policy::from_toml(&format!("[clients.\"mcp-stdio\"]\n{rule}\n")).unwrap();
        let denied = surface(Box::new(NoSpeakerIo), &[], policy)
            .add_tracks_to_sonos_playlist(&Client::McpStdio, &request)
            .unwrap_err();
        assert_eq!(denied.code, ErrorCode::PolicyDenied);
    }
}
