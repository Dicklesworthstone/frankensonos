//! Scripted Web API pages through the real HTTP client and a loopback listener.

use super::*;
use crate::cache::{apply_library_read, pool_from_store, sync_library};
use crate::client::{CachedToken, TokenCache};
use crate::fake_spotify::{config, json as response, runtime, scratch_dir};
use crate::library::{LibraryItem, Origin};
use crate::session::Session;
use crate::taste::{TasteRead, read_taste};
use asupersync::Cx;
use asupersync::http::Client;
use asupersync::http::h1::server::HostPolicy;
use asupersync::http::h1::types::Request;
use asupersync::http::h1::{Http1Config, Http1Listener, Http1ListenerConfig};
use fsonos_core::store::{MemStore, Store};
use serde_json::{Value, json};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

const API: &str = "https://api.spotify.com/v1";
const PLAYLIST_SCOPE: &str = "user-library-read playlist-read-private";

#[test]
fn offsets_advance_independently_of_query_order_and_encoding() {
    let endpoints = Endpoints::default();
    let root = format!("{API}/me/top/tracks");
    let mut chain = Chain::new(&root, Progress::Offset(0));
    chain
        .follow(&endpoints, &format!("{root}?limit=1&%6Fffset=1"))
        .unwrap();
    chain
        .follow(&endpoints, &format!("{root}?offset=2&limit=1"))
        .unwrap();
    for query in [
        "offset=2",
        "offset=1",
        "offset=3&offset=4",
        "offset=x",
        "limit=50",
    ] {
        assert!(
            chain
                .follow(&endpoints, &format!("{root}?{query}"))
                .is_err()
        );
    }
    chain
        .follow(&endpoints, &format!("{root}?offset=3"))
        .unwrap();
}

#[test]
fn continuations_cannot_switch_collection_or_leave_the_api() {
    let endpoints = Endpoints::default();
    let root = endpoints.playlist_items("TestList", 0);
    let mut chain = Chain::new(&root, Progress::Offset(0));
    for url in [
        endpoints.playlist_items("OtherList", 1),
        endpoints.saved_tracks(1),
        format!("{API}/playlists/TestList/items?offset=1#fragment"),
        "https://example.invalid/v1/playlists/TestList/items?offset=1".into(),
    ] {
        assert!(chain.follow(&endpoints, &url).is_err());
    }
    chain
        .follow(&endpoints, &endpoints.playlist_items("TestList", 1))
        .unwrap();
}

#[test]
fn opaque_cursors_are_compared_by_value_not_by_url_spelling() {
    let endpoints = Endpoints::default();
    let root = format!("{API}/me/following");
    let mut chain = Chain::new(&root, Progress::Cursor(HashSet::new()));
    for cursor in ["ArtistB", "ArtistA"] {
        chain
            .follow(&endpoints, &format!("{root}?type=artist&after={cursor}"))
            .unwrap();
    }
    for query in [
        "after=ArtistB&limit=1",
        "after=Artist%41",
        "after=",
        "type=artist",
        "after=C&after=D",
    ] {
        assert!(
            chain
                .follow(&endpoints, &format!("{root}?{query}"))
                .is_err()
        );
    }
}

#[derive(Default)]
struct Script {
    pages: HashMap<String, Value>,
    calls: Vec<String>,
    base: String,
}

