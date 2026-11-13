use super::*;
use crate::client::{CachedToken, TokenCache};
use crate::fake_spotify::{FakeSpotify, config, runtime};
use asupersync::http::Client;
use serde_json::{Value, json};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

const QUERY: &str = "catalog";
const KINDS: [&str; 6] = ["track", "album", "artist", "playlist", "show", "episode"];
static NEXT_DIRECTORY: AtomicU64 = AtomicU64::new(0);

struct Server {
    fake: Option<FakeSpotify>,
    data: PathBuf,
}

impl Server {
    fn new() -> Self {
        let fake = FakeSpotify::start();
        let data = std::env::temp_dir().join(format!(
            "fsonos-catalog-search-{}-{}-{}",
            std::process::id(),
            fake.addr.port(),
            NEXT_DIRECTORY.fetch_add(1, Ordering::Relaxed)
        ));
        Self {
            fake: Some(fake),
            data,
        }
    }

    fn fake(&self) -> &FakeSpotify {
        self.fake.as_ref().unwrap()
    }

    fn session(&self, cx: &Cx, authorized: bool) -> Session {
        let cache = TokenCache::in_data_dir(&self.data);
        if authorized {
            cache
                .store(&CachedToken {
                    access_token: "search-access".into(),
                    refresh_token: "search-refresh".into(),
                    expires_at: i64::MAX / 2,
                    scope: "user-library-read".into(),
                })
                .unwrap();
            let mut state = self.fake().state.lock().unwrap();
            "search-access".clone_into(&mut state.access);
            "search-refresh".clone_into(&mut state.refresh);
        }
        Session::open(config(), cache, Client::default_for_runtime(cx))
            .unwrap()
            .with_endpoints(self.fake().endpoints())
    }

    fn route(&self, query: &str, kind: &str, limit: u32) -> String {
        self.fake()
            .endpoints()
            .search(query, kind, limit)
            .strip_prefix(&format!("http://{}", self.fake().addr))
            .unwrap()
            .into()
    }

    fn set(&self, query: &str, kind: &str, limit: u32, document: &Value) {
        self.fake()
            .state
            .lock()
            .unwrap()
            .library_pages
            .insert(self.route(query, kind, limit), document.to_string());
    }

    fn refuse(&self, status: u16, retry_after: Option<u32>) {
        self.fake()
            .state
            .lock()
            .unwrap()
            .api_errors
            .insert(self.route(QUERY, "track", 10), (status, retry_after));
    }

    fn calls(&self) -> Vec<String> {
        self.fake().state.lock().unwrap().log.clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(fake) = self.fake.take() {
            fake.stop();
        }
    }
}

fn item_id(number: usize) -> String {
    format!("Item{number:018}")
}

fn item_uri(kind: &str, number: usize) -> String {
    format!("spotify:{kind}:{}", item_id(number))
}

fn item(kind: &str, number: usize) -> Value {
    json!({
        "id": item_id(number), "uri": item_uri(kind, number), "type": kind,
        "name": format!("Catalog {kind} {number}"), "is_playable": true,
        "artists": [{ "name": "First Creator" }, { "name": "Second Creator" }],
        "owner": { "id": "fixture-owner", "display_name": "Playlist Owner" },
        "publisher": "Show Publisher", "show": { "name": "Episode's Show" },
    })
}

fn response(kind: &str, items: &[Value], total: u32, limit: u32) -> Value {
    let mut object = serde_json::Map::new();
    object.insert(
        format!("{kind}s"),
        json!({
            "items": items, "offset": 0, "limit": limit, "total": total,
            "next": if total as usize > items.len() {
                Some("https://example.invalid/never-follow-search-pages")
            } else { None },
        }),
    );
    Value::Object(object)
}

fn assert_malformed(kind: &str, document: &Value) {
    let server = Server::new();
    server.set(QUERY, kind, 10, document);
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        assert!(matches!(
            search(&mut session, &cx, QUERY, kind, 10).await,
            Err(SpotifyError::Decode(_))
        ));
    });
    assert_eq!(
        server.calls(),
        [format!("GET {}", server.route(QUERY, kind, 10))]
    );
}

