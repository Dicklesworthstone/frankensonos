//! OpenAPI choices must be the values the same HTTP routes actually accept
//! and return. The pinned derive does not honor serde's enum representation.

use super::*;
use fastapi::fastapi_openapi::JsonSchema;
use fsonos_api::surface::announce::AnnouncementJobPhase;
use fsonos_api::surface::dj_control::ReviewedDjAction;
use fsonos_api::surface::dj_sync::{FollowedArtistsPhase, SavedLibraryIntakePhase};
use fsonos_api::surface::favorite_management::FavoriteOperation;
use fsonos_api::surface::sonos_playlists::{
    SonosPlaylistEditOperation, SonosPlaylistOperation, SonosPlaylistTrackMode,
};
use fsonos_api::surface::source_control::{ReviewedSource, ReviewedSourceMode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::fmt::Debug;

fn enum_parity<T: JsonSchema + Serialize + DeserializeOwned + PartialEq + Debug>(values: &[T]) {
    let names: Vec<_> = values
        .iter()
        .map(|value| {
            let name = serde_json::to_value(value).unwrap();
            assert!(name.is_string());
            assert_eq!(&serde_json::from_value::<T>(name.clone()).unwrap(), value);
            name
        })
        .collect();
    let schema = serde_json::to_value(T::schema()).unwrap();
    assert_eq!(schema["type"], "string");
    assert_eq!(schema["enum"], json!(names), "{:?}", T::schema_name());
}

#[test]
fn public_enum_schemas_match_every_serialized_wire_value() {
    enum_parity(&[
        ReviewedSourceMode::Play,
        ReviewedSourceMode::Next,
        ReviewedSourceMode::Append,
    ]);
    enum_parity(&[
        ReviewedDjAction::Start,
        ReviewedDjAction::Steer,
        ReviewedDjAction::Skip,
        ReviewedDjAction::Stop,
    ]);
    enum_parity(&[
        SonosPlaylistTrackMode::Play,
        SonosPlaylistTrackMode::Next,
        SonosPlaylistTrackMode::Append,
    ]);
    enum_parity(&[
        SonosPlaylistOperation::Rename,
        SonosPlaylistOperation::Remove,
    ]);
    enum_parity(&[
        SonosPlaylistEditOperation::RemoveTracks,
        SonosPlaylistEditOperation::MoveTracks,
    ]);
    enum_parity(&[
        FavoriteOperation::Save,
        FavoriteOperation::Rename,
        FavoriteOperation::Remove,
    ]);
    enum_parity(&[
        AnnouncementJobPhase::Queued,
        AnnouncementJobPhase::Preparing,
        AnnouncementJobPhase::Playing,
        AnnouncementJobPhase::Restoring,
        AnnouncementJobPhase::Completed,
        AnnouncementJobPhase::Cancelled,
        AnnouncementJobPhase::Failed,
    ]);
    enum_parity(&[
        SavedLibraryIntakePhase::Reading,
        SavedLibraryIntakePhase::Verifying,
        SavedLibraryIntakePhase::Ready,
    ]);
    enum_parity(&[
        FollowedArtistsPhase::Reading,
        FollowedArtistsPhase::Verifying,
        FollowedArtistsPhase::Ready,
    ]);
    // Existing manual schemas stay under the same wire-contract test.
    enum_parity(&fsonos_api::ErrorCode::ALL);
    enum_parity(&fsonos_api::NoteCode::ALL);
    enum_parity(&[
        fsonos_api::source::CatalogKind::Track,
        fsonos_api::source::CatalogKind::Album,
        fsonos_api::source::CatalogKind::Artist,
        fsonos_api::source::CatalogKind::Playlist,
        fsonos_api::source::CatalogKind::Show,
        fsonos_api::source::CatalogKind::Episode,
    ]);
}

fn resolve<'a>(doc: &'a Value, schema: &'a Value) -> &'a Value {
    if let Some(reference) = schema["$ref"].as_str() {
        return resolve(
            doc,
            doc.pointer(reference.strip_prefix('#').unwrap()).unwrap(),
        );
    }
    // Option<Object> is emitted as anyOf [object, null].
    if let Some(branches) = schema["anyOf"].as_array() {
        let nonnull: Vec<_> = branches
            .iter()
            .filter(|value| value["type"] != "null")
            .collect();
        assert_eq!(nonnull.len(), 1, "expected one nullable schema: {schema}");
        return resolve(doc, nonnull[0]);
    }
    schema
}

fn body<'a>(doc: &'a Value, path: &str) -> &'a Value {
    resolve(
        doc,
        &doc["paths"][path]["post"]["requestBody"]["content"]["application/json"]["schema"],
    )
}

fn reply<'a>(doc: &'a Value, path: &str, method: &str) -> &'a Value {
    resolve(
        doc,
        &doc["paths"][path][method]["responses"]["200"]["content"]["application/json"]["schema"],
    )
}

fn field<'a>(doc: &'a Value, schema: &'a Value, name: &str) -> &'a Value {
    resolve(doc, &resolve(doc, schema)["properties"][name])
}

fn choices<'a>(doc: &'a Value, path: &str, name: &str) -> &'a [Value] {
    field(doc, body(doc, path), name)["enum"]
        .as_array()
        .unwrap()
}

#[test]
fn live_openapi_preserves_request_choices_and_track_receipts() {
    let api = Api::start("", &Client::Unknown);
    let (status, doc) = api.get("/openapi.json");
    assert_eq!(status, 200);
    let modes = [json!("play"), json!("next"), json!("append")];
    for path in [
        "/sources/control",
        "/sonos/playlists/track",
        "/sonos/playlists/tracks",
    ] {
        assert_eq!(choices(&doc, path, "mode"), &modes);
    }
    for path in ["/sonos/playlists/track", "/sonos/playlists/tracks"] {
        assert_eq!(
            field(&doc, reply(&doc, path, "post"), "mode")["enum"],
            json!(modes),
        );
    }
    assert_eq!(
        choices(&doc, "/dj/control", "action"),
        &[json!("start"), json!("steer"), json!("skip"), json!("stop")],
    );
    assert!(api.actions().is_empty());
}

