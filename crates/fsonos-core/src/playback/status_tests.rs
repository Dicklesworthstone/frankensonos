//! Status is distinct from transport state: a stopped renderer may be healthy,
//! failing, or unknown without changing its source or queue occurrence.

use super::*;
use fsonos_proto::control::{MediaInfo, TransportInfo};

fn player() -> PlayerId {
    PlayerId("test-status-player".into())
}

fn notify(body: &str) -> Notify {
    Notify {
        sid: "uuid:test-status".into(),
        seq: 1,
        path: "/AVTransport".into(),
        properties: vec![(
            "LastChange".into(),
            format!("<Event><InstanceID val=\"0\">{body}</InstanceID></Event>"),
        )],
    }
}

fn read(status: &str) -> PlaybackObservation {
    PlaybackObservation {
        transport: TransportInfo {
            state: TransportState::Stopped,
            status: status.into(),
        },
        position: PositionInfo {
            track: 2,
            duration_secs: Some(180),
            position_secs: Some(43),
            uri: "test:same-recording".into(),
            metadata: None,
        },
        media: MediaInfo {
            tracks: 3,
            uri: "x-rincon-queue:test-status-player#0".into(),
            uri_metadata: String::new(),
        },
    }
}

#[test]
fn status_only_events_report_error_recovery_and_clear_without_a_track_change() {
    let now = Instant::now();
    let mut playback = Playback::default();
    playback.apply_observation(&player(), &read("OK"), now);
    for (wire, expected) in [
        (" ERROR_OCCURRED ", Some("ERROR_OCCURRED")),
        (
            " vendor-specific: waiting ",
            Some("vendor-specific: waiting"),
        ),
        (" error_occurred ", Some("error_occurred")),
        (" OK ", Some("OK")),
        (" ", None),
    ] {
        let before = playback.of(&player()).unwrap().clone();
        let changes = playback
            .apply(
                &player(),
                EventSource::AvTransport,
                &notify(&format!("<TransportStatus val=\"{wire}\"/>")),
                now,
            )
            .unwrap();
        assert_eq!(
            changes,
            Changes {
                transport_status: Some((before.transport_status, expected.map(str::to_owned))),
                ..Changes::default()
            }
        );
        assert!(!changes.is_empty(), "status alone wakes subscribers");
        let state = playback.of(&player()).unwrap();
        assert_eq!(state.transport_status.as_deref(), expected);
        assert_eq!(state.transport, before.transport);
        assert_eq!(state.track_uri, before.track_uri);
        assert_eq!(state.media_uri, before.media_uri);
        assert_eq!(state.queue_position, before.queue_position);
        assert_eq!(state.position_at(now), Some(43));
        assert!(state.needs_position_sample());
    }
}

#[test]
fn omitted_event_status_preserves_an_error_but_explicit_blank_clears_it() {
    let now = Instant::now();
    let mut playback = Playback::default();
    let event = |body| notify(&format!("<TransportState val=\"STOPPED\"/>{body}"));
    playback
        .apply(&player(), EventSource::AvTransport, &event(""), now)
        .unwrap();
    assert_eq!(playback.of(&player()).unwrap().transport_status, None);
    let error = event("<TransportStatus val=\"ERROR_OCCURRED\"/>");
    playback
        .apply(&player(), EventSource::AvTransport, &error, now)
        .unwrap();
    for event in [&error, &event("")] {
        let changes = playback
            .apply(&player(), EventSource::AvTransport, event, now)
            .unwrap();
        assert!(changes.is_empty());
        assert_eq!(
            playback.of(&player()).unwrap().transport_status.as_deref(),
            Some("ERROR_OCCURRED")
        );
    }
    let cleared = playback
        .apply(
            &player(),
            EventSource::AvTransport,
            &event("<TransportStatus val=\"\"/>"),
            now,
        )
        .unwrap();
    assert_eq!(
        cleared.transport_status,
        Some((Some("ERROR_OCCURRED".into()), None))
    );
    assert_eq!(playback.of(&player()).unwrap().transport_status, None);
}

#[test]
fn complete_readback_reports_status_and_can_clear_an_old_error() {
    let now = Instant::now();
    let mut playback = Playback::default();
    playback.apply_observation(&player(), &read("OK"), now);
    for (wire, expected) in [
        (" ERROR_OCCURRED ", Some("ERROR_OCCURRED")),
        ("", None),
        (" vendor-specific ", Some("vendor-specific")),
        (" \n ", None),
        (" OK ", Some("OK")),
    ] {
        let before = playback.of(&player()).unwrap().transport_status.clone();
        let changes = playback.apply_observation(&player(), &read(wire), now);
        assert_eq!(
            changes,
            Changes {
                transport_status: Some((before, expected.map(str::to_owned))),
                ..Changes::default()
            }
        );
        assert!(!changes.is_empty());
        assert_eq!(
            playback.of(&player()).unwrap().transport_status.as_deref(),
            expected
        );
        assert!(
            playback
                .apply_observation(&player(), &read(wire), now)
                .is_empty()
        );
    }
}

#[test]
fn malformed_metadata_does_not_publish_or_stamp_a_partial_status_change() {
    let now = Instant::now();
    let mut playback = Playback::default();
    playback.apply_observation(&player(), &read("OK"), now);
    let before = playback.of(&player()).unwrap().clone();
    let stamp = playback.observation_stamp(&player(), EventSource::AvTransport);
    let result = playback.apply(
        &player(),
        EventSource::AvTransport,
        &notify(
            "<TransportStatus val=\"ERROR_OCCURRED\"/>\
             <CurrentTrackMetaData val=\"not-xml\"/>",
        ),
        now,
    );
    assert!(result.is_err());
    assert_eq!(playback.of(&player()), Some(&before));
    assert_eq!(
        playback.observation_stamp(&player(), EventSource::AvTransport),
        stamp
    );
}

#[test]
fn a_status_round_trip_invalidates_an_older_equal_readback() {
    let now = Instant::now();
    let mut playback = Playback::default();
    playback.apply_observation(&player(), &read("OK"), now);
    let before = playback.begin_position_sample(&player()).unwrap();
    let stamp = playback.observation_stamp(&player(), EventSource::AvTransport);
    for status in ["ERROR_OCCURRED", "OK"] {
        playback
            .apply(
                &player(),
                EventSource::AvTransport,
                &notify(&format!("<TransportStatus val=\"{status}\"/>")),
                now,
            )
            .unwrap();
        assert_ne!(
            playback.observation_stamp(&player(), EventSource::AvTransport),
            stamp
        );
        if status == "ERROR_OCCURRED" {
            assert!(
                !playback
                    .of(&player())
                    .unwrap()
                    .same_transport_observation(&before)
            );
        }
    }
    let after = playback.begin_position_sample(&player()).unwrap();
    assert!(after.same_transport_observation(&before));
    assert_ne!(
        playback.observation_stamp(&player(), EventSource::AvTransport),
        stamp
    );
}
