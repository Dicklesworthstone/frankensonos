//! Native catalog management through policy, fresh group resolution, action
//! logging and the simulator's real SOAP sockets, without playback effects.

use fsonos_api::surface::sonos_playlists::{
    MoveSonosPlaylistTracksRequest, PlaySonosPlaylistRequest, RemoveSonosPlaylistRequest,
    RemoveSonosPlaylistTracksRequest, RenameSonosPlaylistRequest, SonosPlaylistContentsRequest,
    SonosPlaylistEditOperation, SonosPlaylistOperation,
};
use fsonos_api::{ErrorCode, Surface};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{ActionFilter, MemStore};
use fsonos_core::{HouseholdState, control, inventory, resolve_room};
use fsonos_proto::content::{QUEUE, browse_all};
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_sim::{SimHandle, SimHousehold, SimLan, SimModel, SimPlayerSpec};
use std::net::IpAddr;
use std::time::Duration;

fn sim() -> SimHandle {
    SimHousehold::builder()
        .s1([SimPlayerSpec::new("Den", SimModel::Play5Gen1)])
        .s2([SimPlayerSpec::new("Den", SimModel::One)])
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
    .with_action_log(Box::new(MemStore::default()), "playlist-management")
}

fn observed(
    s: &Surface,
    zone: &str,
) -> fsonos_api::surface::sonos_playlists::SonosPlaylistContentsDto {
    s.sonos_playlist_contents(
        &Client::McpStdio,
        &SonosPlaylistContentsRequest {
            zone: zone.into(),
            playlist: "SQ:7".into(),
            offset: 0,
            limit: 1,
            expected_revision: None,
        },
    )
    .unwrap()
}

fn rename(zone: &str, revision: &str, title: &str) -> RenameSonosPlaylistRequest {
    RenameSonosPlaylistRequest {
        zone: zone.into(),
        playlist: "SQ:7".into(),
        expected_revision: revision.into(),
        title: title.into(),
    }
}

fn remove(zone: &str, revision: &str) -> RemoveSonosPlaylistRequest {
    RemoveSonosPlaylistRequest {
        zone: zone.into(),
        playlist: "SQ:7".into(),
        expected_revision: revision.into(),
    }
}

fn full_contents(
    s: &Surface,
    zone: &str,
) -> fsonos_api::surface::sonos_playlists::SonosPlaylistContentsDto {
    s.sonos_playlist_contents(
        &Client::McpStdio,
        &SonosPlaylistContentsRequest {
            zone: zone.into(),
            playlist: "SQ:7".into(),
            offset: 0,
            limit: 100,
            expected_revision: None,
        },
    )
    .unwrap()
}