struct Server {
    state: Arc<Mutex<Script>>,
    endpoints: Endpoints,
    data: PathBuf,
    shutdown: Option<Box<dyn FnOnce()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl Server {
    fn new() -> Self {
        let state = Arc::new(Mutex::new(Script::default()));
        let shared = Arc::clone(&state);
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let rt = runtime();
            let handle = rt.handle();
            rt.block_on(async move {
                let config = Http1ListenerConfig::default().http_config(
                    Http1Config::default()
                        .host_policy(HostPolicy::allow_list(vec!["127.0.0.1".into()])),
                );
                let listener = Http1Listener::bind_with_config(
                    "127.0.0.1:0",
                    move |req: Request| {
                        let state = Arc::clone(&shared);
                        async move {
                            let mut state = state.lock().unwrap();
                            state.calls.push(req.uri.clone());
                            if req.header_value("authorization") != Some("Bearer test-access") {
                                return response(401, "{}");
                            }
                            let uri = req.uri.as_str();
                            match state.pages.get(uri).or_else(|| state.pages.get(path(uri))) {
                                Some(body) => {
                                    response(200, body.to_string().replace(API, &state.base))
                                }
                                None => response(404, "{}"),
                            }
                        }
                    },
                    config,
                )
                .await
                .unwrap();
                ready_tx
                    .send((listener.local_addr().unwrap(), listener.shutdown_signal()))
                    .unwrap();
                listener.run(&handle).await.unwrap();
            });
        });
        let (addr, signal) = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let endpoints = Endpoints {
            api: format!("http://{addr}/v1"),
            token: format!("http://{addr}/api/token"),
        };
        state.lock().unwrap().base.clone_from(&endpoints.api);
        let server = Self {
            state,
            endpoints,
            data: scratch_dir(&format!("taste-pages-{}", addr.port())),
            shutdown: Some(Box::new(move || signal.trigger_immediate())),
            thread: Some(thread),
        };
        for collection in ["/v1/me/albums", "/v1/me/tracks", "/v1/me/top/artists"] {
            server.set(collection, page(Vec::new(), 0, None));
        }
        server.set("/v1/me", json!({ "id": "test-owner" }));
        server
    }

    fn set(&self, route: &str, body: Value) {
        self.state.lock().unwrap().pages.insert(route.into(), body);
    }

    fn session(&self, cx: &Cx, scope: &str) -> Session {
        let cache = TokenCache::in_data_dir(&self.data);
        cache
            .store(&CachedToken {
                access_token: "test-access".into(),
                refresh_token: "test-refresh".into(),
                expires_at: i64::MAX / 2,
                scope: format!("user-library-read {scope}"),
            })
            .unwrap();
        Session::open(config(), cache, Client::default_for_runtime(cx))
            .unwrap()
            .with_endpoints(self.endpoints.clone())
    }

    fn calls(&self, prefix: &str) -> Vec<String> {
        self.state
            .lock()
            .unwrap()
            .calls
            .iter()
            .filter(|uri| uri.starts_with(prefix))
            .cloned()
            .collect()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown();
        }
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
        let _ = std::fs::remove_dir_all(&self.data);
    }
}

fn page(items: Vec<Value>, offset: u32, next: Option<String>) -> Value {
    json!({ "items": Value::from(items), "offset": offset, "next": Value::from(next) })
}

fn playlist(n: usize, owned: bool) -> Value {
    json!({ "id": format!("TestList{n}"), "name": format!("Playlist {n}"),
        "owner": { "id": if owned { "test-owner" } else { "other-owner" } } })
}

fn track(n: usize) -> Value {
    json!({ "uri": format!("spotify:track:TestTrack{n}"), "name": format!("Test song {n}"),
        "artists": [{ "name": "Test performer" }], "album": { "name": "Test album" },
        "duration_ms": 180_000, "is_playable": true })
}

#[test]
fn exact_page_budget_completion_is_distinct_from_an_unfinished_read() {
    for more in [false, true] {
        let server = Server::new();
        let first = format!("{API}/me/top/tracks?offset=0");
        let next = format!("{API}/me/top/tracks?offset=1");
        server.set("/v1/me/top/tracks", page(vec![json!(1)], 0, Some(next)));
        server.set(
            "/v1/me/top/tracks?offset=1",
            page(
                vec![json!(2)],
                1,
                more.then(|| format!("{API}/me/top/tracks?offset=2")),
            ),
        );
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut session = server.session(&cx, "user-top-read");
            let mut read = TasteRead::default();
            let mut reader = Reader {
                session: &mut session,
                cx: &cx,
                read: &mut read,
            };
            let first = first.replace(API, &server.endpoints.api);
            assert_eq!(reader.page::<u32>(&first, "top tracks", 2).await, [1, 2]);
            assert_eq!(read.incomplete(), more);
        });
        assert_eq!(server.calls("/v1/me/top/tracks").len(), 2);
    }
}

