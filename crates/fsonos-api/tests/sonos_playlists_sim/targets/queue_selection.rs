//! Live queue occurrences reuse the native selection executor and undo path.

use super::*;
use fsonos_api::surface::queue::QueueSelectionRequest;

fn request(
    _house: &House,
    surface: &Surface,
    generation: &str,
    target: &str,
    mode: SonosPlaylistTrackMode,
) -> QueueSelectionRequest {
    let zone = format!("Den@{generation}");
    QueueSelectionRequest {
        expected_revision: surface
            .queue(&Client::Cli, &zone, 0, 3)
            .unwrap()
            .revision
            .unwrap(),
        zone,
        target: Some(target.into()),
        expected_target_revision: Some(
            surface
                .resolve_rooms(&Client::Cli, target)
                .unwrap()
                .revision,
        ),
        positions: vec![3, 2, 3],
        mode,
        expected_queue_revision: None,
    }
}

#[test]
fn live_queue_selection_copies_to_other_groups_before_changing_its_source() {
    for generation in ["S1", "S2"] {
        for mode in [
            SonosPlaylistTrackMode::Play,
            SonosPlaylistTrackMode::Next,
            SonosPlaylistTrackMode::Append,
        ] {
            let house = House::new();
            let mut policy = Policy::default();
            policy.defaults.max_volume = 25;
            let surface = house.surface(Box::new(house.sim.lan()), policy);
            let alias = format!("selected_{}", generation.to_lowercase());
            let request = request(&house, &surface, generation, &alias, mode);
            let original = house.queue(&format!("Den@{generation}"));
            let before = ["Den", "Office"].map(|room| {
                control::playback(
                    &house.sim.lan(),
                    &house.households,
                    &house.target(&format!("{room}@{generation}")).coordinator.id,
                )
                .unwrap()
            });
            let start = house.sim.soap_log().len();
            let selected = surface
                .queue_selection(&Client::McpStdio, &request)
                .unwrap();
            assert!(selected.complete() && selected.changed, "{selected:?}");
            assert_eq!(selected.positions, [3, 2, 3]);
            assert_eq!(selected.titles[0], selected.titles[2]);
            let wanted = [
                original[2].clone(),
                original[1].clone(),
                original[2].clone(),
            ];
            if mode == SonosPlaylistTrackMode::Play {
                let batch = selected.batch.as_ref().unwrap();
                assert_eq!(
                    batch.results.len(),
                    2,
                    "grouped alias members are deduplicated"
                );
                assert_eq!(batch.results[0].zone, "Office");
                assert_eq!(batch.results[1].zone, "Den", "the source must run last");
                for room in ["Den", "Office"] {
                    let zone = format!("{room}@{generation}");
                    assert_eq!(house.queue(&zone), wanted);
                    let host = house.target(&zone).coordinator.ip;
                    assert_eq!(
                        get_play_mode(&house.sim.lan(), host).unwrap(),
                        PlayMode::Normal
                    );
                    assert_eq!(
                        soap::get_position_info(&house.sim.lan(), host)
                            .unwrap()
                            .track,
                        1
                    );
                    assert_eq!(
                        soap::get_transport_info(&house.sim.lan(), host)
                            .unwrap()
                            .state,
                        TransportState::Playing
                    );
                }
                for room in ["Den", "Member", "Office"] {
                    assert_eq!(
                        soap::get_volume(
                            &house.sim.lan(),
                            house.target(&format!("{room}@{generation}")).player.ip
                        )
                        .unwrap(),
                        25
                    );
                }
            } else {
                let batch = selected.queue_batch.as_ref().unwrap();
                assert!(batch.complete && batch.logged && batch.undoable);
                assert_eq!(batch.added.len(), 2);
                assert_eq!(batch.added[0].queue.zone, "Office");
                assert_eq!(batch.added[1].queue.zone, "Den", "the source must run last");
                for (i, room) in ["Den", "Office"].into_iter().enumerate() {
                    let zone = format!("{room}@{generation}");
                    let first = if mode == SonosPlaylistTrackMode::Append {
                        4
                    } else if room == "Den" {
                        3
                    } else {
                        2
                    };
                    let mut expected = original.clone();
                    expected.splice(first - 1..first - 1, wanted.clone());
                    assert_eq!(house.queue(&zone), expected);
                    assert_eq!(
                        control::playback(
                            &house.sim.lan(),
                            &house.households,
                            &house.target(&zone).coordinator.id
                        )
                        .unwrap(),
                        before[i]
                    );
                }
                assert!(
                    house
                        .writes(start)
                        .iter()
                        .all(|call| call.action == "AddURIToQueue")
                );
            }
            assert_eq!(
                house
                    .writes(start)
                    .iter()
                    .filter(|call| call.action == "AddURIToQueue")
                    .count(),
                6
            );
            assert_eq!(
                surface
                    .recent_actions(&Client::Cli, &ActionFilter::default())
                    .unwrap()
                    .len(),
                1
            );
            house.undo(&surface);
            for room in ["Den", "Office"] {
                assert_eq!(house.queue(&format!("{room}@{generation}")), original);
            }
        }
    }
}