#[test]
#[allow(clippy::too_many_lines)] // Keep sequential revisions and preserved playback in one fixture.
fn saved_occurrence_edits_preserve_loaded_queues_and_other_households_on_s1_and_s2() {
    for generation in ["S1", "S2"] {
        let sim = sim();
        let houses = households(&sim);
        let zone = format!("Den@{generation}");
        let other = if generation == "S1" {
            "Den@S2"
        } else {
            "Den@S1"
        };
        let s = surface(Box::new(sim.lan()), &houses, Policy::default());
        s.play_sonos_playlist(
            &Client::McpStdio,
            &PlaySonosPlaylistRequest {
                zone: zone.clone(),
                playlist: "SQ:7".into(),
            },
        )
        .unwrap();
        let undo = s.preview_undo(&Client::McpStdio).unwrap().unwrap().id;
        let target = resolve_room(&houses, &zone).unwrap();
        let playing = control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap();
        let queue = browse_all(&sim.lan(), target.coordinator.ip, QUEUE).unwrap();
        let before = full_contents(&s, &zone);
        let other_before = full_contents(&s, other);
        assert!(before.items.len() >= 3);
        let start = sim.soap_log().len();
        let moved = s
            .move_sonos_playlist_tracks(
                &Client::McpStdio,
                &MoveSonosPlaylistTracksRequest {
                    zone: zone.clone(),
                    playlist: "SQ:7".into(),
                    expected_revision: before.revision.clone(),
                    position: 1,
                    count: 2,
                    insert_before: before.total + 1,
                },
            )
            .unwrap();
        assert!(moved.changed && moved.logged);
        assert_eq!(moved.operation, SonosPlaylistEditOperation::MoveTracks);
        assert_eq!(
            (moved.previous_total, moved.total),
            (before.total, before.total)
        );
        assert_eq!((moved.position, moved.count), (1, 2));
        let after = full_contents(&s, &zone);
        assert_eq!(moved.revision, after.revision);
        let identity = |page: &fsonos_api::surface::sonos_playlists::SonosPlaylistContentsDto| {
            page.items
                .iter()
                .map(|item| (item.title.clone(), item.uri.clone()))
                .collect::<Vec<_>>()
        };
        let mut wanted = identity(&before);
        wanted.rotate_left(2);
        assert_eq!(identity(&after), wanted);
        let stale = RemoveSonosPlaylistTracksRequest {
            zone: zone.clone(),
            playlist: "SQ:7".into(),
            expected_revision: before.revision,
            position: 1,
            count: 1,
        };
        assert_eq!(
            s.remove_sonos_playlist_tracks(&Client::McpStdio, &stale)
                .unwrap_err()
                .code,
            ErrorCode::QueueChanged
        );
        let noop = s
            .move_sonos_playlist_tracks(
                &Client::McpStdio,
                &MoveSonosPlaylistTracksRequest {
                    zone: zone.clone(),
                    playlist: "SQ:7".into(),
                    expected_revision: after.revision.clone(),
                    position: 1,
                    count: 2,
                    insert_before: 3,
                },
            )
            .unwrap();
        assert!(!noop.changed);
        assert_eq!(noop.revision, after.revision);
        let removed = s
            .remove_sonos_playlist_tracks(
                &Client::McpStdio,
                &RemoveSonosPlaylistTracksRequest {
                    expected_revision: after.revision,
                    ..stale
                },
            )
            .unwrap();
        assert!(removed.changed && removed.logged);
        assert_eq!(removed.operation, SonosPlaylistEditOperation::RemoveTracks);
        assert_eq!(removed.total, before.total - 1);
        let after_remove = full_contents(&s, &zone);
        assert_eq!(removed.revision, after_remove.revision);
        assert_eq!(identity(&after_remove), wanted[1..]);
        assert_eq!(full_contents(&s, other), other_before);
        assert_eq!(
            control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap(),
            playing
        );
        assert_eq!(
            browse_all(&sim.lan(), target.coordinator.ip, QUEUE).unwrap(),
            queue
        );
        assert_eq!(s.preview_undo(&Client::McpStdio).unwrap().unwrap().id, undo);
        let writes: Vec<_> = sim.soap_log()[start..]
            .iter()
            .filter(|entry| {
                !matches!(entry.action.as_str(), "Browse") && !entry.action.starts_with("Get")
            })
            .cloned()
            .collect();
        assert_eq!(writes.len(), 2);
        assert!(
            writes
                .iter()
                .all(|call| call.action == "ReorderTracksInSavedQueue"
                    && call.player == target.coordinator.id.0)
        );
        let actions = s
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        let edits: Vec<_> = actions
            .iter()
            .filter(|row| {
                row.action.intent.starts_with("move_sonos_playlist_tracks:")
                    || row
                        .action
                        .intent
                        .starts_with("remove_sonos_playlist_tracks:")
            })
            .collect();
        assert_eq!(edits.len(), 4);
        assert!(edits.iter().all(|row| row.action.before_state.is_none()));
    }
}

