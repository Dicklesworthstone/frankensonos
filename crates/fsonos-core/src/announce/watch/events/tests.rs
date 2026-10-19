use super::*;
use crate::playback::Changes;
use std::sync::mpsc;

mod probe;

const CLIP: &str = "http://192.0.2.250:3400/media/event-test.wav";

fn player() -> PlayerId {
    PlayerId("announcement-lead".into())
}

fn changed(uri: Option<&str>, state: Option<TransportState>) -> LiveEvent {
    LiveEvent::Playback {
        player: player(),
        changes: Changes {
            track: uri.map(|uri| (None, Some(uri.into()))),
            transport: state.map(|state| (None, state)),
            ..Changes::default()
        },
    }
}

fn heard(evidence: &mut Evidence, event: LiveEvent) {
    evidence.observe(event, &player(), CLIP).unwrap();
}

fn playing() -> Evidence {
    let mut evidence = Evidence::default();
    heard(
        &mut evidence,
        changed(Some(CLIP), Some(TransportState::Playing)),
    );
    evidence
}

#[test]
fn stopped_requires_this_clips_own_start_and_never_a_cached_transport_state() {
    let mut evidence = Evidence::default();
    for event in [
        changed(None, Some(TransportState::Stopped)),
        changed(Some("test:older"), Some(TransportState::Playing)),
        changed(Some(CLIP), Some(TransportState::Stopped)),
    ] {
        heard(&mut evidence, event);
        assert!(!evidence.began);
        assert!(!evidence.inspect);
    }
    heard(
        &mut evidence,
        changed(None, Some(TransportState::Transitioning)),
    );
    assert!(evidence.began);
    heard(&mut evidence, changed(None, Some(TransportState::Stopped)));
    assert!(evidence.inspect);
}

#[test]
fn a_source_away_and_back_in_the_same_prefix_remains_superseded() {
    let mut evidence = playing();
    let (send, receive) = mpsc::channel();
    send.send(changed(Some("test:newer"), Some(TransportState::Playing)))
        .unwrap();
    send.send(changed(Some(CLIP), Some(TransportState::Playing)))
        .unwrap();
    let result = evidence.drain(&receive, None, &player(), CLIP);
    assert!(matches!(
        result,
        Err(Interrupted::LeaveAlone(why)) if why.contains("superseded")
    ));
    assert!(
        receive.try_recv().is_ok(),
        "never consume past supersession"
    );
}

#[test]
fn pause_after_start_is_preserved_and_an_eof_track_clear_is_verified() {
    let mut evidence = playing();
    assert!(matches!(
        evidence.observe(
            changed(None, Some(TransportState::Paused)),
            &player(),
            CLIP,
        ),
        Err(Interrupted::LeaveAlone(why)) if why.contains("paused")
    ));
    let mut evidence = playing();
    heard(
        &mut evidence,
        LiveEvent::Playback {
            player: player(),
            changes: Changes {
                track: Some((Some(CLIP.into()), None)),
                transport: Some((Some(TransportState::Playing), TransportState::Stopped)),
                ..Changes::default()
            },
        },
    );
    assert!(evidence.inspect, "EOF needs a fresh source/state read");
}

#[test]
fn loss_discards_event_readiness_and_requests_bounded_fresh_observation() {
    for event in [
        LiveEvent::Resync,
        LiveEvent::Health {
            player: player(),
            health: Health::Offline,
        },
    ] {
        let mut evidence = playing();
        heard(&mut evidence, event);
        assert!(!evidence.clip);
        assert!(!evidence.began);
        assert!(evidence.inspect);
        evidence.inspect = false;
        heard(&mut evidence, changed(None, Some(TransportState::Stopped)));
        assert!(!evidence.inspect, "an incomplete suffix cannot finish");
    }
}

