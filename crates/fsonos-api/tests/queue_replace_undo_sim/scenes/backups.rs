//! A durable scene backup is another complete queue replacement: one reviewed
//! Undo restores the intervening occurrence, playback, levels and mutes.

use super::*;

const SAVED: [&str; 3] = [LAST, EXTERNAL, LAST];
const NAME: &str = "Durable replacement";

fn load_queue(
    f: &Fixture,
    player: &str,
    entries: &[&str],
    track: u32,
    seconds: u32,
    mode: PlayMode,
    playing: bool,
) {
    let player = f.sim.player(player).unwrap();
    let lan = f.sim.lan();
    soap::remove_all_tracks_from_queue(&lan, player.ip).unwrap();
    for uri in entries {
        soap::add_uri_to_queue(&lan, player.ip, uri, "", false).unwrap();
    }
    soap::set_av_transport_uri(
        &lan,
        player.ip,
        &format!("x-rincon-queue:{}#0", player.uuid),
        "",
    )
    .unwrap();
    set_play_mode(&lan, player.ip, mode).unwrap();
    soap::seek_track(&lan, player.ip, track).unwrap();
    soap::seek_position(&lan, player.ip, seconds).unwrap();
    if playing {
        soap::play(&lan, player.ip).unwrap();
    } else {
        soap::pause(&lan, player.ip).unwrap();
    }
}

fn prepare(f: &Fixture, saved_playing: bool, original_playing: bool, second: bool) {
    let lan = f.sim.lan();
    let beta = f.sim.player("Beta").unwrap().ip;
    let guest = f.sim.player("Guest").unwrap().ip;
    load_queue(
        f,
        "Alpha",
        &SAVED,
        3,
        29,
        PlayMode::ShuffleNoRepeat,
        saved_playing,
    );
    soap::set_volume(&lan, f.host, 12).unwrap();
    soap::set_volume(&lan, beta, 18).unwrap();
    soap::set_mute(&lan, f.host, false).unwrap();
    soap::set_mute(&lan, beta, true).unwrap();
    soap::set_volume(&lan, guest, 25).unwrap();
    if second {
        load_queue(
            f,
            "Guest",
            &[LAST, EXTERNAL],
            2,
            31,
            PlayMode::Normal,
            false,
        );
        soap::set_volume(&lan, guest, 15).unwrap();
        soap::set_mute(&lan, guest, true).unwrap();
    }
    let start = f.sim.soap_log().len();
    let saved = f
        .surface
        .save_scene_with_queue_backups(&Client::Cli, NAME)
        .unwrap();
    assert!(
        f.writes_since(start).is_empty(),
        "saving a backup only reads"
    );
    assert_eq!(
        saved
            .groups
            .iter()
            .filter(|group| group.source.kind == "queue_backup")
            .count(),
        if second { 2 } else { 1 }
    );
    let group = saved
        .groups
        .iter()
        .find(|group| {
            group
                .source
                .queue
                .as_ref()
                .is_some_and(|queue| queue.coordinator == f.coordinator.0)
        })
        .unwrap();
    let queue = group.source.queue.as_ref().unwrap();
    assert_eq!(
        (queue.total, queue.track, queue.position_secs),
        (Some(3), 3, 29)
    );
    assert_eq!(queue.play_mode.as_deref(), Some("SHUFFLE_NOREPEAT"));

    load_queue(
        f,
        "Alpha",
        &[DUPLICATE, DUPLICATE, LAST],
        2,
        17,
        PlayMode::RepeatAll,
        original_playing,
    );
    soap::set_volume(&lan, f.host, 80).unwrap();
    soap::set_volume(&lan, beta, 70).unwrap();
    soap::set_mute(&lan, f.host, true).unwrap();
    soap::set_mute(&lan, beta, false).unwrap();
    if second {
        load_queue(
            f,
            "Guest",
            &[EXTERNAL, DUPLICATE],
            1,
            9,
            PlayMode::RepeatAll,
            false,
        );
        soap::set_volume(&lan, guest, 60).unwrap();
        soap::set_mute(&lan, guest, false).unwrap();
    }
}

fn assert_original(f: &Fixture, playing: bool) {
    assert_eq!(f.queue(), [DUPLICATE, DUPLICATE, LAST]);
    let position = soap::get_position_info(&f.sim.lan(), f.host).unwrap();
    assert_eq!((position.track, position.position_secs), (2, Some(17)));
    assert_eq!(
        position.uri, DUPLICATE,
        "the second occurrence must survive"
    );
    assert_eq!(
        get_play_mode(&f.sim.lan(), f.host).unwrap(),
        PlayMode::RepeatAll
    );
    assert_eq!(
        soap::get_transport_info(&f.sim.lan(), f.host)
            .unwrap()
            .state
            == TransportState::Playing,
        playing
    );
    for name in ["Alpha", "Beta", "Guest"] {
        assert_eq!(
            soap::get_volume(&f.sim.lan(), f.sim.player(name).unwrap().ip).unwrap(),
            25,
            "Undo must retain playback caps"
        );
    }
    assert_original_mutes(f);
}