#[test]
fn all_six_kinds_return_canonical_ordered_sources_and_their_creator_bylines() {
    let server = Server::new();
    for kind in KINDS {
        server.set(
            QUERY,
            kind,
            10,
            &response(kind, &[item(kind, 2), item(kind, 1)], 2, 10),
        );
    }
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        for kind in KINDS {
            let found = search(&mut session, &cx, QUERY, kind, 10).await.unwrap();
            assert_eq!(found.items.len(), 2);
            assert!(!found.more);
            assert_eq!(found.items[0].source_uri, item_uri(kind, 2));
            assert_eq!(found.items[1].source_uri, item_uri(kind, 1));
            assert_eq!(found.items[0].title, format!("Catalog {kind} 2"));
            let subtitle = match kind {
                "track" | "album" => Some("First Creator, Second Creator"),
                "playlist" => Some("Playlist Owner"),
                "show" => Some("Show Publisher"),
                "episode" => Some("Episode's Show"),
                _ => None,
            };
            assert_eq!(found.items[0].subtitle.as_deref(), subtitle);
        }
    });
    assert_eq!(server.calls().len(), KINDS.len());
    assert!(server.calls().iter().all(|call| {
        call.starts_with("GET /v1/search?")
            && !call.contains("market=")
            && !call.contains("include_external=")
    }));
}

#[test]
fn unicode_queries_are_trimmed_and_encoded_once_without_query_injection() {
    let server = Server::new();
    let query = "Björk & café + genre:jazz";
    server.set(
        query,
        "track",
        1,
        &response("track", &[item("track", 1)], 100, 1),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        let found = search(&mut session, &cx, &format!("  {query}  "), "track", 1)
            .await
            .unwrap();
        assert_eq!(found.items.len(), 1);
        assert!(found.more);
    });
    assert_eq!(
        server.calls(),
        [
            "GET /v1/search?q=Bj%C3%B6rk%20%26%20caf%C3%A9%20%2B%20genre%3Ajazz&type=track&limit=1&offset=0"
        ]
    );
}

#[test]
fn invalid_query_kind_and_limit_fail_before_any_http_request() {
    let server = Server::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        for query in [
            "".into(),
            "   ".into(),
            "bad\nquery".into(),
            "\tcatalog".into(),
            "x".repeat(257),
            "é".repeat(129),
        ] {
            assert!(matches!(
                search(&mut session, &cx, &query, "track", 10).await,
                Err(SpotifyError::Config(_))
            ));
        }
        for kind in ["", "Track", "track,album", "audiobook", "track&limit=50"] {
            assert!(matches!(
                search(&mut session, &cx, QUERY, kind, 10).await,
                Err(SpotifyError::Config(_))
            ));
        }
        for limit in [0, 11, u32::MAX] {
            assert!(matches!(
                search(&mut session, &cx, QUERY, "track", limit).await,
                Err(SpotifyError::Config(_))
            ));
        }
    });
    assert!(server.calls().is_empty());
}

#[test]
fn query_byte_boundary_is_accepted_and_results_never_exceed_ten() {
    let server = Server::new();
    let query = "é".repeat(128);
    let rows: Vec<_> = (0..10).map(|number| item("track", number)).collect();
    server.set(&query, "track", 10, &response("track", &rows, 1000, 10));
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        let found = search(&mut session, &cx, &query, "track", 10)
            .await
            .unwrap();
        assert_eq!(found.items.len(), 10);
        assert!(found.more);
        assert_eq!(found.items[9].source_uri, item_uri("track", 9));
    });
    assert_eq!(
        server.calls().len(),
        1,
        "the external next URL is never requested"
    );
}

#[test]
fn null_unavailable_local_and_restricted_rows_are_skipped_and_duplicates_keep_order() {
    for kind in ["track", "episode"] {
        let server = Server::new();
        let mut rows = vec![
            item(kind, 2),
            Value::Null,
            json!({ "is_playable": false, "id": null, "uri": "unavailable placeholder" }),
            json!({ "is_local": true }),
            json!({ "restrictions": { "reason": "future-restriction" } }),
            item(kind, 2),
            item(kind, 1),
        ];
        if kind == "track" {
            rows.insert(
                1,
                json!({ "album": { "restrictions": { "reason": "product" } } }),
            );
        }
        server.set(QUERY, kind, 10, &response(kind, &rows, 30, 10));
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut session = server.session(&cx, true);
            let found = search(&mut session, &cx, QUERY, kind, 10).await.unwrap();
            let uris: Vec<_> = found
                .items
                .iter()
                .map(|item| item.source_uri.as_str())
                .collect();
            assert_eq!(uris, [item_uri(kind, 2), item_uri(kind, 1)]);
            assert!(found.more);
        });
        assert_eq!(server.calls().len(), 1);
    }
}

