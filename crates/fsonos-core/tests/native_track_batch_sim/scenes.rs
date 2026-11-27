//! Scene recall retains the same complete local/service metadata as direct batches.

use super::*;
use fsonos_core::scenes::{
    self, ApplyOptions, NativeTrackSelection, Scene, SceneGroup, SceneSource,
};
use fsonos_core::store::MemStore;
use fsonos_types::HouseholdId;

fn captured(rig: &Rig, playing: bool) -> Scene {
    let lan = rig.sim.lan();
    let page = playlists::contents(
        &lan,
        &rig.houses,
        &rig.coordinator,
        &rig.playlist_id,
        0,
        3,
        None,
    )
    .unwrap();
    let selection = NativeTrackSelection::capture(
        &lan,
        &rig.houses,
        &rig.coordinator,
        &rig.playlist_id,
        &[3, 2, 1, 3],
        &page.revision,
    )
    .unwrap();
    assert_eq!(selection.positions, [3, 2, 1, 3]);
    assert_eq!(selection.tracks[0], selection.tracks[3]);
    assert_eq!(selection.tracks[0].uri, selection.tracks[2].uri);
    assert_ne!(selection.tracks[0].metadata, selection.tracks[2].metadata);
    Scene {
        name: "Native sequence".into(),
        groups: vec![SceneGroup {
            coordinator: "Lead".into(),
            members: Vec::new(),
            source: SceneSource::SonosPlaylistTracks { selection },
            playing: Some(playing),
        }],
        volumes: Default::default(),
        mutes: Default::default(),
        writer: None,
    }
}

fn rig(generation: u8) -> Rig {
    let mut rig = Rig::new(generation);
    rig.houses[0].id = Some(HouseholdId(
        rig.sim.player("Lead").unwrap().household.clone(),
    ));
    rig
}

#[test]
fn mixed_local_service_scene_persists_native_occurrences_and_loads_with_or_without_sound() {
    for generation in [1, 2] {
        for playing in [false, true] {
            let rig = rig(generation);
            let lan = rig.sim.lan();
            let original = captured(&rig, playing);
            let mut store = MemStore::default();
            scenes::save(&mut store, &original, 1).unwrap();
            let saved = scenes::load(&store, "native sequence").unwrap();
            assert_eq!(saved, original);
            let SceneSource::SonosPlaylistTracks { selection } = &saved.groups[0].source else {
                panic!("native selection was not retained");
            };
            let plan = scenes::plan_apply(&saved, &rig.houses, &[]).unwrap();
            let before = rig.sim.soap_log().len();
            scenes::check_sources(&lan, &rig.houses, &plan).unwrap();
            let report = scenes::apply(&lan, &rig.houses, &plan, &ApplyOptions::default());
            assert!(report.is_complete(), "{report:?}");
            assert_eq!(rig.queue(), [LOCAL, SERVICE, LOCAL, LOCAL]);
            assert_eq!(get_play_mode(&lan, rig.host).unwrap(), PlayMode::Normal);
            assert_eq!(soap::get_position_info(&lan, rig.host).unwrap().track, 1);
            assert_eq!(
                soap::get_transport_info(&lan, rig.host).unwrap().state == TransportState::Playing,
                playing
            );
            let log = rig.sim.soap_log();
            let adds: Vec<_> = log[before..]
                .iter()
                .filter(|entry| entry.action == "AddURIToQueue")
                .collect();
            assert_eq!(adds.len(), 4);
            for (call, track) in adds.iter().zip(&selection.tracks) {
                assert_eq!(
                    call.args
                        .iter()
                        .find(|(name, _)| name == "EnqueuedURI")
                        .unwrap()
                        .1,
                    track.uri
                );
                assert_eq!(
                    call.args
                        .iter()
                        .find(|(name, _)| name == "EnqueuedURIMetaData")
                        .unwrap()
                        .1,
                    track.metadata
                );
            }
            assert_eq!(
                rig.writes(before)
                    .iter()
                    .filter(|action| *action == "Play")
                    .count(),
                usize::from(playing)
            );
            assert!(log[before..].iter().all(|entry| {
                !entry.args.iter().any(|(name, value)| {
                    name == "ObjectID" && (value == "FV:2" || value.starts_with("SQ:"))
                })
            }));
        }
    }
}

#[test]
fn incomplete_native_scene_selection_never_discards_the_old_queue_or_starts_its_partial_tail() {
    for generation in [1, 2] {
        let rig = rig(generation);
        let saved = captured(&rig, true);
        let plan = scenes::plan_apply(&saved, &rig.houses, &[]).unwrap();
        let transport = Faulty {
            lan: rig.sim.lan(),
            failure: Failure::RefusedSecond,
            adds: Cell::new(0),
        };
        let before = rig.sim.soap_log().len();
        let report = scenes::apply(&transport, &rig.houses, &plan, &ApplyOptions::default());
        assert!(!report.is_complete());
        assert_eq!(transport.adds.get(), 2);
        assert_eq!(rig.queue(), [ORIGINAL[0], ORIGINAL[1], ORIGINAL[2], LOCAL]);
        assert!(!rig.writes(before).iter().any(|action| {
            [
                "Play",
                "RemoveAllTracksFromQueue",
                "RemoveTrackRangeFromQueue",
            ]
            .contains(&action.as_str())
        }));
    }
}

#[test]
fn malformed_retained_native_metadata_cannot_become_a_scene_operation() {
    let rig = rig(2);
    for change in 0..3 {
        let mut saved = captured(&rig, true);
        let SceneSource::SonosPlaylistTracks { selection } = &mut saved.groups[0].source else {
            unreachable!();
        };
        match change {
            0 => selection.tracks[0].uri = EXTERNAL.into(),
            1 => selection.playlist.household = HouseholdId("FOREIGN".into()),
            _ => selection.positions.pop().map(|_| ()).unwrap(),
        }
        let before = rig.sim.soap_log().len();
        assert!(scenes::plan_apply(&saved, &rig.houses, &[]).is_err());
        assert!(rig.writes(before).is_empty());
    }
}
