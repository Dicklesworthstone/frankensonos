//! Frozen native occurrences flow through authored scenes, policy, undo and routines.

use super::*;
use fsonos_api::surface::sonos_playlists::{
    PlaySonosPlaylistRequest, SonosPlaylistContentsRequest,
};
use fsonos_core::store::ActionFilter;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_types::PlayMode;

fn selected(surface: &Surface, source: &str, target: &str, playing: bool) -> SceneDefinition {
    let page = surface
        .sonos_playlist_contents(
            &Client::Cli,
            &SonosPlaylistContentsRequest {
                zone: source.into(),
                playlist: "SQ:7".into(),
                offset: 0,
                limit: 10,
                expected_revision: None,
            },
        )
        .unwrap();
    serde_json::from_value(json!({
        "groups": [{
            "coordinator": target,
            "source": {
                "kind": "sonos_playlist_tracks", "name": "SQ:7",
                "selection": {
                    "zone": source, "positions": [2, 1, 2],
                    "expected_revision": page.revision
                }
            },
            "playing": playing
        }]
    }))
    .unwrap()
}

fn queue(sim: &SimHandle, room: &str) -> Vec<String> {
    browse_snapshot(&sim.lan(), sim.player(room).unwrap().ip, QUEUE)
        .unwrap()
        .objects
        .into_iter()
        .map(|item| item.res.unwrap().uri)
        .collect()
}

#[test]
fn saved_native_selections_retain_exact_order_after_catalog_loss_without_favorites() {
    for playing in [false, true] {
        let (sim, surface, fault) = fixture(Policy::default(), MemStore::default());
        sim.retain_favorites(1, |_| false).unwrap();
        sim.retain_favorites(2, |_| false).unwrap();
        for (room, generation) in [("Kitchen", 1), ("Den", 2)] {
            seed(&sim, room);
            let definition = selected(&surface, room, room, playing);
            let before = sim.soap_log().len();
            let saved = surface
                .save_scene_from(&Client::Cli, room, &definition)
                .unwrap();
            assert_reads(&sim, before);
            assert_eq!(surface.scene(&Client::Cli, room).unwrap(), saved);
            let source = &saved.groups[0].source;
            assert_eq!(source.kind, "sonos_playlist_tracks");
            assert!(
                source.selection.is_none(),
                "admission evidence is not a renderer recipe"
            );
            assert_eq!(
                source.playlist.as_ref().unwrap().household,
                format!("SIM_HOUSEHOLD_S{generation}")
            );
            let expected: Vec<_> = source
                .tracks
                .as_ref()
                .unwrap()
                .iter()
                .map(|track| track.source_uri.clone())
                .collect();
            assert_eq!(expected.len(), 3);
            assert_eq!(expected[0], expected[2]);
            assert_ne!(expected[0], expected[1]);
            let public = serde_json::to_string(&saved).unwrap();
            assert!(!public.contains("DIDL-Lite"));
            assert!(!public.contains("metadata"));
            fault.store(3, Ordering::SeqCst);
            let before = sim.soap_log().len();
            let applied = surface.apply_scene(&Client::Cli, room).unwrap();
            assert!(applied.complete, "{applied:?}");
            assert_eq!(queue(&sim, room), expected);
            let host = sim.player(room).unwrap().ip;
            assert_eq!(get_play_mode(&sim.lan(), host).unwrap(), PlayMode::Normal);
            assert_eq!(soap::get_position_info(&sim.lan(), host).unwrap().track, 1);
            assert_eq!(
                soap::get_transport_info(&sim.lan(), host).unwrap().state
                    == TransportState::Playing,
                playing
            );
            let writes = &sim.soap_log()[before..];
            assert_eq!(
                writes
                    .iter()
                    .filter(|call| call.action == "AddURIToQueue")
                    .count(),
                3
            );
            assert_eq!(
                writes.iter().filter(|call| call.action == "Play").count(),
                usize::from(playing)
            );
            assert!(
                !writes
                    .iter()
                    .any(|call| call.action == "RemoveAllTracksFromQueue")
            );
            fault.store(0, Ordering::SeqCst);
        }
    }
}

#[test]
fn native_selection_saving_refuses_stale_source_or_foreign_destination_before_writes() {
    let (sim, surface, _) = fixture(Policy::default(), MemStore::default());
    for cross_household in [false, true] {
        let mut definition = selected(
            &surface,
            "Kitchen",
            if cross_household { "Den" } else { "Kitchen" },
            false,
        );
        if !cross_household {
            definition.groups[0]
                .source
                .as_mut()
                .unwrap()
                .selection
                .as_mut()
                .unwrap()
                .expected_revision = "obsolete-selection".into();
        }
        let before = sim.soap_log().len();
        assert!(
            surface
                .save_scene_from(&Client::Cli, "Refused", &definition)
                .is_err()
        );
        assert_reads(&sim, before);
        assert!(surface.scene(&Client::Cli, "Refused").is_err());
    }
}

