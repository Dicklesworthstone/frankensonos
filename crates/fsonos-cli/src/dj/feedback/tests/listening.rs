//! Listening time is playback time, not time spent paused or buffering.

use super::*;

const STOPPED: Option<TransportState> = Some(TransportState::Stopped);

#[test]
fn a_long_pause_does_not_turn_an_early_skip_into_a_full_listen() {
    let mut listening = Listening::default();
    listening.on_change(0, PLAYING, Some(movement(1, "a", true)));
    assert_eq!(
        listening.on_change(10, Some(TransportState::Paused), None),
        None
    );
    assert_eq!(
        listening.on_change(3600, PLAYING, Some(movement(1, "a", true))),
        None
    );
    assert_eq!(
        listening.on_change(3610, STOPPED, None),
        Some(("a".into(), Signal::EarlySkip))
    );
    assert_eq!(listening.on_change(4000, STOPPED, None), None);
}

#[test]
fn resumed_intervals_accumulate_without_counting_pauses_twice() {
    let mut listening = Listening::default();
    listening.on_change(0, PLAYING, Some(movement(1, "a", true)));
    listening.on_change(60, Some(TransportState::Paused), None);
    listening.on_change(600, Some(TransportState::Paused), None);
    listening.on_change(1000, PLAYING, Some(movement(1, "a", true)));
    listening.on_change(1060, Some(TransportState::Paused), None);
    listening.on_change(2000, PLAYING, Some(movement(1, "a", true)));
    assert_eq!(
        listening.on_change(2060, STOPPED, None),
        Some(("a".into(), Signal::FullListen))
    );
}

#[test]
fn buffering_and_unknown_transport_suspend_the_clock() {
    for transport in [Some(TransportState::Transitioning), None] {
        let mut listening = Listening::default();
        listening.on_change(0, PLAYING, Some(movement(1, "a", true)));
        listening.on_change(10, transport, None);
        listening.on_change(1000, transport, None);
        listening.on_change(2000, PLAYING, Some(movement(1, "a", true)));
        assert_eq!(
            listening.on_change(2010, STOPPED, None),
            Some(("a".into(), Signal::EarlySkip))
        );
    }
}

#[test]
fn unknown_and_zero_lengths_cannot_prove_a_full_listen() {
    for duration_secs in [None, Some(0)] {
        let mut listening = Listening::default();
        listening.on_change(
            0,
            PLAYING,
            Some(Movement {
                duration_secs,
                ..movement(1, "a", true)
            }),
        );
        assert_eq!(listening.on_change(600, STOPPED, None), None);
    }
}

#[test]
fn late_duration_metadata_does_not_restart_the_clock() {
    let mut listening = Listening::default();
    listening.on_change(
        0,
        PLAYING,
        Some(Movement {
            duration_secs: None,
            ..movement(1, "a", true)
        }),
    );
    listening.on_change(90, PLAYING, Some(movement(1, "a", true)));
    assert_eq!(
        listening.on_change(180, STOPPED, None),
        Some(("a".into(), Signal::FullListen))
    );
}

#[test]
fn short_tracks_can_complete_and_ninety_percent_is_the_positive_threshold() {
    for (duration_secs, heard, expected) in [
        (10, 10, Some(Signal::FullListen)),
        (180, 161, None),
        (180, 162, Some(Signal::FullListen)),
        (180, 29, Some(Signal::EarlySkip)),
        (180, 30, None),
    ] {
        let mut listening = Listening::default();
        listening.on_change(
            0,
            PLAYING,
            Some(Movement {
                duration_secs: Some(duration_secs),
                ..movement(1, "a", true)
            }),
        );
        assert_eq!(
            listening.on_change(heard, STOPPED, None),
            expected.map(|signal| ("a".into(), signal))
        );
    }
}

#[test]
fn stale_observations_and_extreme_times_do_not_overflow_or_double_count() {
    let mut listening = Listening::default();
    listening.on_change(100, PLAYING, Some(movement(1, "a", true)));
    listening.on_change(110, PLAYING, Some(movement(1, "a", true)));
    assert_eq!(listening.on_change(105, STOPPED, None), None);
    assert_eq!(
        listening.on_change(120, STOPPED, None),
        Some(("a".into(), Signal::EarlySkip))
    );
    listening.on_change(i64::MAX, PLAYING, Some(movement(2, "b", true)));
    assert_eq!(
        listening.on_change(i64::MAX, STOPPED, None),
        Some(("b".into(), Signal::EarlySkip))
    );
    let mut extreme = Listening::default();
    extreme.on_change(i64::MIN, PLAYING, Some(movement(1, "a", true)));
    assert_eq!(
        extreme.on_change(i64::MAX, STOPPED, None),
        Some(("a".into(), Signal::FullListen))
    );
}