#[test]
fn looping_cursor_and_wrong_collection_keep_prefixes_without_fetching_the_bad_link() {
    let server = Server::new();
    let cursor = format!("{API}/me/following?after=ArtistA&type=artist");
    let artist = json!({ "id": "ArtistA", "name": "Test performer" });
    server.set(
        "/v1/me/following",
        json!({ "artists": { "items": [artist], "next": cursor }}),
    );
    server.set(
        "/v1/me/following?after=ArtistA&type=artist",
        json!({ "artists": {
            "items": [], "next": format!("{API}/me/following?type=artist&after=Artist%41")
        }}),
    );
    server.set(
        "/v1/me/top/tracks",
        page(vec![track(1)], 0, Some(format!("{API}/me/albums?offset=1"))),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, "user-follow-read user-top-read");
        let (taste, read) = read_taste(&mut session, &cx, &mut MemStore::default(), 1).await;
        assert!(read.incomplete());
        assert_eq!(read.failed.len(), 2);
        assert_eq!(read.followed_artists, 1);
        assert_eq!(read.top_tracks, 1);
        assert_eq!(taste.tracks.len(), 1);
    });
    assert_eq!(server.calls("/v1/me/following").len(), 2);
    assert!(server.calls("/v1/me/albums").is_empty());
}

#[test]
fn a_capped_playlist_directory_preserves_cache_until_a_complete_sync() {
    let server = Server::new();
    for n in 0..MAX_PAGES {
        let offset = u32::try_from(n).unwrap();
        let next = format!("{API}/me/playlists?offset={}", n + 1);
        let route = if n == 0 {
            "/v1/me/playlists".into()
        } else {
            format!("/v1/me/playlists?offset={n}")
        };
        server.set(&route, page(vec![playlist(n, false)], offset, Some(next)));
    }
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        let mut store = MemStore::default();
        let cached = |uri: &str, origin| LibraryItem {
            source_uri: uri.into(),
            title: "Cached song".into(),
            artists: vec!["Test performer".into()],
            origin,
            ..LibraryItem::default()
        };
        apply_library_read(
            &mut store,
            &[
                cached("spotify:track:CachedTaste", Origin::Taste),
                cached("spotify:track:RemovedSave", Origin::SavedAlbum),
            ],
        )
        .unwrap();
        let playlists = vec![(
            "spotify:playlist:CachedList".into(),
            "Cached playlist".into(),
        )];
        store.save_playlists(&playlists).unwrap();
        let partial = sync_library(&mut session, &cx, &mut store).await.unwrap();
        assert_eq!(
            partial.retired, 1,
            "a completed empty library still retires a removed save"
        );
        assert!(
            pool_from_store(&store)
                .unwrap()
                .get("spotify:track:CachedTaste")
                .is_some()
        );
        assert_eq!(store.playlists().unwrap(), playlists);
        server.set("/v1/me/playlists", page(Vec::new(), 0, None));
        let complete = sync_library(&mut session, &cx, &mut store).await.unwrap();
        assert_eq!(
            complete.retired, 1,
            "complete evidence may retire the old taste track"
        );
        assert_eq!(pool_from_store(&store).unwrap().len(), 0);
        assert!(store.playlists().unwrap().is_empty());
    });
    assert_eq!(server.calls("/v1/me/playlists").len(), MAX_PAGES + 1);
}

#[test]
fn playlist_item_and_playlist_count_budgets_report_incomplete_taste() {
    for many_lists in [false, true] {
        let server = Server::new();
        let count = if many_lists {
            super::super::MAX_PLAYLISTS + 1
        } else {
            1
        };
        server.set(
            "/v1/me/playlists",
            page((0..count).map(|n| playlist(n, true)).collect(), 0, None),
        );
        for n in 0..count {
            server.set(
                &format!("/v1/playlists/TestList{n}/items"),
                page(vec![json!({ "item": track(n) })], 0, None),
            );
        }
        if !many_lists {
            for n in 0..super::super::MAX_PLAYLIST_PAGES {
                let route = if n == 0 {
                    "/v1/playlists/TestList0/items".into()
                } else {
                    format!("/v1/playlists/TestList0/items?offset={n}")
                };
                let next = format!("{API}/playlists/TestList0/items?offset={}", n + 1);
                server.set(
                    &route,
                    page(
                        vec![json!({ "item": track(n) })],
                        u32::try_from(n).unwrap(),
                        Some(next),
                    ),
                );
            }
        }
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut session = server.session(&cx, PLAYLIST_SCOPE);
            let (taste, read) = read_taste(&mut session, &cx, &mut MemStore::default(), 1).await;
            assert!(read.incomplete());
            assert_eq!(
                taste.playlists.as_ref().unwrap().len(),
                count,
                "the directory itself was complete"
            );
            let expected = if many_lists {
                super::super::MAX_PLAYLISTS
            } else {
                super::super::MAX_PLAYLIST_PAGES
            };
            assert_eq!(taste.tracks.len(), expected);
            assert_eq!(server.calls("/v1/playlists/").len(), expected);
        });
    }
}