#[test]
fn track_search_rejects_nested_album_restrictions_and_malformed_evidence() {
    for restriction in [
        json!({ "reason": "market" }),
        json!({ "reason": "future-reason" }),
        json!({}),
    ] {
        let mut row = item("track", 1);
        row["album"] = json!({ "restrictions": restriction });
        row["uri"] = "unavailable album needs no playable identity".into();
        let decoded: Item = serde_json::from_value(row).unwrap();
        assert!(catalog_item(&decoded, "track").unwrap().is_none());
    }
    for album in [
        None,
        Some(Value::Null),
        Some(json!({})),
        Some(json!({ "restrictions": null })),
    ] {
        let mut row = item("track", 1);
        if let Some(album) = album {
            row["album"] = album;
        }
        let decoded: Item = serde_json::from_value(row).unwrap();
        assert_eq!(
            catalog_item(&decoded, "track").unwrap().unwrap().source_uri,
            item_uri("track", 1)
        );
    }
    for malformed in [json!("market"), json!([]), json!(false), json!(0)] {
        let mut row = item("track", 1);
        row["album"] = json!({ "restrictions": malformed });
        assert!(serde_json::from_value::<Item>(row).is_err());
    }
}

#[test]
fn empty_results_are_successful_and_provider_more_survives_filtering() {
    for filtered in [false, true] {
        let server = Server::new();
        let rows = if filtered {
            vec![json!({ "is_playable": false })]
        } else {
            vec![]
        };
        server.set(
            QUERY,
            "track",
            10,
            &response("track", &rows, if filtered { 20 } else { 0 }, 10),
        );
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut session = server.session(&cx, true);
            let found = search(&mut session, &cx, QUERY, "track", 10).await.unwrap();
            assert!(found.items.is_empty());
            assert_eq!(found.more, filtered);
        });
        assert_eq!(server.calls().len(), 1);
    }
}

#[test]
fn invalid_usable_identity_or_display_never_returns_an_earlier_valid_result() {
    for variant in 0..10 {
        let mut bad = item("track", 2);
        match variant {
            0 => bad["id"] = "short".into(),
            1 => bad["uri"] = item_uri("track", 3).into(),
            2 => bad["type"] = "episode".into(),
            3 => bad["uri"] = "https://example.invalid/item".into(),
            4 => bad["id"] = Value::Null,
            5 => bad["name"] = "".into(),
            6 => bad["name"] = "bad\u{0}title".into(),
            7 => bad["name"] = "\ntrimmed control".into(),
            8 => bad["name"] = "x".repeat(MAX_TITLE_BYTES + 1).into(),
            _ => bad["artists"][0]["name"] = "bad\ncreator".into(),
        }
        assert_malformed("track", &response("track", &[item("track", 1), bad], 2, 10));
    }
    let mut episode = item("episode", 1);
    episode.as_object_mut().unwrap().remove("is_playable");
    assert_malformed("episode", &response("episode", &[episode], 1, 10));
}

#[test]
fn missing_selected_pages_and_inconsistent_counters_are_rejected() {
    for variant in 0..8 {
        let mut document = response("track", &[item("track", 1)], 1, 10);
        let page = &mut document["tracks"];
        match variant {
            0 => page["offset"] = 1.into(),
            1 => page["limit"] = 5.into(),
            2 => page["total"] = 0.into(),
            3 => {
                page["items"] = (0..11)
                    .map(|number| item("track", number))
                    .collect::<Vec<_>>()
                    .into();
            }
            4 => {
                page.as_object_mut().unwrap().remove("offset");
            }
            5 => {
                page.as_object_mut().unwrap().remove("items");
            }
            6 => document = response("album", &[item("album", 1)], 1, 10),
            _ => document = json!({}),
        }
        assert_malformed("track", &document);
    }
}

