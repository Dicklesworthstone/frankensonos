//! Scene replacements share the actual action log, policy, reviewed Undo and
//! native simulator transport used by direct source playback.

use super::*;
use fsonos_api::source::{SourceSelection, SourceSelectionKind, SpotifySources};
use fsonos_api::surface::scenes::{SceneApplyRequest, SceneDefinition};
use serde_json::json;

fn definition(f: &Fixture, source: u8, playing: bool) -> SceneDefinition {
    let source = match source {
        0 | 1 => json!({"kind": "uri", "uri": Fixture::request("Alpha", source == 0).source_uri}),
        2 => {
            let favorite = favorites::list(&f.sim.lan(), &f.houses, &f.coordinator)
                .unwrap()
                .into_iter()
                .find(|favorite| favorite.kind == FavoriteKind::Container)
                .unwrap();
            json!({"kind": "favorite", "name": favorite.id})
        }
        3 => json!({"kind": "sonos_playlist", "name": "SQ:1"}),
        4 => json!({"kind": "uri", "uri": "spotify:artist:2000000000000000000001"}),
        _ => json!({"kind": "uri", "uri": "spotify:show:3000000000000000000001"}),
    };
    serde_json::from_value(json!({
        "groups": [{"coordinator": "Alpha", "members": ["Beta"], "source": source, "playing": playing}],
        "volumes": {"Alpha": 12, "Beta": 18},
        "mutes": {"Alpha": false, "Beta": true}
    }))
    .unwrap()
}

struct Catalog;

impl Catalog {
    fn selection(episodes: bool) -> SourceSelection {
        let kind = if episodes { "episode" } else { "track" };
        SourceSelection {
            tracks: [1, 2, 3]
                .into_iter()
                .map(|number| {
                    (
                        format!("spotify:{kind}:{number:022}"),
                        format!("Selection {number}"),
                    )
                })
                .collect(),
            kind: if episodes {
                SourceSelectionKind::ShowEpisodes
            } else {
                SourceSelectionKind::TopTracks
            },
        }
    }
}

impl SpotifySources for Catalog {
    fn artist_tracks(&self, _: &str) -> Result<SourceSelection, fsonos_api::Failure> {
        Ok(Self::selection(false))
    }

    fn show_episodes(&self, _: &str) -> Result<SourceSelection, fsonos_api::Failure> {
        Ok(Self::selection(true))
    }
}

fn fixture(generation: u8) -> Fixture {
    let mut f = Fixture::new(generation);
    f.surface = f.surface.with_spotify_sources(Box::new(Catalog));
    soap::set_mute(&f.sim.lan(), f.host, true).unwrap();
    f
}

fn save(f: &Fixture, definition: &SceneDefinition) {
    f.surface
        .save_scene_from(&Client::Cli, "Restorable", definition)
        .unwrap();
}

fn assert_original_mutes(f: &Fixture) {
    assert!(soap::get_mute(&f.sim.lan(), f.host).unwrap());
    assert!(!soap::get_mute(&f.sim.lan(), f.sim.player("Beta").unwrap().ip).unwrap());
}