#[test]
fn excluding_the_source_preserves_its_queue_playhead_mode_and_volume() {
    for mode in [
        SonosPlaylistTrackMode::Play,
        SonosPlaylistTrackMode::Next,
        SonosPlaylistTrackMode::Append,
    ] {
        let house = House::new();
        let surface = house.surface(Box::new(house.sim.lan()), Policy::default());
        let mut request = request(&house, &surface, "S2", "Office@S2", mode);
        request.expected_queue_revision = surface
            .queue(&Client::Cli, "Office@S2", 0, 1)
            .unwrap()
            .revision;
        let source = house.target("Den@S2");
        let before =
            control::playback(&house.sim.lan(), &house.households, &source.coordinator.id).unwrap();
        let queue = house.queue("Den@S2");
        let result = surface
            .queue_selection(&Client::McpStdio, &request)
            .unwrap();
        assert!(result.complete());
        assert_eq!(result.source_zone, "Den@S2");
        assert_eq!(result.zone, "Office");
        assert_eq!(house.queue("Den@S2"), queue);
        assert_eq!(
            control::playback(&house.sim.lan(), &house.households, &source.coordinator.id).unwrap(),
            before
        );
        assert_eq!(
            soap::get_volume(&house.sim.lan(), source.player.ip).unwrap(),
            80
        );
        assert_eq!(
            get_play_mode(&house.sim.lan(), source.coordinator.ip).unwrap(),
            PlayMode::ShuffleRepeatOne
        );
        house.undo(&surface);
        assert_eq!(house.queue("Office@S2"), queue);
    }
}

#[test]
fn invalid_source_and_destination_proofs_reject_all_groups_before_writes() {
    let house = House::new();
    let surface = house.surface(Box::new(house.sim.lan()), Policy::default());
    for mode in [
        SonosPlaylistTrackMode::Play,
        SonosPlaylistTrackMode::Next,
        SonosPlaylistTrackMode::Append,
    ] {
        for case in 0..4 {
            let mut request = request(&house, &surface, "S2", "all@S2", mode);
            match case {
                0 => request.expected_revision = "stale-source".into(),
                1 => {
                    request.target = Some("Office@S1".into());
                    request.expected_target_revision = None;
                }
                2 => {
                    request.expected_queue_revision = surface
                        .queue(&Client::Cli, "Office@S2", 0, 1)
                        .unwrap()
                        .revision;
                }
                _ => request.positions = vec![1, 99],
            }
            let start = house.sim.soap_log().len();
            assert!(
                surface
                    .queue_selection(&Client::McpStdio, &request)
                    .is_err()
            );
            assert!(house.writes(start).is_empty(), "case {case}");
        }
    }
}

#[test]
fn a_failed_source_destination_retains_successful_other_room_receipts_without_replay() {
    for mode in [SonosPlaylistTrackMode::Play, SonosPlaylistTrackMode::Append] {
        for after_prefix in [false, true] {
            let house = House::new();
            let reader = house.surface(Box::new(house.sim.lan()), Policy::default());
            let request = request(&house, &reader, "S2", "all@S2", mode);
            let original = house.queue("Den@S2");
            let surface = house.surface(
                Box::new(RefuseDestinationAdd {
                    inner: house.sim.lan(),
                    destination: house.target("Den@S2").coordinator.ip,
                    after_prefix,
                    adds: AtomicUsize::new(0),
                }),
                Policy::default(),
            );
            let start = house.sim.soap_log().len();
            let result = surface
                .queue_selection(&Client::McpStdio, &request)
                .unwrap();
            assert!(!result.complete() && result.changed, "{result:?}");
            if mode == SonosPlaylistTrackMode::Play {
                let batch = result.batch.unwrap();
                assert_eq!(batch.results.len(), 2);
                assert_eq!(batch.results[0].zone, "Office");
                assert!(batch.results[0].result.complete());
                assert_eq!(batch.results[1].zone, "Den");
                assert!(!batch.results[1].result.complete());
                assert_eq!(house.queue("Office@S2").len(), 3);
            } else {
                let batch = result.queue_batch.unwrap();
                assert_eq!(batch.added.len(), 1);
                assert_eq!(batch.added[0].queue.zone, "Office");
                assert_eq!(batch.failed.len(), 1);
                assert_eq!(batch.failed[0].zone, "Den");
                assert!(!batch.undoable);
                assert_eq!(house.queue("Office@S2").len(), 6);
            }
            assert_eq!(
                house.queue("Den@S2").len(),
                original.len() + usize::from(after_prefix)
            );
            assert_eq!(
                house
                    .writes(start)
                    .iter()
                    .filter(|call| call.action == "AddURIToQueue")
                    .count(),
                3 + usize::from(after_prefix)
            );
            assert!(
                !surface
                    .preview_undo(&Client::McpStdio)
                    .unwrap()
                    .unwrap()
                    .undoable
            );
        }
    }
}