#[test]
fn durable_backup_recall_has_verified_undo_on_both_generations_for_playing_and_paused_queues() {
    for generation in [1, 2] {
        for saved_playing in [false, true] {
            for original_playing in [false, true] {
                let f = fixture(generation);
                prepare(&f, saved_playing, original_playing, false);
                let start = f.sim.soap_log().len();
                let applied = f.surface.apply_scene(&Client::LoopbackHttp, NAME).unwrap();
                assert!(
                    applied.complete && applied.changed,
                    "{generation}/{saved_playing}/{original_playing}: {applied:?}"
                );
                assert!(!applied.done.contains("undo cannot restore"), "{applied:?}");
                assert_eq!(f.queue(), SAVED);
                let position = soap::get_position_info(&f.sim.lan(), f.host).unwrap();
                assert_eq!((position.track, position.position_secs), (3, Some(29)));
                assert_eq!(
                    get_play_mode(&f.sim.lan(), f.host).unwrap(),
                    PlayMode::ShuffleNoRepeat
                );
                assert_eq!(
                    soap::get_transport_info(&f.sim.lan(), f.host)
                        .unwrap()
                        .state
                        == TransportState::Playing,
                    saved_playing
                );
                if !saved_playing {
                    assert!(!f.writes_since(start).iter().any(|write| write == "Play"));
                }
                let action = f.action();
                let before =
                    BeforeState::parse(action.action.before_state.as_deref().unwrap()).unwrap();
                assert_eq!(before.queue_undo.len(), 1);
                assert_eq!(before.queue_undo[0].coordinator(), &f.coordinator);
                assert!(ActionDto::from(&action).undoable);
                let preview = f
                    .surface
                    .preview_undo(&Client::LoopbackHttp)
                    .unwrap()
                    .unwrap();
                assert_eq!(preview.id, action.id);
                assert!(preview.undoable);
                let start = f.sim.soap_log().len();
                let undone = f
                    .surface
                    .undo_checked(&Client::LoopbackHttp, preview.id)
                    .unwrap();
                assert_eq!(undone.undone, Some(action.id));
                assert!(undone.failures.is_empty(), "{undone:?}");
                assert_original(&f, original_playing);
                let writes = f.writes_since(start);
                assert_eq!(
                    writes.iter().filter(|write| *write == "Play").count(),
                    usize::from(original_playing)
                );
                if original_playing {
                    let play = writes.iter().position(|write| write == "Play").unwrap();
                    assert!(
                        writes
                            .iter()
                            .rposition(|write| write == "SetVolume")
                            .unwrap()
                            < play
                    );
                    assert!(writes.iter().rposition(|write| write == "SetMute").unwrap() < play);
                }
                let start = f.sim.soap_log().len();
                let replay = f
                    .surface
                    .undo_checked(&Client::LoopbackHttp, preview.id)
                    .unwrap_err();
                assert_eq!(replay.code, ErrorCode::InvalidArgument);
                assert!(
                    f.writes_since(start).is_empty(),
                    "the consumed Undo cannot run twice"
                );
            }
        }
    }
}

#[test]
fn two_backup_recalls_share_one_complete_undo_or_one_whole_action_barrier() {
    for eligible in [false, true] {
        let f = fixture(2);
        prepare(&f, false, false, true);
        let guest = f.sim.player("Guest").unwrap();
        if !eligible {
            soap::set_av_transport_uri(
                &f.sim.lan(),
                guest.ip,
                "x-rincon-mp3radio://example.invalid/intervening-radio.mp3",
                "",
            )
            .unwrap();
        }
        let applied = f.surface.apply_scene(&Client::LoopbackHttp, NAME).unwrap();
        assert!(applied.complete && applied.changed, "{applied:?}");
        assert_eq!(f.queue(), SAVED);
        if !eligible {
            assert!(
                applied
                    .done
                    .contains("undo cannot restore replaced queue contents")
            );
            f.barrier();
            continue;
        }
        let action = f.action();
        let before = BeforeState::parse(action.action.before_state.as_deref().unwrap()).unwrap();
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
        assert_original(&f, false);
        let queue: Vec<_> = browse_snapshot(&f.sim.lan(), guest.ip, QUEUE)
            .unwrap()
            .objects
            .into_iter()
            .map(|entry| entry.res.unwrap().uri)
            .collect();
        assert_eq!(queue, [EXTERNAL, DUPLICATE]);
        let position = soap::get_position_info(&f.sim.lan(), guest.ip).unwrap();
        assert_eq!((position.track, position.position_secs), (1, Some(9)));
        assert_eq!(
            get_play_mode(&f.sim.lan(), guest.ip).unwrap(),
            PlayMode::RepeatAll
        );
        assert!(!soap::get_mute(&f.sim.lan(), guest.ip).unwrap());
        assert!(!f.writes_since(start).iter().any(|write| write == "Play"));
    }
}

