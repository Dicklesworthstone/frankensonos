use super::*;
use fsonos_api::surface::sonos_playlists::SonosPlaylistTracksRequest;
use fsonos_core::actions::BeforeState;
use fsonos_proto::play_mode::{get_play_mode, set_play_mode};
use fsonos_types::PlayMode;

fn selection(s: &Surface, zone: &str, mode: SonosPlaylistTrackMode) -> SonosPlaylistTracksRequest {
    let page = s
        .sonos_playlist_contents(&Client::Cli, &contents(zone, "SQ:7", 100))
        .unwrap();
    SonosPlaylistTracksRequest {
        zone: zone.into(),
        playlist: page.playlist.id,
        positions: vec![3, 2, 3],
        expected_revision: page.revision,
        mode,
        expected_queue_revision: s.queue(&Client::Cli, zone, 0, 1).unwrap().revision,
    }
}

#[test]
fn native_batch_play_uses_member_coordinator_caps_and_one_complete_undo() {
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
        let houses = households(&sim);
        let target = resolve_room(&houses, &member_zone).unwrap();
        let host = target.coordinator.ip;
        let mut policy = Policy::default();
        policy.defaults.max_volume = 25;
        let s = surface(Box::new(sim.lan()), &houses, policy);
        seed_native_queue(&s, &zone);
        control::play_queue_from(&sim.lan(), &houses, &target.coordinator.id, 2).unwrap();
        soap::pause(&sim.lan(), host).unwrap();
        soap::seek_position(&sim.lan(), host, 17).unwrap();
        set_play_mode(&sim.lan(), host, PlayMode::ShuffleRepeatOne).unwrap();
        soap::set_volume(&sim.lan(), host, 80).unwrap();
        soap::set_volume(&sim.lan(), target.player.ip, 70).unwrap();
        let original = queue(&sim, &houses, &zone);
        let asked = selection(&s, &member_zone, SonosPlaylistTrackMode::Play);
        let start = sim.soap_log().len();
        let result = s.sonos_playlist_tracks(&Client::McpStdio, &asked).unwrap();
        assert!(result.changed);
        assert_eq!(result.zone, "Den");
        assert_eq!(result.positions, [3, 2, 3]);
        assert_eq!(result.titles.len(), 3);
        assert_eq!(result.titles[0], result.titles[2]);
        assert_eq!(
            queue(&sim, &houses, &zone),
            [
                original[2].clone(),
                original[1].clone(),
                original[2].clone()
            ]
        );
        assert_eq!(get_play_mode(&sim.lan(), host).unwrap(), PlayMode::Normal);
        assert_eq!(soap::get_position_info(&sim.lan(), host).unwrap().track, 1);
        assert_eq!(
            soap::get_transport_info(&sim.lan(), host).unwrap().state,
            TransportState::Playing
        );
        assert_eq!(soap::get_volume(&sim.lan(), host).unwrap(), 25);
        assert_eq!(soap::get_volume(&sim.lan(), target.player.ip).unwrap(), 25);
        let calls = sim.soap_log();
        let writes = &calls[start..];
        let removal = writes
            .iter()
            .position(|call| call.action == "RemoveTrackRangeFromQueue")
            .unwrap();
        assert_eq!(
            writes[..removal]
                .iter()
                .filter(|call| call.action == "AddURIToQueue")
                .count(),
            3
        );
        assert!(
            !writes
                .iter()
                .any(|call| call.action == "RemoveAllTracksFromQueue")
        );
        let actions = s
            .recent_actions(&Client::McpStdio, &ActionFilter::default())
            .unwrap();
        let action = &actions[0].action;
        assert!(
            action
                .intent
                .starts_with("play_favorite: play saved playlist SQ:7 occurrences [3, 2, 3]")
        );
        let saved = BeforeState::parse(action.before_state.as_deref().unwrap()).unwrap();
        assert_eq!(saved.queue_undo.len(), 1);
        let preview = s.preview_undo(&Client::McpStdio).unwrap().unwrap();
        assert!(preview.undoable);
        let undone = s.undo_checked(&Client::McpStdio, preview.id).unwrap();
        assert!(undone.failures.is_empty(), "{undone:?}");
        assert_eq!(queue(&sim, &houses, &zone), original);
        let position = soap::get_position_info(&sim.lan(), host).unwrap();
        assert_eq!((position.track, position.position_secs), (2, Some(17)));
        assert_eq!(
            get_play_mode(&sim.lan(), host).unwrap(),
            PlayMode::ShuffleRepeatOne
        );
        assert!(matches!(
            soap::get_transport_info(&sim.lan(), host).unwrap().state,
            TransportState::Paused | TransportState::Stopped
        ));
    }
}