#[test]
fn container_and_catalog_scenes_restore_original_occurrence_mode_levels_and_mutes() {
    for generation in [1, 2] {
        for source in 0..6 {
            for playing in [false, true] {
                let f = fixture(generation);
                save(&f, &definition(&f, source, playing));
                let start = f.sim.soap_log().len();
                let applied = f
                    .surface
                    .apply_scene(&Client::LoopbackHttp, "Restorable")
                    .unwrap();
                assert!(
                    applied.complete && applied.changed,
                    "{generation}/{source}/{playing}: {applied:?}"
                );
                assert!(!applied.done.contains("undo cannot restore"), "{applied:?}");
                assert_ne!(f.queue(), [DUPLICATE, DUPLICATE, LAST]);
                if !playing {
                    assert!(!f.writes_since(start).iter().any(|write| write == "Play"));
                }
                let action = f.action();
                let before =
                    BeforeState::parse(action.action.before_state.as_deref().unwrap()).unwrap();
                assert_eq!(before.queue_undo.len(), 1);
                assert!(ActionDto::from(&action).undoable);
                let preview = f
                    .surface
                    .preview_undo(&Client::LoopbackHttp)
                    .unwrap()
                    .unwrap();
                assert!(preview.undoable);
                let start = f.sim.soap_log().len();
                let undone = f
                    .surface
                    .undo_checked(&Client::LoopbackHttp, preview.id)
                    .unwrap();
                assert_eq!(undone.undone, Some(action.id));
                assert!(undone.failures.is_empty(), "{undone:?}");
                f.restored();
                assert_original_mutes(&f);
                assert!(
                    !f.writes_since(start).iter().any(|write| write == "Play"),
                    "the original queue was paused"
                );
            }
        }
    }
}

#[test]
fn silent_scene_undo_restores_levels_before_resuming_the_original_playing_queue() {
    for generation in [1, 2] {
        let f = fixture(generation);
        soap::set_mute(&f.sim.lan(), f.host, false).unwrap();
        soap::play(&f.sim.lan(), f.host).unwrap();
        save(&f, &definition(&f, 3, false));
        let start = f.sim.soap_log().len();
        let applied = f
            .surface
            .apply_scene(&Client::LoopbackHttp, "Restorable")
            .unwrap();
        assert!(applied.complete);
        assert!(!f.writes_since(start).iter().any(|write| write == "Play"));
        let preview = f
            .surface
            .preview_undo(&Client::LoopbackHttp)
            .unwrap()
            .unwrap();
        assert!(preview.undoable);
        let start = f.sim.soap_log().len();
        let undone = f
            .surface
            .undo_checked(&Client::LoopbackHttp, preview.id)
            .unwrap();
        assert!(undone.failures.is_empty(), "{undone:?}");
        assert_eq!(f.queue(), [DUPLICATE, DUPLICATE, LAST]);
        assert_eq!(
            soap::get_position_info(&f.sim.lan(), f.host).unwrap().track,
            2
        );
        assert_eq!(
            soap::get_transport_info(&f.sim.lan(), f.host)
                .unwrap()
                .state,
            TransportState::Playing
        );
        let writes = f.writes_since(start);
        let play = writes.iter().position(|write| write == "Play").unwrap();
        assert_eq!(writes.iter().filter(|write| *write == "Play").count(), 1);
        assert!(
            writes
                .iter()
                .rposition(|write| write == "SetVolume")
                .unwrap()
                < play
        );
        assert!(writes.iter().rposition(|write| write == "SetMute").unwrap() < play);
    }
}

#[test]
fn completed_scene_volume_fades_retain_a_complete_queue_undo() {
    let f = fixture(2);
    save(&f, &definition(&f, 0, true));
    let applied = f
        .surface
        .apply_scene_with(
            &Client::LoopbackHttp,
            "Restorable",
            &SceneApplyRequest { fade_secs: Some(1) },
        )
        .unwrap();
    assert!(applied.complete, "{applied:?}");
    let preview = f
        .surface
        .preview_undo(&Client::LoopbackHttp)
        .unwrap()
        .unwrap();
    assert!(preview.undoable);
    let undone = f
        .surface
        .undo_checked(&Client::LoopbackHttp, preview.id)
        .unwrap();
    assert!(undone.failures.is_empty(), "{undone:?}");
    f.restored();
    assert_original_mutes(&f);
}