#[test]
fn native_selection_from_another_room_follows_confirmed_regrouping_and_saved_author_caps() {
    let (sim, surface, _) = fixture(
        Policy::from_toml("[defaults]\nmax_volume = 24\n").unwrap(),
        MemStore::default(),
    );
    let kitchen = sim.player("Kitchen").unwrap();
    let office = sim.player("Office").unwrap();
    soap::join_group(&sim.lan(), office.ip, &PlayerId(kitchen.uuid.clone())).unwrap();
    soap::set_volume(&sim.lan(), office.ip, 80).unwrap();
    soap::set_volume(&sim.lan(), kitchen.ip, 70).unwrap();
    let mut definition = selected(&surface, "Kitchen", "Office", true);
    definition.groups[0].members.push("Kitchen".into());
    let saved = surface
        .save_scene_from(
            &Client::Tailnet("tag:restricted".into()),
            "Promote Office",
            &definition,
        )
        .unwrap();
    let expected: Vec<_> = saved.groups[0]
        .source
        .tracks
        .as_ref()
        .unwrap()
        .iter()
        .map(|track| track.source_uri.clone())
        .collect();
    let applied = surface.apply_scene(&Client::Cli, "Promote Office").unwrap();
    assert!(applied.complete, "{applied:?}");
    assert_eq!(queue(&sim, "Office"), expected);
    assert!(soap::get_volume(&sim.lan(), office.ip).unwrap() <= 24);
    assert!(soap::get_volume(&sim.lan(), kitchen.ip).unwrap() <= 24);
    assert!(
        get_zone_group_state(&sim.lan(), office.ip)
            .unwrap()
            .groups
            .iter()
            .any(|group| group.coordinator.0 == office.uuid && group.members.len() == 2)
    );
}

#[test]
fn complete_native_selection_scene_keeps_bounded_queue_undo() {
    for room in ["Kitchen", "Den"] {
        for playing in [false, true] {
            let (sim, surface, _) = fixture(Policy::default(), MemStore::default());
            surface
                .play_sonos_playlist(
                    &Client::Cli,
                    &PlaySonosPlaylistRequest {
                        zone: room.into(),
                        playlist: "SQ:7".into(),
                    },
                )
                .unwrap();
            let host = sim.player(room).unwrap().ip;
            soap::pause(&sim.lan(), host).unwrap();
            soap::seek_track(&sim.lan(), host, 2).unwrap();
            soap::seek_position(&sim.lan(), host, 17).unwrap();
            set_play_mode(&sim.lan(), host, PlayMode::ShuffleRepeatOne).unwrap();
            let original = queue(&sim, room);
            let definition = selected(&surface, room, room, playing);
            surface
                .save_scene_from(&Client::Cli, "Exact selection", &definition)
                .unwrap();
            let applied = surface
                .apply_scene(&Client::Cli, "Exact selection")
                .unwrap();
            assert!(applied.complete, "{applied:?}");
            assert_ne!(queue(&sim, room), original);
            let preview = surface.preview_undo(&Client::Cli).unwrap().unwrap();
            assert!(preview.undoable, "{preview:?}");
            let report = surface.undo_checked(&Client::Cli, preview.id).unwrap();
            assert!(report.failures.is_empty(), "{report:?}");
            assert_eq!(queue(&sim, room), original);
            let position = soap::get_position_info(&sim.lan(), host).unwrap();
            assert_eq!((position.track, position.position_secs), (2, Some(17)));
            assert_eq!(
                get_play_mode(&sim.lan(), host).unwrap(),
                PlayMode::ShuffleRepeatOne
            );
            assert_ne!(
                soap::get_transport_info(&sim.lan(), host).unwrap().state,
                TransportState::Playing
            );
        }
    }
}

#[test]
fn scheduled_native_selection_uses_saved_contents_when_the_original_catalog_is_unreadable() {
    let (sim, surface, fault) = fixture(Policy::default(), MemStore::default());
    let definition = selected(&surface, "Den", "Den", false);
    let saved = surface
        .save_scene_from(&Client::Cli, "Quiet selected tracks", &definition)
        .unwrap();
    let expected: Vec<_> = saved.groups[0]
        .source
        .tracks
        .as_ref()
        .unwrap()
        .iter()
        .map(|track| track.source_uri.clone())
        .collect();
    let schedule = surface
        .add_schedule(
            &Client::Cli,
            &ScheduleRequest {
                when: "in 1m".into(),
                action: "apply_scene".into(),
                scene: Some("Quiet selected tracks".into()),
                ..ScheduleRequest::default()
            },
        )
        .unwrap();
    let at = DateTime::parse_from_rfc3339(schedule.next.as_deref().unwrap())
        .unwrap()
        .to_utc();
    let scheduler = Scheduler::new(&surface);
    fault.store(3, Ordering::SeqCst);
    let before = sim.soap_log().len();
    assert_eq!(
        scheduler.tick_at(at, &Utc),
        [Ran::Fired {
            id: schedule.id,
            at
        }]
    );
    assert!(scheduler.wait(Duration::from_secs(10)));
    assert!(
        surface.schedules(&Client::Cli).unwrap()[0]
            .last_fired
            .is_some()
    );
    assert!(
        scheduler
            .tick_at(at + chrono::TimeDelta::minutes(1), &Utc)
            .is_empty()
    );
    assert_eq!(queue(&sim, "Den"), expected);
    assert!(
        !sim.soap_log()[before..]
            .iter()
            .any(|call| call.action == "Play")
    );
    let actions = surface
        .recent_actions(&Client::Cli, &ActionFilter::default())
        .unwrap();
    assert_eq!(
        actions
            .iter()
            .filter(|row| row
                .action
                .intent
                .contains("apply_scene: Quiet selected tracks"))
            .count(),
        1
    );
}