#[test]
fn native_next_and_append_are_silent_ordered_batches_with_exact_range_undo() {
    for generation in ["S1", "S2"] {
        for mode in [SonosPlaylistTrackMode::Next, SonosPlaylistTrackMode::Append] {
            let sim = sim();
            let houses = households(&sim);
            let zone = format!("Den@{generation}");
            let target = resolve_room(&houses, &zone).unwrap();
            let host = target.coordinator.ip;
            let s = surface(Box::new(sim.lan()), &houses, Policy::default());
            seed_native_queue(&s, &zone);
            control::play_queue_from(&sim.lan(), &houses, &target.coordinator.id, 2).unwrap();
            soap::pause(&sim.lan(), host).unwrap();
            let original = queue(&sim, &houses, &zone);
            let before = control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap();
            let asked = selection(&s, &zone, mode);
            let start = sim.soap_log().len();
            let added = s.sonos_playlist_tracks(&Client::McpStdio, &asked).unwrap();
            let first = if mode == SonosPlaylistTrackMode::Next {
                3
            } else {
                4
            };
            assert_eq!(
                (added.first, added.count, added.playing_queue),
                (Some(first), Some(3), Some(true))
            );
            let mut expected = original.clone();
            expected.splice(
                (first - 1) as usize..(first - 1) as usize,
                [
                    original[2].clone(),
                    original[1].clone(),
                    original[2].clone(),
                ],
            );
            assert_eq!(queue(&sim, &houses, &zone), expected);
            let after = control::playback(&sim.lan(), &houses, &target.coordinator.id).unwrap();
            assert_eq!(after, before);
            assert!(!sim.soap_log()[start..].iter().any(|call| matches!(
                call.action.as_str(),
                "Play" | "Pause" | "SetAVTransportURI" | "SetVolume" | "Seek"
            )));
            let preview = s.preview_undo(&Client::McpStdio).unwrap().unwrap();
            assert!(preview.undoable);
            assert!(
                s.undo_checked(&Client::McpStdio, preview.id)
                    .unwrap()
                    .failures
                    .is_empty()
            );
            assert_eq!(queue(&sim, &houses, &zone), original);
        }
    }
}

#[test]
fn stale_or_unavailable_native_batch_selection_changes_no_queue_playback_or_levels() {
    let sim = sim();
    let houses = households(&sim);
    let s = surface(Box::new(sim.lan()), &houses, Policy::default());
    for mode in [
        SonosPlaylistTrackMode::Play,
        SonosPlaylistTrackMode::Next,
        SonosPlaylistTrackMode::Append,
    ] {
        let original = selection(&s, "Den@S2", mode);
        for changed in 0..3 {
            let mut asked = original.clone();
            match changed {
                0 => asked.expected_revision = "obsolete-native-observation".into(),
                1 => asked.expected_queue_revision = Some("obsolete-queue-observation".into()),
                _ => asked.positions = vec![1, 4],
            }
            let start = sim.soap_log().len();
            assert!(s.sonos_playlist_tracks(&Client::McpStdio, &asked).is_err());
            assert!(
                sim.soap_log()[start..]
                    .iter()
                    .all(|call| call.action == "Browse" || call.action.starts_with("Get"))
            );
            assert!(queue(&sim, &houses, "Den@S2").is_empty());
        }
    }
}

struct RefuseSecondBatchAdd {
    inner: SimLan,
    adds: AtomicUsize,
}

impl Transport for RefuseSecondBatchAdd {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if action.ends_with("#AddURIToQueue\"") && self.adds.fetch_add(1, Ordering::SeqCst) == 1 {
            return Err(ProtoError::SoapFault {
                code: 711,
                reason: "second selected track refused".into(),
            });
        }
        self.inner.soap_post(host, path, action, body)
    }
}

#[test]
fn a_partial_native_replacement_keeps_original_queue_and_blocks_older_undo() {
    let sim = sim();
    let houses = households(&sim);
    let reader = surface(Box::new(sim.lan()), &houses, Policy::default());
    seed_native_queue(&reader, "Den@S2");
    let original = queue(&sim, &houses, "Den@S2");
    let asked = selection(&reader, "Den@S2", SonosPlaylistTrackMode::Play);
    let s = surface(
        Box::new(RefuseSecondBatchAdd {
            inner: sim.lan(),
            adds: AtomicUsize::new(0),
        }),
        &houses,
        Policy::default(),
    );
    let start = sim.soap_log().len();
    let failure = s
        .sonos_playlist_tracks(&Client::McpStdio, &asked)
        .unwrap_err();
    assert_eq!(failure.code, ErrorCode::OutcomeUncertain);
    let after = queue(&sim, &houses, "Den@S2");
    assert_eq!(after.len(), original.len() + 1);
    assert_eq!(after[..original.len()], original);
    let writes = sim.soap_log();
    assert_eq!(
        writes[start..]
            .iter()
            .filter(|call| call.action == "AddURIToQueue")
            .count(),
        1
    );
    assert!(!writes[start..].iter().any(|call| matches!(
        call.action.as_str(),
        "Play" | "RemoveAllTracksFromQueue" | "RemoveTrackRangeFromQueue"
    )));
    let preview = s.preview_undo(&Client::McpStdio).unwrap().unwrap();
    assert!(!preview.undoable);
}

#[test]
fn lost_native_batch_add_replies_are_observed_without_duplicates_or_extra_actions() {
    let sim = sim();
    let houses = households(&sim);
    let reader = surface(Box::new(sim.lan()), &houses, Policy::default());
    let asked = selection(&reader, "Den@S2", SonosPlaylistTrackMode::Append);
    let s = surface(
        Box::new(LoseTrackEnqueueReply(sim.lan())),
        &houses,
        Policy::default(),
    );
    let start = sim.soap_log().len();
    let added = s.sonos_playlist_tracks(&Client::McpStdio, &asked).unwrap();
    assert_eq!(added.count, Some(3));
    assert_eq!(queue(&sim, &houses, "Den@S2").len(), 3);
    assert_eq!(
        sim.soap_log()[start..]
            .iter()
            .filter(|call| call.action == "AddURIToQueue")
            .count(),
        3
    );
    assert_eq!(
        s.recent_actions(&Client::Cli, &ActionFilter::default())
            .unwrap()
            .len(),
        1
    );
    let preview = s.preview_undo(&Client::McpStdio).unwrap().unwrap();
    assert!(
        s.undo_checked(&Client::McpStdio, preview.id)
            .unwrap()
            .failures
            .is_empty()
    );
    assert!(queue(&sim, &houses, "Den@S2").is_empty());
}