#[test]
fn scene_queue_undo_never_overwrites_a_later_queue_or_room_level_change_even_with_force() {
    for queue_changed in [false, true] {
        let f = fixture(2);
        save(&f, &definition(&f, 1, true));
        assert!(
            f.surface
                .apply_scene(&Client::LoopbackHttp, "Restorable")
                .unwrap()
                .complete
        );
        assert!(ActionDto::from(&f.action()).undoable);
        if queue_changed {
            soap::add_uri_to_queue(&f.sim.lan(), f.host, EXTERNAL, "", false).unwrap();
        } else {
            soap::set_volume(&f.sim.lan(), f.host, 7).unwrap();
        }
        let queue = f.queue();
        let start = f.sim.soap_log().len();
        assert!(f.surface.undo_forced(&Client::Cli, true, true).is_err());
        assert!(f.writes_since(start).is_empty());
        assert_eq!(f.queue(), queue);
    }
}

#[test]
fn regrouping_and_uncertain_scene_replacements_remain_whole_action_barriers() {
    for regroup in [false, true] {
        let f = fixture(2);
        let mut scene = definition(&f, 0, true);
        if regroup {
            scene.groups[0].members.push("Guest".into());
        }
        save(&f, &scene);
        if !regroup {
            *f.wire.fault.lock().unwrap() = Fault::LostPlayReply;
        }
        let applied = f
            .surface
            .apply_scene(&Client::LoopbackHttp, "Restorable")
            .unwrap();
        assert_eq!(applied.complete, regroup);
        assert!(
            applied
                .done
                .contains("undo cannot restore replaced queue contents")
        );
        *f.wire.fault.lock().unwrap() = Fault::None;
        f.barrier();
    }
}

#[test]
fn multi_group_scenes_require_complete_recovery_for_every_replaced_queue() {
    for second_queue_eligible in [false, true] {
        let f = fixture(2);
        let guest = f.sim.player("Guest").unwrap();
        if second_queue_eligible {
            for uri in [LAST, DUPLICATE] {
                soap::enqueue_uri(&f.sim.lan(), guest.ip, uri, "", 0, false).unwrap();
            }
            soap::set_av_transport_uri(
                &f.sim.lan(),
                guest.ip,
                &format!("x-rincon-queue:{}#0", guest.uuid),
                "",
            )
            .unwrap();
            soap::seek_track(&f.sim.lan(), guest.ip, 1).unwrap();
            soap::seek_position(&f.sim.lan(), guest.ip, 9).unwrap();
        }
        let mut scene = definition(&f, 0, true);
        let mut second = scene.groups[0].clone();
        second.coordinator = "Guest".into();
        second.members.clear();
        scene.groups.push(second);
        save(&f, &scene);
        let applied = f
            .surface
            .apply_scene(&Client::LoopbackHttp, "Restorable")
            .unwrap();
        assert!(applied.complete, "{applied:?}");
        if second_queue_eligible {
            let action = f.action();
            let before =
                BeforeState::parse(action.action.before_state.as_deref().unwrap()).unwrap();
            assert_eq!(before.queue_undo.len(), 2);
            let preview = f
                .surface
                .preview_undo(&Client::LoopbackHttp)
                .unwrap()
                .unwrap();
            assert!(preview.undoable);
            let start = f.sim.soap_log().len();
            let undone = f
                .surface
                .undo_checked(&Client::LoopbackHttp, preview.id)
                .unwrap();
            assert!(undone.failures.is_empty(), "{undone:?}");
            assert_eq!(f.queue(), [DUPLICATE, DUPLICATE, LAST]);
            let guest_queue: Vec<_> = browse_snapshot(&f.sim.lan(), guest.ip, QUEUE)
                .unwrap()
                .objects
                .into_iter()
                .map(|item| item.res.unwrap().uri)
                .collect();
            assert_eq!(guest_queue, [LAST, DUPLICATE]);
            let position = soap::get_position_info(&f.sim.lan(), guest.ip).unwrap();
            assert_eq!((position.track, position.position_secs), (1, Some(9)));
            assert!(!f.writes_since(start).iter().any(|write| write == "Play"));
        } else {
            assert!(
                applied
                    .done
                    .contains("undo cannot restore replaced queue contents")
            );
            f.barrier();
        }
    }
}
