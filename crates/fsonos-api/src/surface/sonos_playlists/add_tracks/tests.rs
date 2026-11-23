use super::*;
use serde_json::json;

const TRACK: &str = "spotify:track:0000000000000000000001";
const EPISODE: &str = "spotify:episode:0000000000000000000002";

fn request() -> AddTracksToSonosPlaylistRequest {
    AddTracksToSonosPlaylistRequest {
        zone: " Studio@S2 ".into(),
        playlist: " SQ:7 ".into(),
        expected_revision: "saved:observed".into(),
        tracks: vec![QueueTrackInput {
            source_uri: TRACK.into(),
            title: Some("  First & last  ".into()),
        }],
    }
}

#[test]
fn links_normalize_and_keep_mixed_occurrences_and_saved_revision() {
    let mut req = request();
    req.tracks.push(QueueTrackInput {
        source_uri: "https://open.spotify.com/episode/0000000000000000000002?si=fixture".into(),
        title: None,
    });
    req.tracks.push(req.tracks[0].clone());
    // Saved contents have their own 4096-byte token bound, not the 1024-byte
    // playing-queue token bound used by ordered playback.
    req.expected_revision = "x".repeat(4096);
    let normalized = req.normalized().unwrap();
    assert_eq!(normalized.zone, "Studio@S2");
    assert_eq!(normalized.playlist, "SQ:7");
    assert_eq!(normalized.expected_revision, req.expected_revision);
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
}

#[test]
fn a_saved_destination_requires_an_exact_id_and_bounded_revision() {
    for playlist in ["Evening", "7", "SQ:", "SQ:7/2", "FV:2/7", "SQ:-1"] {
        let mut req = request();
        req.playlist = playlist.into();
        assert!(req.validate().is_err(), "{playlist}");
    }
    for revision in ["".into(), " ".into(), "a\nb".into(), "x".repeat(4097)] {
        let mut req = request();
        req.expected_revision = revision;
        assert!(req.validate().is_err());
    }
    let mut req = request();
    req.zone = " ".into();
    assert!(req.validate().is_err());
}

#[test]
fn selection_is_bounded_and_never_expands_containers_or_raw_renderer_metadata() {
    for count in 0..=11 {
        let mut req = request();
        req.tracks = vec![req.tracks[0].clone(); count];
        assert_eq!(req.validate().is_ok(), (1..=10).contains(&count));
    }
    for source in [
        "spotify:album:0000000000000000000001",
        "spotify:playlist:0000000000000000000001",
        "spotify:artist:0000000000000000000001",
        "spotify:show:0000000000000000000001",
        "spotify:track:short",
        "https://example.invalid/music.mp3",
        "x-sonos-spotify:spotify%3atrack%3a0000000000000000000001?sid=9",
    ] {
        let mut req = request();
        req.tracks.push(QueueTrackInput {
            source_uri: source.into(),
            title: None,
        });
        assert!(req.validate().is_err(), "{source}");
    }
}

#[test]
fn wire_requires_the_selection_and_saved_revision_and_refuses_extra_fields() {
    let body = json!({
        "zone": "Studio@S2", "playlist": "SQ:7", "expected_revision": "saved:observed",
        "tracks": [{"source_uri": TRACK}]
    });
    let parsed: AddTracksToSonosPlaylistRequest = serde_json::from_value(body.clone()).unwrap();
    assert!(parsed.tracks[0].title.is_none());
    parsed.validate().unwrap();
    for field in ["zone", "playlist", "expected_revision", "tracks"] {
        let mut missing = body.clone();
        missing.as_object_mut().unwrap().remove(field);
        assert!(serde_json::from_value::<AddTracksToSonosPlaylistRequest>(missing).is_err());
    }
    for field in ["metadata", "force", "host", "next", "source_revision"] {
        let mut extra = body.clone();
        extra[field] = json!("untrusted");
        assert!(serde_json::from_value::<AddTracksToSonosPlaylistRequest>(extra).is_err());
    }
    for field in ["uri", "metadata", "sid", "sn"] {
        let mut extra = body.clone();
        extra["tracks"][0][field] = json!("untrusted");
        assert!(serde_json::from_value::<AddTracksToSonosPlaylistRequest>(extra).is_err());
    }
}

#[test]
fn confirmed_additions_stay_successful_when_action_logging_fails() {
    let result = AddedTracksToSonosPlaylistDto {
        done: "added 3 tracks to Evening".into(),
        zone: "Studio".into(),
        playlist: SonosPlaylistDto {
            id: "SQ:7".into(),
            title: "Evening".into(),
            art_uri: None,
        },
        revision: "saved:new-revision".into(),
        previous_total: 4,
        total: 7,
        added: 3,
        source_uris: vec![TRACK.into(), EPISODE.into(), TRACK.into()],
        titles: vec!["First".into(), "Episode".into(), "First".into()],
        logged: false,
    };
    assert!(result.text().contains("Revision: saved:new-revision"));
    assert!(result.text().contains("do not repeat this operation"));
    assert!(
        !AddedTracksToSonosPlaylistDto {
            logged: true,
            ..result
        }
        .text()
        .contains("could not be written")
    );
}