#[test]
fn unrelated_rooms_and_rendering_updates_do_not_restart_ownership_checks() {
    let mut evidence = playing();
    evidence.changed = false;
    heard(
        &mut evidence,
        LiveEvent::Playback {
            player: PlayerId("other-room".into()),
            changes: Changes {
                track: Some((None, Some("test:elsewhere".into()))),
                transport: Some((None, TransportState::Stopped)),
                ..Changes::default()
            },
        },
    );
    heard(
        &mut evidence,
        LiveEvent::Playback {
            player: player(),
            changes: Changes {
                volume: Some(17),
                mute: Some(true),
                ..Changes::default()
            },
        },
    );
    assert!(evidence.clip && evidence.began);
    assert!(!evidence.inspect);
    assert!(!evidence.changed);
}

#[test]
fn a_bounded_burst_does_not_skip_a_later_source_change() {
    let mut evidence = playing();
    let (send, receive) = mpsc::channel();
    for _ in 0..EVENT_BURST {
        send.send(LiveEvent::Playback {
            player: PlayerId("other-room".into()),
            changes: Changes::default(),
        })
        .unwrap();
    }
    send.send(changed(Some("test:newer"), None)).unwrap();
    assert!(!evidence.drain(&receive, None, &player(), CLIP).unwrap());
    assert!(matches!(
        evidence.drain(&receive, None, &player(), CLIP),
        Err(Interrupted::LeaveAlone(_))
    ));
}

#[test]
fn a_uri_only_start_uses_its_later_transport_edge_without_borrowing_an_old_source() {
    let stopped = |with_uri: bool| LiveEvent::Playback {
        player: player(),
        changes: Changes {
            track: with_uri.then(|| (None, Some(CLIP.into()))),
            transport: Some((Some(TransportState::Playing), TransportState::Stopped)),
            ..Changes::default()
        },
    };
    let mut old_source = Evidence::default();
    heard(&mut old_source, stopped(true));
    assert!(!old_source.began);
    assert!(!old_source.inspect);

    let mut clip = Evidence::default();
    heard(&mut clip, changed(Some(CLIP), None));
    assert!(!clip.began);
    heard(&mut clip, stopped(false));
    assert!(clip.began);
    assert!(clip.inspect);
}

#[test]
fn fresh_ownership_after_loss_preserves_the_callback_prefix_during_the_probe() {
    let mut evidence = playing();
    heard(&mut evidence, LiveEvent::Resync);
    evidence.observed_source(None);
    assert!(
        !evidence.clip,
        "an unanswered probe cannot regain ownership"
    );

    let (send, receive) = mpsc::channel();
    send.send(changed(Some("test:newer"), Some(TransportState::Playing)))
        .unwrap();
    send.send(changed(Some(CLIP), Some(TransportState::Playing)))
        .unwrap();
    evidence.observed_source(Some(TransportState::Playing));
    assert!(matches!(
        evidence.drain(&receive, None, &player(), CLIP),
        Err(Interrupted::LeaveAlone(why)) if why.contains("superseded")
    ));
    assert!(
        receive.try_recv().is_ok(),
        "the intervening source remains terminal"
    );
}

#[test]
fn an_eof_track_clear_does_not_hide_a_later_source_away_and_back() {
    let mut evidence = playing();
    let (send, receive) = mpsc::channel();
    send.send(LiveEvent::Playback {
        player: player(),
        changes: Changes {
            track: Some((Some(CLIP.into()), None)),
            transport: Some((Some(TransportState::Playing), TransportState::Stopped)),
            ..Changes::default()
        },
    })
    .unwrap();
    send.send(changed(Some("test:newer"), Some(TransportState::Playing)))
        .unwrap();
    send.send(changed(Some(CLIP), Some(TransportState::Playing)))
        .unwrap();
    assert!(matches!(
        evidence.drain(&receive, None, &player(), CLIP),
        Err(Interrupted::LeaveAlone(why)) if why.contains("superseded")
    ));
    assert!(receive.try_recv().is_ok(), "EOF cannot hide supersession");
}
