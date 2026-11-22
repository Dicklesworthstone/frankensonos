use super::*;
use crate::ErrorCode;
use crate::surface::scenes::{SceneDefinition, source_of};
use serde_json::{Value, json};

const TRACK: &str = "spotify:track:0000000000000000000001";
const EPISODE: &str = "spotify:episode:0000000000000000000002";

fn selected() -> Value {
    json!({
        "kind": "tracks",
        "tracks": [
            {"source_uri": "https://open.spotify.com/track/0000000000000000000001?si=saved", "title": "  Opening  "},
            {"source_uri": EPISODE, "title": "Interlude"},
            {"source_uri": TRACK, "title": "Reprise"}
        ]
    })
}

#[test]
fn authoring_normalizes_identities_and_titles_without_losing_occurrences() {
    let dto: SceneSourceDto = serde_json::from_value(selected()).unwrap();
    let source = source_of(&dto).unwrap();
    let SceneSource::Tracks { tracks } = &source else {
        panic!("a tracks source must retain its selection");
    };
    assert_eq!(
        tracks
            .iter()
            .map(|track| track.source_uri.as_str())
            .collect::<Vec<_>>(),
        [TRACK, EPISODE, TRACK]
    );
    assert_eq!(
        tracks
            .iter()
            .map(|track| track.title.as_deref())
            .collect::<Vec<_>>(),
        [Some("Opening"), Some("Interlude"), Some("Reprise")]
    );
    let saved = SceneSourceDto::from(&source);
    assert_eq!(source_of(&saved).unwrap(), source);
    let wire = serde_json::to_value(&saved).unwrap();
    assert_eq!(wire["tracks"][0]["source_uri"], TRACK);
    assert_eq!(wire["tracks"][2]["source_uri"], TRACK);
    assert_eq!(wire.as_object().unwrap().len(), 2);
    assert_eq!(
        serde_json::from_value::<SceneSourceDto>(wire).unwrap(),
        saved
    );
    assert_eq!(
        saved.tracks_text().unwrap(),
        format!(
            "3 selected tracks in order: 1. Opening [{TRACK}]; 2. Interlude [{EPISODE}]; 3. Reprise [{TRACK}] (replaces the queue)"
        )
    );
}

#[test]
fn missing_or_empty_tracks_have_no_display_and_invalid_selections_are_rejected() {
    for tracks in [
        Value::Null,
        json!([]),
        json!(vec![json!({"source_uri": TRACK}); 11]),
    ] {
        let dto: SceneSourceDto =
            serde_json::from_value(json!({"kind": "tracks", "tracks": tracks})).unwrap();
        assert_eq!(
            source_of(&dto).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        if dto.tracks.as_ref().is_none_or(Vec::is_empty) {
            assert!(dto.tracks_text().is_none());
        }
    }
    for uri in [
        "spotify:album:0000000000000000000001",
        "spotify:artist:0000000000000000000001",
        "spotify:show:0000000000000000000001",
        "spotify:track:too-short",
        "spotify:episode:000000000000000000000!",
        "https://example.invalid/song.mp3",
    ] {
        let definition: SceneDefinition = serde_json::from_value(json!({
            "groups": [{"coordinator": "Kitchen", "source": {"kind": "tracks", "tracks": [{"source_uri": uri}]}}]
        })).unwrap();
        assert_eq!(
            definition.validate_sources().unwrap_err().code,
            ErrorCode::InvalidArgument,
            "{uri}"
        );
    }
}

#[test]
fn authored_tracks_cannot_smuggle_another_source_or_renderer_metadata() {
    for (field, value) in [
        ("name", json!("Album")),
        ("uri", json!(TRACK)),
        ("mood", json!("calm")),
        ("direction", json!({})),
        (
            "queue",
            json!({"coordinator":"RINCON_SYNTHETIC", "update_id":1, "track":1, "position_secs":0}),
        ),
        (
            "playlist",
            json!({"household":"synthetic", "id":"SQ:1", "title":"Playlist"}),
        ),
    ] {
        let mut wire = selected();
        wire[field] = value;
        let dto: SceneSourceDto = serde_json::from_value(wire).unwrap();
        assert_eq!(
            source_of(&dto).unwrap_err().code,
            ErrorCode::InvalidArgument,
            "{field}"
        );
    }
    for kind in ["keep", "uri", "favorite", "dj", "sonos_playlist"] {
        let mut wire = selected();
        wire["kind"] = json!(kind);
        let definition: SceneDefinition = serde_json::from_value(json!({
            "groups": [{"coordinator":"Kitchen", "source":wire}]
        }))
        .unwrap();
        assert_eq!(
            definition.validate_sources().unwrap_err().code,
            ErrorCode::InvalidArgument,
            "{kind}"
        );
    }
    let mut wire = selected();
    wire["tracks"][0]["metadata"] = json!("untrusted renderer metadata");
    assert!(serde_json::from_value::<SceneSourceDto>(wire).is_err());
}

#[test]
fn optional_titles_are_normalized_and_control_characters_are_refused() {
    let dto: SceneSourceDto = serde_json::from_value(json!({
        "kind": "tracks", "tracks": [{"source_uri": TRACK, "title":"  "}, {"source_uri": EPISODE}]
    }))
    .unwrap();
    let saved = SceneSourceDto::from(&source_of(&dto).unwrap());
    assert!(
        saved
            .tracks
            .as_ref()
            .unwrap()
            .iter()
            .all(|track| track.title.is_none())
    );
    assert!(
        saved
            .tracks_text()
            .unwrap()
            .contains(&format!("1. {TRACK} [{TRACK}]"))
    );
    let mut bad = selected();
    bad["tracks"][0]["title"] = json!("hidden\u{0001}control");
    let dto: SceneSourceDto = serde_json::from_value(bad).unwrap();
    assert_eq!(
        source_of(&dto).unwrap_err().code,
        ErrorCode::InvalidArgument
    );
}