#[test]
fn saved_occurrence_edits_enforce_catalog_permissions_before_any_speaker_io() {
    for tool in ["remove_sonos_playlist_tracks", "move_sonos_playlist_tracks"] {
        for rule in [
            format!("deny = ['{tool}']"),
            "deny = ['house-config']".into(),
            "allow = ['read']".into(),
        ] {
            let policy = Policy::from_toml(&format!("[clients.\"mcp-stdio\"]\n{rule}\n")).unwrap();
            let s = surface(Box::new(NoSpeakerIo), &[], policy);
            let failure = if tool == "remove_sonos_playlist_tracks" {
                s.remove_sonos_playlist_tracks(
                    &Client::McpStdio,
                    &RemoveSonosPlaylistTracksRequest {
                        zone: "Den".into(),
                        playlist: "SQ:7".into(),
                        expected_revision: "r".into(),
                        position: 2,
                        count: 1,
                    },
                )
            } else {
                s.move_sonos_playlist_tracks(
                    &Client::McpStdio,
                    &MoveSonosPlaylistTracksRequest {
                        zone: "Den".into(),
                        playlist: "SQ:7".into(),
                        expected_revision: "r".into(),
                        position: 2,
                        count: 1,
                        insert_before: 1,
                    },
                )
            }
            .unwrap_err();
            assert_eq!(failure.code, ErrorCode::PolicyDenied);
        }
    }
    let s = surface(Box::new(NoSpeakerIo), &[], Policy::default());
    for (position, count) in [(0, 1), (1, 0), (u32::MAX, 2), (1, 1001)] {
        assert_eq!(
            s.remove_sonos_playlist_tracks(
                &Client::Cli,
                &RemoveSonosPlaylistTracksRequest {
                    zone: "Den".into(),
                    playlist: "SQ:7".into(),
                    expected_revision: "r".into(),
                    position,
                    count,
                }
            )
            .unwrap_err()
            .code,
            ErrorCode::InvalidArgument
        );
    }
}

#[test]
fn native_edits_preserve_playing_queues_caps_undo_and_the_other_household() {
    for generation in ["S1", "S2"] {
        let sim = sim();
        let houses = households(&sim);
        let zone = format!("Den@{generation}");
        let other = if generation == "S1" {
            "Den@S2"
        } else {
            "Den@S1"
        };
        let s = surface(Box::new(sim.lan()), &houses, Policy::default());
        s.play_sonos_playlist(
            &Client::McpStdio,
            &PlaySonosPlaylistRequest {
                zone: zone.clone(),
                playlist: "SQ:7".into(),
            },
        )
        .unwrap();
        let undo = s.preview_undo(&Client::McpStdio).unwrap().unwrap().id;
        let target = resolve_room(&houses, &zone).unwrap();
        soap::set_volume(&sim.lan(), target.coordinator.ip, 85).unwrap();
        let playback = control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap();
        let queue = browse_all(&sim.lan(), target.coordinator.ip, QUEUE).unwrap();
        let other_catalog = s.sonos_playlists(&Client::Cli, other).unwrap();
        let before = observed(&s, &zone);
        let start = sim.soap_log().len();
        let renamed = s
            .rename_sonos_playlist(
                &Client::McpStdio,
                &rename(&zone, &before.revision, "  Dvořák & friends, <renamed>  "),
            )
            .unwrap();
        assert!(renamed.changed && renamed.logged);
        assert_eq!(renamed.operation, SonosPlaylistOperation::Rename);
        assert_eq!(renamed.playlist.id, "SQ:7");
        assert_eq!(renamed.playlist.title, "Dvořák & friends, <renamed>");
        assert_eq!(renamed.zone, "Den");
        let after = observed(&s, &zone);
        let unchanged = s
            .rename_sonos_playlist(
                &Client::McpStdio,
                &rename(&zone, &after.revision, &renamed.playlist.title),
            )
            .unwrap();
        assert!(!unchanged.changed && unchanged.logged);
        let removed = s
            .remove_sonos_playlist(&Client::McpStdio, &remove(&zone, &after.revision))
            .unwrap();
        assert_eq!(removed.operation, SonosPlaylistOperation::Remove);
        assert!(removed.changed && removed.logged);
        assert_eq!(removed.playlist, renamed.playlist);
        let writes: Vec<_> = sim.soap_log()[start..]
            .iter()
            .filter(|entry| !matches!(entry.action.as_str(), "GetZoneGroupState" | "Browse"))
            .cloned()
            .collect();
        assert_eq!(
            writes
                .iter()
                .map(|call| call.action.as_str())
                .collect::<Vec<_>>(),
            ["UpdateObject", "DestroyObject"]
        );
        assert!(
            writes
                .iter()
                .all(|call| call.player == target.coordinator.id.0)
        );
        assert_eq!(
            control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap(),
            playback
        );
        assert_eq!(
            browse_all(&sim.lan(), target.coordinator.ip, QUEUE).unwrap(),
            queue
        );
        assert_eq!(
            soap::get_volume(&sim.lan(), target.coordinator.ip).unwrap(),
            85
        );
        assert_eq!(
            s.sonos_playlists(&Client::Cli, other).unwrap(),
            other_catalog
        );
        assert!(
            !s.sonos_playlists(&Client::Cli, &zone)
                .unwrap()
                .iter()
                .any(|item| item.id == "SQ:7")
        );
        assert_eq!(s.preview_undo(&Client::McpStdio).unwrap().unwrap().id, undo);
        let actions = s
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        let edits: Vec<_> = actions
            .iter()
            .filter(|row| {
                row.action.intent.starts_with("rename_sonos_playlist:")
                    || row.action.intent.starts_with("remove_sonos_playlist:")
            })
            .collect();
        assert_eq!(edits.len(), 3);
        assert!(edits.iter().all(|row| row.action.client == "mcp-stdio"
            && row.action.decision == "allow"
            && row.action.before_state.is_none()));
    }
}