#[test]
fn optional_fields_are_not_required_and_removed_albums_are_omitted() {
    let server = Server::new();
    let mut track = item("track", 1);
    track.as_object_mut().unwrap().remove("is_playable");
    let mut playlist = item("playlist", 1);
    playlist["owner"]["display_name"] = Value::Null;
    let mut episode = item("episode", 1);
    episode.as_object_mut().unwrap().remove("show");
    let mut album = item("album", 1);
    album["name"] = "".into();
    for (kind, row) in [
        ("track", track),
        ("playlist", playlist),
        ("episode", episode),
        ("album", album),
    ] {
        server.set(QUERY, kind, 10, &response(kind, &[row], 1, 10));
    }
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        assert_eq!(
            search(&mut session, &cx, QUERY, "track", 10)
                .await
                .unwrap()
                .items
                .len(),
            1
        );
        let playlist = search(&mut session, &cx, QUERY, "playlist", 10)
            .await
            .unwrap();
        assert_eq!(playlist.items[0].subtitle.as_deref(), Some("fixture-owner"));
        let episode = search(&mut session, &cx, QUERY, "episode", 10)
            .await
            .unwrap();
        assert_eq!(episode.items[0].subtitle, None);
        assert!(
            search(&mut session, &cx, QUERY, "album", 10)
                .await
                .unwrap()
                .items
                .is_empty()
        );
    });
}

#[test]
fn long_creator_bylines_are_bounded_at_a_valid_utf8_boundary() {
    let server = Server::new();
    let mut row = item("track", 1);
    row["artists"] = json!([{ "name": "é".repeat(255) }, { "name": "Another Creator" }]);
    server.set(QUERY, "track", 10, &response("track", &[row], 1, 10));
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        let found = search(&mut session, &cx, QUERY, "track", 10).await.unwrap();
        let subtitle = found.items[0].subtitle.as_deref().unwrap();
        assert!(subtitle.len() <= MAX_SUBTITLE_BYTES);
        assert!(subtitle.ends_with('…'));
        assert!(subtitle.starts_with(&"é".repeat(254)));
    });
}

#[test]
fn a_missing_grant_fails_without_any_request() {
    let server = Server::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, false);
        assert!(matches!(
            search(&mut session, &cx, QUERY, "track", 10).await,
            Err(SpotifyError::Auth(_))
        ));
    });
    assert!(server.calls().is_empty());
}

#[test]
fn api_refusals_propagate_without_fallback_or_another_page() {
    for status in [400, 403, 404] {
        let server = Server::new();
        server.refuse(status, None);
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut session = server.session(&cx, true);
            assert!(matches!(
                search(&mut session, &cx, QUERY, "track", 10).await,
                Err(SpotifyError::Api { status: actual, .. }) if actual == status
            ));
        });
        assert_eq!(server.calls().len(), 1);
    }
}

#[test]
fn a_long_rate_limit_is_retained_across_subsequent_searches() {
    let server = Server::new();
    server.refuse(429, Some(3600));
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        for kind in ["track", "episode"] {
            assert!(matches!(
                search(&mut session, &cx, QUERY, kind, 10).await,
                Err(SpotifyError::Api { status: 429, .. })
            ));
            assert!(session.retry_after_secs() > 3500);
        }
    });
    assert_eq!(server.calls().len(), 1);
}

#[test]
fn an_expired_access_token_is_refreshed_through_the_existing_session() {
    let server = Server::new();
    server.set(
        QUERY,
        "track",
        10,
        &response("track", &[item("track", 1)], 1, 10),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, true);
        "expired-access".clone_into(&mut server.fake().state.lock().unwrap().access);
        let found = search(&mut session, &cx, QUERY, "track", 10).await.unwrap();
        assert_eq!(found.items[0].source_uri, item_uri("track", 1));
    });
    assert_eq!(
        server.calls(),
        [
            format!("GET {}", server.route(QUERY, "track", 10)),
            "POST /api/token".into(),
            format!("GET {}", server.route(QUERY, "track", 10)),
        ]
    );
    assert_eq!(server.fake().state.lock().unwrap().refreshes, 1);
    let token = TokenCache::in_data_dir(&server.data)
        .load()
        .unwrap()
        .unwrap();
    assert_eq!(token.access_token, "access-r1");
    assert_eq!(token.refresh_token, "refresh-r1");
}
