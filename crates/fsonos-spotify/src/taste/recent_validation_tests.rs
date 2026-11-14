//! Full refreshes must not bypass the lightweight reader's history validation.

use super::*;
use crate::cache::sync_library_detailed;
use crate::recent::RECENT_SCOPE;
use chrono::{DateTime, SecondsFormat, Utc};
use fsonos_core::store::SqliteStore;

const HISTORY: &str = "/v1/me/player/recently-played";

fn stamp(at: i64) -> String {
    DateTime::from_timestamp(at, 0)
        .unwrap()
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn play(number: usize, at: i64) -> Value {
    json!({ "track": track(number), "played_at": stamp(at) })
}

fn uri(number: usize) -> String {
    format!("spotify:track:TestTrack{number}")
}

#[test]
fn recent_validation_uses_the_window_time_after_slow_optional_reads() {
    let server = Server::new();
    let started_at = Utc::now().timestamp() - 600;
    server.set(HISTORY, json!({ "items": [play(0, started_at + 590)] }));
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, RECENT_SCOPE);
        let mut read = TasteRead::default();
        let mut taste = crate::taste::Taste::default();
        let mut reader = Reader {
            session: &mut session,
            cx: &cx,
            read: &mut read,
            owner: Some("test-owner"),
            receipt: None,
        };
        let started = std::time::Instant::now() - Duration::from_secs(600);
        reader.recent_plays(&mut taste, started_at, started).await;
        assert!(!read.incomplete(), "{read:?}");
        assert_eq!(taste.heard, Some(vec![(uri(0), started_at + 590)]));
    });
}

#[test]
fn full_sync_rejects_invalid_recent_windows_without_poisoning_retained_history() {
    let server = Server::new();
    let now = Utc::now().timestamp();
    server.set(
        "/v1/me/tracks",
        page(vec![json!({ "track": track(0) })], 0, None),
    );
    server.set(HISTORY, json!({ "items": [play(1, now - 60)] }));
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let path = server.data.join("full-sync-recent-validation.db");
        let mut session = server.session(&cx, RECENT_SCOPE);
        let mut store = SqliteStore::open(&path).unwrap();
        sync_library_detailed(&mut session, &cx, &mut store)
            .await
            .unwrap();
        let before = store.spotify_heard().unwrap();
        let owner = store.spotify_library_owner().unwrap();
        assert_eq!(before, [(uri(1), now - 60)]);
        store.close().unwrap();

        let mut missing_time = play(0, now - 10);
        missing_time.as_object_mut().unwrap().remove("played_at");
        let mut malformed_time = play(0, now - 10);
        malformed_time["played_at"] = json!("not-a-time");
        let mut bad_uri = play(0, now - 10);
        bad_uri["track"]["uri"] = json!("spotify:track:bad/identity");
        let invalid = [
            json!({ "items": [play(2, now - 20), play(0, now + 86_400)] }),
            json!({ "items": [play(2, now - 20), missing_time] }),
            json!({ "items": [play(2, now - 20), malformed_time] }),
            json!({ "items": [play(2, now - 20), play(0, -1)] }),
            json!({ "items": [play(2, now - 20), bad_uri] }),
            json!({ "items": (0..51).map(|_| play(2, now - 20)).collect::<Vec<_>>() }),
            json!({ "next": null }),
            json!({ "items": [play(2, now - 20)], "padding": "x".repeat(2 * 1024 * 1024) }),
        ];
        // This essential saved-library change still publishes: the optional
        // history source fails independently and keeps its previous evidence.
        server.set(
            "/v1/me/tracks",
            page(vec![json!({ "track": track(3) })], 0, None),
        );
        for (index, body) in invalid.into_iter().enumerate() {
            server.set(HISTORY, body);
            let mut store = SqliteStore::open(&path).unwrap();
            let result = sync_library_detailed(&mut session, &cx, &mut store)
                .await
                .unwrap();
            assert!(
                result
                    .notes
                    .iter()
                    .any(|note| note.contains("recently played")),
                "invalid case {index}: {result:?}"
            );
            assert_eq!(store.spotify_heard().unwrap(), before, "case {index}");
            assert_eq!(store.spotify_library_owner().unwrap(), owner);
            let pool = pool_from_store(&store).unwrap();
            assert!(pool.get(&uri(3)).is_some(), "saved music still publishes");
            assert!(pool.get(&uri(1)).is_some(), "prior taste survives failure");
            assert!(pool.get(&uri(2)).is_none(), "no partially accepted window");
            store.close().unwrap();
        }

        // The rejected future row cannot outlive a subsequent legitimate play.
        server.set(HISTORY, json!({ "items": [play(0, now - 5)] }));
        let mut store = SqliteStore::open(&path).unwrap();
        sync_library_detailed(&mut session, &cx, &mut store)
            .await
            .unwrap();
        assert_eq!(
            store.spotify_heard().unwrap(),
            [(uri(1), now - 60), (uri(0), now - 5)]
        );
        store.close().unwrap();
    });
    assert_eq!(server.calls(HISTORY).len(), 10);
}

#[test]
fn full_sync_recent_history_keeps_latest_plays_independently_of_current_availability() {
    let server = Server::new();
    let now = Utc::now().timestamp();
    let mut restricted = play(3, now - 30);
    restricted["track"]["restrictions"] = json!({ "reason": "market" });
    let mut local = play(4, now - 40);
    local["track"]["is_local"] = json!(true);
    let mut non_track = play(5, now - 50);
    non_track["track"]["type"] = json!("episode");
    server.set(
        HISTORY,
        json!({
            "items": [play(2, now - 20), play(2, now - 40), play(0, now - 60),
                restricted, local, non_track, { "track": null, "played_at": stamp(now - 70) }],
            "next": "https://example.invalid/older-history"
        }),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, RECENT_SCOPE);
        let mut store = MemStore::default();
        store.adopt_spotify_library_owner("test-owner").unwrap();
        store.save_spotify_heard(&[(uri(0), now - 10)]).unwrap();
        let result = sync_library_detailed(&mut session, &cx, &mut store)
            .await
            .unwrap();
        assert_eq!(result.notes.len(), 1, "{result:?}");
        assert!(result.notes[0].starts_with("the Spotify grant lacks "));
        assert_eq!(
            store.spotify_heard().unwrap(),
            [(uri(3), now - 30), (uri(2), now - 20), (uri(0), now - 10)]
        );
        let pool = pool_from_store(&store).unwrap();
        assert!(pool.get(&uri(0)).is_some());
        assert!(pool.get(&uri(2)).is_some());
        for number in [3, 4, 5] {
            assert!(pool.get(&uri(number)).is_none());
        }
    });
    assert_eq!(server.calls(HISTORY).len(), 1);
    assert!(server.calls("/older-history").is_empty());
}
