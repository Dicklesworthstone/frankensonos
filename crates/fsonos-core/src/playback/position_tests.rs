//! Transport notifications invalidate timing without fabricating a seek.

use super::*;
use std::time::Duration;

fn id() -> PlayerId {
    PlayerId("test-clock".into())
}

fn event(body: &str) -> Notify {
    Notify {
        sid: "uuid:clock".into(),
        seq: 1,
        path: "/AVTransport".into(),
        properties: vec![(
            "LastChange".into(),
            format!("<Event><InstanceID val=\"0\">{body}</InstanceID></Event>"),
        )],
    }
}

fn sample(playback: &mut Playback, secs: Option<u32>, now: Instant) {
    playback.apply_position(
        &id(),
        &PositionInfo {
            track: 1,
            duration_secs: Some(180),
            position_secs: secs,
            uri: "test:clock".into(),
            metadata: None,
        },
        now,
    );
}

const PLAYING: &str = "<TransportState val=\"PLAYING\"/><CurrentTrackURI val=\"test:clock\"/><CurrentTrack val=\"1\"/>";

#[test]
fn first_transport_event_requests_a_clock_without_guessing_zero() {
    let now = Instant::now();
    let mut playback = Playback::default();
    playback
        .apply(&id(), EventSource::AvTransport, &event(PLAYING), now)
        .unwrap();
    let state = playback.of(&id()).unwrap();
    assert!(state.needs_position_sample());
    assert_eq!(state.position_at(now), None);
    sample(&mut playback, Some(73), now);
    let state = playback.of(&id()).unwrap();
    assert!(!state.needs_position_sample());
    assert_eq!(state.position_at(now + Duration::from_secs(2)), Some(75));
}

#[test]
fn an_identical_transport_event_can_be_a_same_track_seek() {
    let now = Instant::now();
    let mut playback = Playback::default();
    playback
        .apply(&id(), EventSource::AvTransport, &event(PLAYING), now)
        .unwrap();
    sample(&mut playback, Some(73), now);
    let changes = playback
        .apply(&id(), EventSource::AvTransport, &event(PLAYING), now)
        .unwrap();
    assert!(changes.is_empty(), "no invented track transition");
    assert!(playback.of(&id()).unwrap().needs_position_sample());
    assert_eq!(playback.of(&id()).unwrap().position_at(now), Some(73));
    sample(&mut playback, Some(17), now);
    assert_eq!(playback.of(&id()).unwrap().position_at(now), Some(17));
    assert!(!playback.of(&id()).unwrap().needs_position_sample());
}

#[test]
fn volume_mode_and_malformed_events_do_not_request_a_clock() {
    let now = Instant::now();
    let mut playback = Playback::default();
    playback
        .apply(&id(), EventSource::AvTransport, &event(PLAYING), now)
        .unwrap();
    sample(&mut playback, Some(73), now);
    playback
        .apply(
            &id(),
            EventSource::RenderingControl,
            &event("<Volume channel=\"Master\" val=\"24\"/>"),
            now,
        )
        .unwrap();
    playback
        .apply(
            &id(),
            EventSource::AvTransport,
            &event("<CurrentPlayMode val=\"NORMAL\"/>"),
            now,
        )
        .unwrap();
    let before = playback.of(&id()).unwrap().clone();
    assert!(!before.needs_position_sample());
    assert!(
        playback
            .apply(
                &id(),
                EventSource::AvTransport,
                &event(&format!("{PLAYING}<CurrentTrackMetaData val=\"not-xml\"/>")),
                now,
            )
            .is_err()
    );
    assert_eq!(playback.of(&id()), Some(&before));
}

#[test]
fn unknown_timing_and_cleared_sources_do_not_request_reads_forever() {
    let now = Instant::now();
    let mut playback = Playback::default();
    playback
        .apply(&id(), EventSource::AvTransport, &event(PLAYING), now)
        .unwrap();
    sample(&mut playback, None, now);
    assert_eq!(playback.of(&id()).unwrap().position_at(now), None);
    assert!(!playback.of(&id()).unwrap().needs_position_sample());
    playback
        .apply(&id(), EventSource::AvTransport, &event(PLAYING), now)
        .unwrap();
    assert!(playback.of(&id()).unwrap().needs_position_sample());
    playback
        .apply(
            &id(),
            EventSource::AvTransport,
            &event("<CurrentTrackURI val=\"\"/><TransportState val=\"STOPPED\"/>"),
            now,
        )
        .unwrap();
    assert!(!playback.of(&id()).unwrap().needs_position_sample());
}
