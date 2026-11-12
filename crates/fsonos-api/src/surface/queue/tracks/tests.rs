use super::*;
use serde_json::json;

const TRACK: &str = "spotify:track:0000000000000000000001";
const EPISODE: &str = "spotify:episode:0000000000000000000002";

fn request() -> QueueTracksRequest {
    QueueTracksRequest {
        zone: " Studio@S2 ".into(),
        tracks: vec![QueueTrackInput {
            source_uri: TRACK.into(),
            title: Some("  First & last  ".into()),
        }],
        next: false,
        expected_revision: None,
    }
}

#[test]
fn links_normalize_without_deduplicating_or_reordering_selected_occurrences() {
    let mut req = request();
    req.tracks.push(QueueTrackInput {
        source_uri: "https://open.spotify.com/episode/0000000000000000000002?si=fixture".into(),
        title: None,
    });
    req.tracks.push(req.tracks[0].clone());
    req.next = true;
    req.expected_revision = Some("queue-token".into());
    let normalized = req.normalized().unwrap();
    assert_eq!(normalized.zone, "Studio@S2");
    assert_eq!(
        normalized
            .tracks
            .iter()
            .map(|track| track.source_uri.as_str())
            .collect::<Vec<_>>(),
        [TRACK, EPISODE, TRACK]
    );
    assert_eq!(normalized.tracks[0].title.as_deref(), Some("First & last"));
    assert_eq!(normalized.tracks[0], normalized.tracks[2]);
    assert!(normalized.next);
    assert_eq!(normalized.expected_revision, req.expected_revision);
}

#[test]
fn bounded_selections_accept_each_size_and_refuse_empty_or_oversized_inputs() {
    for count in 0..=MAX_QUEUE_TRACKS + 1 {
        let mut req = request();
        req.tracks = vec![req.tracks[0].clone(); count];
        assert_eq!(
            req.validate().is_ok(),
            (1..=MAX_QUEUE_TRACKS).contains(&count)
        );
    }
}

#[test]
fn invalid_tracks_containers_and_revision_tokens_are_rejected_as_a_whole() {
    for source in [
        "spotify:album:0000000000000000000001",
        "spotify:playlist:0000000000000000000001",
        "spotify:artist:0000000000000000000001",
        "spotify:show:0000000000000000000001",
        "spotify:track:short",
        "https://example.invalid/music.mp3",
    ] {
        let mut req = request();
        req.tracks.push(QueueTrackInput {
            source_uri: source.into(),
            title: None,
        });
        assert!(req.validate().is_err(), "{source}");
    }
    for revision in [" ".into(), "x".repeat(1025)] {
        let mut req = request();
        req.expected_revision = Some(revision);
        assert!(req.validate().is_err());
    }
    let mut req = request();
    req.zone = " ".into();
    assert!(req.validate().is_err());
}

#[test]
fn wire_defaults_to_append_and_refuses_unknown_fields_or_missing_selection() {
    let req: QueueTracksRequest = serde_json::from_value(json!({
        "zone": "Studio", "tracks": [{"source_uri": TRACK}]
    }))
    .unwrap();
    assert!(!req.next);
    assert!(req.expected_revision.is_none());
    assert!(req.tracks[0].title.is_none());
    req.validate().unwrap();
    for body in [
        json!({"zone": "Studio"}),
        json!({"zone": "Studio", "tracks": [{"source_uri": TRACK}], "client": "cli"}),
        json!({"zone": "Studio", "tracks": [{"source_uri": TRACK, "metadata": "raw DIDL"}]}),
        json!({"zone": "Studio", "tracks": [{"title": "No identity"}]}),
        json!({"zone": "Studio", "tracks": [{"source_uri": TRACK}], "next": "yes"}),
    ] {
        assert!(serde_json::from_value::<QueueTracksRequest>(body).is_err());
    }
}