#[test]
fn malformed_directory_coverage_preserves_cache_until_a_complete_retry() {
    let server = Server::new();
    let previous = vec![("spotify:playlist:Previous".into(), "Previous list".into())];
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        let mut store = MemStore::default();
        let item = LibraryItem {
            source_uri: "spotify:track:Previous".into(),
            title: "Previous song".into(),
            artists: vec!["Test performer".into()],
            origin: Origin::Taste,
            ..LibraryItem::default()
        };
        apply_library_read(&mut store, &[item]).unwrap();
        store.save_playlists(&previous).unwrap();
        let before = store.library().unwrap();
        for body in [
            json!({ "items": [playlist(0, false)], "offset": 0, "total": 2, "next": null }),
            json!({ "items": [playlist(0, false)], "offset": 1, "total": 2, "next": null }),
            json!({ "items": [playlist(0, false)], "offset": 0, "total": 0, "next": null }),
            json!({ "items": [playlist(0, false)], "offset": 0, "total": 3,
                "next": format!("{API}/me/playlists?offset=2") }),
        ] {
            server.set("/v1/me/playlists", body);
            let result = sync_library(&mut session, &cx, &mut store).await.unwrap();
            assert_eq!(result.retired, 0);
            assert_eq!(store.library().unwrap(), before);
            assert_eq!(store.playlists().unwrap(), previous);
        }
        server.set(
            "/v1/me/playlists",
            json!({
                "items": [playlist(0, false)], "offset": 0, "total": 1, "next": null
            }),
        );
        let complete = sync_library(&mut session, &cx, &mut store).await.unwrap();
        assert_eq!(complete.retired, 1);
        assert_eq!(store.playlists().unwrap().len(), 1);
        assert_eq!(
            store.playlists().unwrap()[0].0,
            "spotify:playlist:TestList0"
        );
    });
    assert_eq!(
        server.calls("/v1/me/playlists").len(),
        5,
        "invalid links were never fetched"
    );
}

#[test]
fn playlist_coverage_counts_unavailable_entries_and_episodes_before_filtering() {
    let server = Server::new();
    server.set("/v1/me/playlists", page(vec![playlist(0, true)], 0, None));
    let mut unavailable = track(0);
    unavailable["is_playable"] = false.into();
    server.set(
        "/v1/playlists/TestList0/items",
        json!({
            "items": [
                { "item": null },
                { "item": { "type": "episode" } },
                { "item": unavailable }
            ],
            "offset": 0, "total": 4,
            "next": format!("{API}/playlists/TestList0/items?offset=3")
        }),
    );
    server.set(
        "/v1/playlists/TestList0/items?offset=3",
        json!({
            "items": [{ "item": track(1) }], "offset": 3, "total": 4, "next": null
        }),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        let (taste, read) = read_taste(&mut session, &cx, &mut MemStore::default(), 1).await;
        assert!(!read.incomplete(), "{read:?}");
        assert_eq!(read.playlist_tracks, 1);
        assert_eq!(taste.tracks[0].source_uri, "spotify:track:TestTrack1");
    });
    assert_eq!(server.calls("/v1/playlists/").len(), 2);
}

#[test]
fn a_changed_directory_total_keeps_earlier_pages_but_cannot_replace_cached_names() {
    let server = Server::new();
    server.set(
        "/v1/me/playlists",
        json!({
            "items": [playlist(0, false)], "offset": 0, "total": 2,
            "next": format!("{API}/me/playlists?offset=1")
        }),
    );
    server.set(
        "/v1/me/playlists?offset=1",
        json!({
            "items": [playlist(1, false)], "offset": 1, "total": 3,
            "next": format!("{API}/me/playlists?offset=2")
        }),
    );
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut session = server.session(&cx, PLAYLIST_SCOPE);
        let (taste, read) = read_taste(&mut session, &cx, &mut MemStore::default(), 1).await;
        assert!(read.incomplete());
        assert_eq!(taste.playlists, None);
        assert_eq!(read.failed.len(), 1);
        assert!(read.failed[0].1.contains("changed total"));
    });
    assert_eq!(server.calls("/v1/me/playlists").len(), 2);
}