#[test]
fn an_uncertain_backup_start_is_a_barrier_and_never_replays_the_rebuild_or_play() {
    for generation in [1, 2] {
        let f = fixture(generation);
        prepare(&f, true, false, false);
        *f.wire.fault.lock().unwrap() = Fault::LostPlayReply;
        let start = f.sim.soap_log().len();
        let applied = f.surface.apply_scene(&Client::LoopbackHttp, NAME).unwrap();
        assert!(!applied.complete, "{applied:?}");
        let writes = f.writes_since(start);
        assert_eq!(writes.iter().filter(|write| *write == "Play").count(), 1);
        assert_eq!(
            writes
                .iter()
                .filter(|write| *write == "RemoveTrackRangeFromQueue")
                .count(),
            1
        );
        assert_eq!(
            writes
                .iter()
                .filter(|write| *write == "AddURIToQueue")
                .count(),
            SAVED.len()
        );
        *f.wire.fault.lock().unwrap() = Fault::None;
        f.barrier();
    }
}

#[test]
fn backup_scene_undo_refuses_later_queue_occurrence_mode_level_and_group_changes_even_with_force() {
    for change in 0..5 {
        let f = fixture(2);
        prepare(&f, false, false, false);
        assert!(
            f.surface
                .apply_scene(&Client::LoopbackHttp, NAME)
                .unwrap()
                .complete
        );
        assert!(ActionDto::from(&f.action()).undoable);
        match change {
            0 => {
                soap::add_uri_to_queue(&f.sim.lan(), f.host, EXTERNAL, "", false).unwrap();
            }
            1 => {
                soap::seek_track(&f.sim.lan(), f.host, 1).unwrap();
            }
            2 => {
                set_play_mode(&f.sim.lan(), f.host, PlayMode::Normal).unwrap();
            }
            3 => {
                soap::set_volume(&f.sim.lan(), f.host, 7).unwrap();
            }
            _ => {
                let member = f.sim.player("Beta").unwrap();
                soap::leave_group(&f.sim.lan(), member.ip, &PlayerId(member.uuid.clone())).unwrap();
            }
        }
        let queue = f.queue();
        let start = f.sim.soap_log().len();
        assert!(f.surface.undo_forced(&Client::Cli, false, true).is_err());
        assert!(f.writes_since(start).is_empty());
        assert_eq!(f.queue(), queue);
    }
}

#[test]
fn new_policy_denial_prevents_backup_undo_writes_and_consumes_the_attempt_without_retry() {
    let directory = tempfile::tempdir().unwrap();
    let policy = directory.path().join(fsonos_core::policy::FILE_NAME);
    std::fs::write(&policy, "[defaults]\nmax_volume = 25\n").unwrap();
    let mut f = fixture(2);
    f.surface = f.surface.with_policy_file(directory.path().to_path_buf());
    prepare(&f, false, false, false);
    assert!(
        f.surface
            .apply_scene(&Client::LoopbackHttp, NAME)
            .unwrap()
            .complete
    );
    let preview = f
        .surface
        .preview_undo(&Client::LoopbackHttp)
        .unwrap()
        .unwrap();
    assert!(preview.undoable);
    std::fs::write(
        &policy,
        "[defaults]\nmax_volume = 25\n[clients.\"loopback-http\"]\ndeny = [\"clear_queue\"]\n",
    )
    .unwrap();
    let queue = f.queue();
    let start = f.sim.soap_log().len();
    let undone = f
        .surface
        .undo_checked(&Client::LoopbackHttp, preview.id)
        .unwrap();
    assert!(!undone.failures.is_empty(), "{undone:?}");
    assert!(
        undone
            .failures
            .iter()
            .any(|failure| failure.contains("clear_queue"))
    );
    assert!(f.writes_since(start).is_empty());
    assert_eq!(f.queue(), queue);
    let replay = f
        .surface
        .undo_checked(&Client::LoopbackHttp, preview.id)
        .unwrap_err();
    assert_eq!(replay.code, ErrorCode::InvalidArgument);
    assert!(f.writes_since(start).is_empty());
}