struct NoSpeakerIo;
impl Transport for NoSpeakerIo {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("authorization and request validation must precede all speaker I/O")
    }
}

#[test]
fn exact_identity_proof_and_house_config_permission_precede_native_io() {
    for tool in ["rename_sonos_playlist", "remove_sonos_playlist"] {
        for rule in [
            format!("deny = ['{tool}']"),
            "deny = ['house-config']".into(),
            "allow = ['read']".into(),
        ] {
            let policy = Policy::from_toml(&format!("[clients.\"mcp-stdio\"]\n{rule}\n")).unwrap();
            let s = surface(Box::new(NoSpeakerIo), &[], policy);
            let failure = if tool == "rename_sonos_playlist" {
                s.rename_sonos_playlist(&Client::McpStdio, &rename("Den", "revision", "New"))
            } else {
                s.remove_sonos_playlist(&Client::McpStdio, &remove("Den", "revision"))
            }
            .unwrap_err();
            assert_eq!(failure.code, ErrorCode::PolicyDenied);
            let log = s
                .recent_actions(&Client::Cli, &ActionFilter::default())
                .unwrap();
            assert_eq!(log.len(), 1);
            assert!(log[0].action.decision.starts_with("deny:"));
        }
    }
    let s = surface(Box::new(NoSpeakerIo), &[], Policy::default());
    for request in [
        rename("Den", "", "New"),
        rename("Den", "revision", "a\nb"),
        rename(" ", "revision", "New"),
    ] {
        assert_eq!(
            s.rename_sonos_playlist(&Client::Cli, &request)
                .unwrap_err()
                .code,
            ErrorCode::InvalidArgument
        );
    }
    let mut wrong_id = remove("Den", "revision");
    wrong_id.playlist = "Saved title".into();
    assert_eq!(
        s.remove_sonos_playlist(&Client::Cli, &wrong_id)
            .unwrap_err()
            .code,
        ErrorCode::InvalidArgument
    );
}

struct LoseReply(SimLan);
impl Transport for LoseReply {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        let result = self.0.soap_post(host, path, action, body)?;
        if action.ends_with("#UpdateObject\"")
            || action.ends_with("#DestroyObject\"")
            || action.ends_with("#ReorderTracksInSavedQueue\"")
        {
            return Err(ProtoError::Network {
                target: host.to_string(),
                detail: "reply lost after catalog persistence".into(),
            });
        }
        Ok(result)
    }
}

