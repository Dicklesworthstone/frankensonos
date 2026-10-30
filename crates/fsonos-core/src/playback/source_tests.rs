//! Source identity stays distinct from the current track's identity.

use super::*;
use fsonos_proto::control::{MediaInfo, PositionInfo, TransportInfo};

const PLAYER: &str = "test-source-player";
const TRACK: &str = "x-sonos-spotify:spotify%3atrack%3aTestTrack?sid=9";

fn notice(source: &str) -> Notify {
    Notify {
        sid: "uuid:test-source".into(),
        seq: 1,
        path: "/AVTransport".into(),
        properties: vec![(
            "LastChange".into(),
            format!(
                "<Event><InstanceID val=\"0\"><TransportState val=\"PLAYING\"/><CurrentTrackURI val=\"{TRACK}\"/><CurrentTrack val=\"1\"/><AVTransportURI val=\"{source}\"/></InstanceID></Event>"
            ),
        )],
    }
}

fn read(source: &str, position_secs: Option<u32>) -> PlaybackObservation {
    PlaybackObservation {
        transport: TransportInfo {
            state: TransportState::Playing,
            status: "OK".into(),
        },
        position: PositionInfo {
            track: 1,
            duration_secs: Some(180),
            position_secs,
            uri: TRACK.into(),
            metadata: None,
        },
        media: MediaInfo {
            tracks: 1,
            uri: source.into(),
            uri_metadata: String::new(),
        },
    }
}

#[test]
fn source_changes_wake_consumers_without_inventing_a_track_transition() {
    let player = PlayerId(PLAYER.into());
    let queue = format!("x-rincon-queue:{PLAYER}#0");
    let temporary = "http://example.invalid/announcement.wav";
    let now = Instant::now();
    let mut playback = Playback::default();
    playback
        .apply(&player, EventSource::AvTransport, &notice(&queue), now)
        .unwrap();
    assert!(playback.of(&player).unwrap().is_queue_of(&player));
    for (from, to) in [(&*queue, temporary), (temporary, &*queue)] {
        let before = playback.of(&player).unwrap().clone();
        let changes = playback
            .apply(&player, EventSource::AvTransport, &notice(to), now)
            .unwrap();
        assert_eq!(
            changes.media_uri,
            Some((Some(from.into()), Some(to.into())))
        );
        assert_eq!(changes.track, None);
        assert_eq!(changes.transport, None);
        assert!(!changes.is_empty());
        let after = playback.of(&player).unwrap();
        assert!(
            !after.same_transport_observation(&before),
            "an earlier read cannot overwrite this source"
        );
        assert!(after.needs_position_sample());
        assert_eq!(after.is_queue_of(&player), to == queue);
        assert!(
            playback
                .apply(&player, EventSource::AvTransport, &notice(to), now)
                .unwrap()
                .is_empty()
        );
    }
}

#[test]
fn an_unknown_or_cleared_source_does_not_reuse_an_earlier_queue() {
    let player = PlayerId(PLAYER.into());
    let queue = format!("x-rincon-queue:{PLAYER}#0");
    let now = Instant::now();
    let mut playback = Playback::default();
    assert!(!PlayerPlayback::default().is_queue_of(&player));
    for source in [
        "",
        "NOT_IMPLEMENTED",
        "x-rincon-queue:other-player#0",
        "x-rincon-queue:test-source-player#1",
        "x-sonos-vli:test-source-player:2,spotify:test-session",
    ] {
        playback
            .apply(&player, EventSource::AvTransport, &notice(&queue), now)
            .unwrap();
        let changes = playback
            .apply(&player, EventSource::AvTransport, &notice(source), now)
            .unwrap();
        assert!(changes.media_uri.is_some());
        assert!(
            !playback.of(&player).unwrap().is_queue_of(&player),
            "{source}"
        );
    }
}

#[test]
fn read_back_recovers_source_only_changes_without_reusing_the_old_clock() {
    let player = PlayerId(PLAYER.into());
    let queue = format!("x-rincon-queue:{PLAYER}#0");
    let now = Instant::now();
    let mut playback = Playback::default();
    playback.apply_observation(&player, &read(&queue, Some(120)), now);
    let before = playback.of(&player).unwrap().clone();
    let temporary = "x-sonos-vli:test-source-player:2,spotify:test-session";
    let changed = playback.apply_observation(&player, &read(temporary, None), now);
    assert_eq!(changed.track, None);
    assert_eq!(changed.transport, None);
    assert_eq!(
        changed.media_uri,
        Some((Some(queue.clone()), Some(temporary.into())))
    );
    let state = playback.of(&player).unwrap();
    assert!(!state.same_transport_observation(&before));
    assert!(!state.is_queue_of(&player));
    assert_eq!(state.position_at(now), None);
    let returned = playback.apply_observation(&player, &read(&queue, Some(125)), now);
    assert!(returned.media_uri.is_some());
    assert!(playback.of(&player).unwrap().is_queue_of(&player));
    assert_eq!(playback.of(&player).unwrap().position_at(now), Some(125));
}

#[test]
fn a_source_only_event_can_request_the_missing_track_readback() {
    let player = PlayerId(PLAYER.into());
    let mut playback = Playback::default();
    let mut event = notice("http://example.invalid/clip.wav");
    event.properties[0].1 = "<Event><InstanceID val=\"0\"><TransportState val=\"PLAYING\"/><AVTransportURI val=\"http://example.invalid/clip.wav\"/></InstanceID></Event>".into();
    let changed = playback
        .apply(&player, EventSource::AvTransport, &event, Instant::now())
        .unwrap();
    assert!(changed.media_uri.is_some());
    let state = playback.of(&player).unwrap();
    assert!(state.track_uri.is_none());
    assert!(state.needs_position_sample());
}