#[test]
fn live_openapi_preserves_catalog_job_and_sync_response_names() {
    let api = Api::start("", &Client::Unknown);
    let (status, doc) = api.get("/openapi.json");
    assert_eq!(status, 200);
    for (paths, expected) in [
        (
            &["/favorites/save", "/favorites/rename", "/favorites/remove"][..],
            json!(["save", "rename", "remove"]),
        ),
        (
            &["/sonos/playlists/rename", "/sonos/playlists/remove"][..],
            json!(["rename", "remove"]),
        ),
        (
            &[
                "/sonos/playlists/remove-tracks",
                "/sonos/playlists/move-tracks",
            ][..],
            json!(["remove_tracks", "move_tracks"]),
        ),
    ] {
        for path in paths {
            assert_eq!(
                field(&doc, reply(&doc, path, "post"), "operation")["enum"],
                expected,
                "{path}",
            );
        }
    }
    let phases = json!([
        "queued",
        "preparing",
        "playing",
        "restoring",
        "completed",
        "cancelled",
        "failed"
    ]);
    for (path, method) in [
        ("/announcements", "post"),
        ("/announcements/{id}", "get"),
        ("/announcements/{id}/cancel", "post"),
    ] {
        assert_eq!(
            field(&doc, reply(&doc, path, method), "phase")["enum"],
            phases
        );
    }
    let listing = resolve(&doc, &reply(&doc, "/announcements", "get")["items"]);
    assert_eq!(field(&doc, listing, "phase")["enum"], phases);
    for name in ["library_intake", "followed_artists"] {
        let progress = field(&doc, reply(&doc, "/dj/sync", "get"), name);
        assert_eq!(
            field(&doc, progress, "phase")["enum"],
            json!(["reading", "verifying", "ready"]),
            "{name}: {progress}",
        );
    }
    assert!(api.actions().is_empty());
}

fn sources() -> [Value; 4] {
    [
        json!({"kind":"uri", "source_uri":"spotify:track:0123456789abcdefghijkl"}),
        json!({"kind":"tracks", "tracks":[{"source_uri":"spotify:track:0123456789abcdefghijkl"}]}),
        json!({"kind":"favorite", "favorite":"FV:2/1"}),
        json!({"kind":"playlist", "playlist":"SQ:1"}),
    ]
}

#[test]
fn published_source_alternatives_are_exact_exclusive_wire_objects() {
    let api = Api::start("", &Client::Unknown);
    let (_, doc) = api.get("/openapi.json");
    let source = field(&doc, body(&doc, "/sources/control"), "source");
    let branches = source["oneOf"].as_array().unwrap();
    assert_eq!(branches.len(), 4);
    for (sample, required) in sources().into_iter().zip([
        ["kind", "source_uri"],
        ["kind", "tracks"],
        ["kind", "favorite"],
        ["kind", "playlist"],
    ]) {
        let matching: Vec<_> = branches
            .iter()
            .filter(|branch| branch["properties"]["kind"]["enum"] == json!([sample["kind"]]))
            .collect();
        assert_eq!(matching.len(), 1, "{sample}: {source}");
        let branch = matching[0];
        assert_eq!(branch["required"], json!(required));
        assert_eq!(branch["additionalProperties"], false);
        assert!(branch["properties"].get("type").is_none());
        assert_eq!(
            branch["properties"].as_object().unwrap().len(),
            if sample["kind"] == "uri" { 3 } else { 2 },
        );
        let decoded = serde_json::from_value::<ReviewedSource>(sample.clone()).unwrap();
        assert_eq!(
            serde_json::to_value(decoded).unwrap()["kind"],
            sample["kind"]
        );
        let mut invalid = sample;
        invalid["metadata"] = json!("untrusted");
        assert!(serde_json::from_value::<ReviewedSource>(invalid).is_err());
    }
}

#[test]
fn every_published_request_choice_passes_http_deserialization() {
    // Unknown callers prove parsing reaches authorization without requiring
    // music-service linkage or sending any speaker command.
    let api = Api::start("", &Client::Unknown);
    let (_, doc) = api.get("/openapi.json");
    let denied = |path: &str, request: Value| {
        let (status, reply) = api.post(path, &request);
        assert_eq!(status, 403, "{path}: {request} => {reply}");
        assert_eq!(reply["code"], "POLICY_DENIED");
    };
    for mode in choices(&doc, "/sources/control", "mode") {
        for source in sources() {
            denied(
                "/sources/control",
                json!({
                    "zone":"Kitchen", "expected_revision":"a".repeat(64),
                    "mode":mode, "source":source,
                }),
            );
        }
    }
    for action in choices(&doc, "/dj/control", "action") {
        denied(
            "/dj/control",
            json!({
                "zone":"Kitchen", "expected_revision":"a".repeat(64),
                "action":action, "clear":action == "steer",
            }),
        );
    }
    for path in ["/sonos/playlists/track", "/sonos/playlists/tracks"] {
        for mode in choices(&doc, path, "mode") {
            let mut request = json!({
                "zone":"Kitchen", "playlist":"SQ:1", "expected_revision":"observed", "mode":mode,
            });
            if path.ends_with("/tracks") {
                request["positions"] = json!([1, 2]);
            } else {
                request["position"] = json!(1);
            }
            denied(path, request);
        }
    }
    assert!(api.actions().is_empty());
}