#[test]
fn uncertain_saved_occurrence_write_keeps_the_confirmed_old_queue_and_is_never_replayed() {
    for moving in [false, true] {
        let sim = sim();
        let houses = households(&sim);
        let s = surface(Box::new(LoseReply(sim.lan())), &houses, Policy::default());
        let before = full_contents(&s, "Den@S2");
        let start = sim.soap_log().len();
        let result = if moving {
            s.move_sonos_playlist_tracks(
                &Client::McpStdio,
                &MoveSonosPlaylistTracksRequest {
                    zone: "Den@S2".into(),
                    playlist: "SQ:7".into(),
                    expected_revision: before.revision,
                    position: 1,
                    count: 1,
                    insert_before: before.total + 1,
                },
            )
        } else {
            s.remove_sonos_playlist_tracks(
                &Client::McpStdio,
                &RemoveSonosPlaylistTracksRequest {
                    zone: "Den@S2".into(),
                    playlist: "SQ:7".into(),
                    expected_revision: before.revision,
                    position: 1,
                    count: 1,
                },
            )
        };
        let failure = result.unwrap_err();
        assert_eq!(failure.code, ErrorCode::OutcomeUncertain);
        assert!(!failure.retryable());
        assert!(failure.hint.contains("original household"));
        let calls = sim.soap_log();
        let writes: Vec<_> = calls[start..]
            .iter()
            .filter(|call| !call.action.starts_with("Get") && call.action != "Browse")
            .collect();
        assert_eq!(writes.len(), 1);
        assert_eq!(writes[0].action, "ReorderTracksInSavedQueue");
        let actions = s
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        assert_eq!(actions.len(), 1);
        assert!(actions[0].action.before_state.is_none());
        assert!(actions[0].action.result.starts_with("failed:"));
    }
}

#[test]
fn an_uncertain_catalog_write_is_logged_once_without_playback_recovery_or_undo() {
    for remove_playlist in [false, true] {
        let sim = sim();
        let houses = households(&sim);
        let reader = surface(Box::new(sim.lan()), &houses, Policy::default());
        let before = observed(&reader, "Den@S2");
        let s = surface(Box::new(LoseReply(sim.lan())), &houses, Policy::default());
        let start = sim.soap_log().len();
        let failure = if remove_playlist {
            s.remove_sonos_playlist(&Client::McpStdio, &remove("Den@S2", &before.revision))
        } else {
            s.rename_sonos_playlist(
                &Client::McpStdio,
                &rename("Den@S2", &before.revision, "Changed once"),
            )
        }
        .unwrap_err();
        assert_eq!(failure.code, ErrorCode::OutcomeUncertain);
        assert!(failure.hint.contains("original household"));
        assert!(failure.hint.contains("not automatically retried"));
        let writes: Vec<_> = sim.soap_log()[start..]
            .iter()
            .filter(|entry| !matches!(entry.action.as_str(), "GetZoneGroupState" | "Browse"))
            .cloned()
            .collect();
        assert_eq!(writes.len(), 1);
        assert_eq!(
            writes[0].action,
            if remove_playlist {
                "DestroyObject"
            } else {
                "UpdateObject"
            }
        );
        let catalog = reader.sonos_playlists(&Client::Cli, "Den@S2").unwrap();
        if remove_playlist {
            assert!(!catalog.iter().any(|item| item.id == "SQ:7"));
        } else {
            assert_eq!(
                catalog.iter().find(|item| item.id == "SQ:7").unwrap().title,
                "Changed once"
            );
        }
        let actions = s
            .recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap();
        assert_eq!(actions.len(), 1);
        assert!(actions[0].action.result.starts_with("failed:"));
        assert!(actions[0].action.before_state.is_none());
        assert!(s.preview_undo(&Client::McpStdio).unwrap().is_none());
    }
}

#[test]
fn confirmed_native_catalog_edits_do_not_require_a_local_action_log() {
    let sim = sim();
    let houses = households(&sim);
    let known = houses.clone();
    let s = Surface::new(
        Box::new(sim.lan()),
        Box::new(move |_| Ok(known.clone())),
        Policy::default(),
        Box::new(SystemClock),
    );
    let before = observed(&s, "Den@S1");
    let renamed = s
        .rename_sonos_playlist(
            &Client::Cli,
            &rename("Den@S1", &before.revision, "Saved on Sonos"),
        )
        .unwrap();
    assert!(renamed.changed);
    assert!(!renamed.logged);
    assert!(renamed.text().contains("catalog result was confirmed"));
    assert!(renamed.text().contains("do not repeat"));
    assert!(
        s.sonos_playlists(&Client::Cli, "Den@S1")
            .unwrap()
            .contains(&renamed.playlist)
    );
}
