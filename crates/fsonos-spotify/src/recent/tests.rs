//! The lightweight reader through a real loopback HTTP client, with account
//! changes and malformed observations kept away from accepted listening.

use super::*;
use crate::cache::{apply_library_read, works_from_store};
use crate::client::{CachedToken, Endpoints, SCOPE, TokenCache};
use crate::dj::{DjConfig, WorkPool};
use crate::fake_spotify::{config, json as response, runtime};
use crate::feed::{Planning, QueueFeed};
use crate::library::{LibraryItem, Origin};
use crate::rehearse::Span;
use asupersync::http::Client;
use asupersync::http::h1::server::HostPolicy;
use asupersync::http::h1::types::{Request, Response};
use asupersync::http::h1::{Http1Config, Http1Listener, Http1ListenerConfig};
use chrono::{DateTime, SecondsFormat};
use fsonos_core::store::{MemStore, SqliteStore};
use fsonos_types::PlayerId;
use serde_json::{Value, json};
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

const NOW: i64 = 1_790_035_200;
const OWNER: &str = "recent-owner";
const HISTORY: &str = "/v1/me/player/recently-played?limit=50";

#[derive(Default)]
struct Script {
    owners: VecDeque<String>,
    history: Value,
    status: u16,
    calls: Vec<String>,
    before_history: Option<Box<dyn FnOnce() + Send>>,
}

impl Script {
    fn respond(&mut self, request: &Request) -> Response {
        self.calls.push(request.uri.clone());
        if request.header_value("authorization") != Some("Bearer recent-access") {
            return response(401, "{}");
        }
        match request.uri.as_str() {
            "/v1/me" => {
                let owner = if self.owners.len() > 1 {
                    self.owners.pop_front().unwrap()
                } else {
                    self.owners.front().cloned().unwrap_or_default()
                };
                response(200, json!({"id": owner}).to_string())
            }
            HISTORY => {
                if let Some(change) = self.before_history.take() {
                    change();
                }
                response(self.status, self.history.to_string()).with_header("Retry-After", "3600")
            }
            _ => response(404, "{}"),
        }
    }
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
        let state = Arc::new(Mutex::new(Script {
            owners: VecDeque::from([OWNER.to_owned()]),
            history: json!({"items": [], "next": null}),
            status: 200,
            ..Script::default()
        }));
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
                    move |request: Request| {
                        let shared = Arc::clone(&shared);
                        async move { shared.lock().unwrap().respond(&request) }
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
        let (address, signal) = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        let data = std::env::temp_dir().join(format!(
            "fsonos-recent-{}-{}",
            std::process::id(),
            address.port()
        ));
        std::fs::create_dir_all(&data).unwrap();
        Self {
            state,
            endpoints: Endpoints {
                api: format!("http://{address}/v1"),
                token: format!("http://{address}/api/token"),
            },
            data,
            shutdown: Some(Box::new(move || signal.trigger_immediate())),
            thread: Some(thread),
        }
    }

    fn session(&self, cx: &Cx, scope: &str) -> Session {
        let cache = TokenCache::in_data_dir(&self.data);
        cache
            .store(&CachedToken {
                access_token: "recent-access".into(),
                refresh_token: "recent-refresh".into(),
                expires_at: i64::MAX / 2,
                scope: format!("{SCOPE} {scope}"),
            })
            .unwrap();
        Session::open(config(), cache, Client::default_for_runtime(cx))
            .unwrap()
            .with_endpoints(self.endpoints.clone())
    }

    fn history(&self, value: Value) {
        self.state.lock().unwrap().history = value;
    }

    fn calls(&self) -> Vec<String> {
        self.state.lock().unwrap().calls.clone()
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            shutdown();
        }
        if let Some(thread) = self.thread.take() {
            thread.join().unwrap();
        }
    }
}

fn stamp(at: i64) -> String {
    DateTime::from_timestamp(at, 0)
        .unwrap()
        .to_rfc3339_opts(SecondsFormat::Secs, true)
}

fn item(uri: &str, at: i64) -> Value {
    json!({"played_at": stamp(at), "track": {"uri": uri, "type": "track"}})
}

fn owned_store() -> MemStore {
    let mut store = MemStore::default();
    store.adopt_spotify_library_owner(OWNER).unwrap();
    store
        .save_spotify_heard(&[("spotify:track:Retained".into(), NOW - 300)])
        .unwrap();
    store
}

fn next_uri(pool: &WorkPool, store: &impl Store) -> String {
    let feed = QueueFeed::new(
        &PlayerId("RECENT_TEST_ZONE".into()),
        DjConfig::default(),
        17,
    );
    feed.rehearse(
        store,
        Planning {
            pool,
            steer: None,
            feedback: None,
            now: NOW,
            local_hour: Some(12),
            queued_elsewhere: &[],
        },
        DateTime::from_timestamp(NOW, 0).unwrap().fixed_offset(),
        Span::Works(1),
        &|_| None,
        17,
    )
    .unwrap()[0]
        .planned
        .movements[0]
        .track
        .source_uri
        .clone()
}

#[test]
fn fresh_listening_changes_the_next_pick_without_rebuilding_or_rewriting_the_library() {
    let server = Server::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = owned_store();
        let songs: Vec<_> = (0..4)
            .map(|number| LibraryItem {
                source_uri: format!("spotify:track:RecentSong{number}"),
                title: format!("Song {number}"),
                artists: vec![format!("Test performer {number}")],
                genres: vec!["pop".into()],
                duration_secs: Some(180),
                origin: Origin::LikedTrack,
                ..LibraryItem::default()
            })
            .collect();
        apply_library_read(&mut store, &songs).unwrap();
        let library = store.library().unwrap();
        let pool = works_from_store(&store).unwrap();
        let picked = next_uri(&pool, &store);
        server.history(json!({"items": [item(&picked, NOW - 30)], "next": null}));
        let mut session = server.session(&cx, RECENT_SCOPE);
        let report = refresh_recent_listening(&mut session, &cx, &mut store, NOW)
            .await
            .unwrap();
        assert_eq!(report.checked_at, Some(NOW));
        assert_eq!(
            (report.received, report.accepted, report.skipped),
            (1, 1, 0)
        );
        assert!(!report.more_available && !report.requires_refresh && !report.missing_scope);
        assert_eq!(report.owner.as_deref(), Some(OWNER));
        assert_eq!(report.generation, Some(1));
        assert_ne!(next_uri(&pool, &store), picked);
        assert_eq!(store.library().unwrap(), library);
        assert!(store.recent_plays(None, 10).unwrap().is_empty());
        assert_eq!(server.calls(), ["/v1/me", HISTORY, "/v1/me"]);
    });
}

#[test]
fn repeated_and_empty_windows_never_erase_unseen_or_newer_listening() {
    let server = Server::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = owned_store();
        let mut session = server.session(&cx, RECENT_SCOPE);
        server.history(json!({"items": [
            item("spotify:track:New", NOW - 20),
            item("spotify:track:Retained", NOW - 600),
            item("spotify:track:New", NOW - 40)
        ]}));
        let report = refresh_recent_listening(&mut session, &cx, &mut store, NOW)
            .await
            .unwrap();
        assert_eq!((report.received, report.accepted), (3, 2));
        let heard = store.spotify_heard().unwrap();
        assert_eq!(
            heard,
            [
                ("spotify:track:Retained".into(), NOW - 300),
                ("spotify:track:New".into(), NOW - 20)
            ]
        );
        server.history(json!({"items": []}));
        let empty = refresh_recent_listening(&mut session, &cx, &mut store, NOW + 900)
            .await
            .unwrap();
        assert_eq!(empty.checked_at, Some(NOW + 900));
        assert_eq!((empty.received, empty.accepted), (0, 0));
        assert_eq!(store.spotify_heard().unwrap(), heard);
    });
}

#[test]
fn unavailable_local_and_non_track_rows_do_not_require_unrelated_catalog_metadata() {
    let page = json!({"items": [
        {"played_at": stamp(NOW), "track": null},
        {"played_at": stamp(NOW), "track": {"uri": null}},
        {"played_at": stamp(NOW), "track": {"uri": "spotify:local:Private", "is_local": true}},
        {"played_at": stamp(NOW), "track": {"uri": "spotify:episode:Episode", "type": "episode"}},
        {"played_at": stamp(NOW), "track": {"uri": "spotify:track:Heard", "is_playable": false}}
    ]});
    let observed = observation(&serde_json::to_vec(&page).unwrap(), NOW).unwrap();
    assert_eq!((observed.received, observed.skipped), (5, 4));
    assert_eq!(observed.plays, [("spotify:track:Heard".into(), NOW)]);
}

#[test]
fn missing_scope_and_unadopted_accounts_make_no_personal_http_reads() {
    let server = Server::new();
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = owned_store();
        let before = store.spotify_heard().unwrap();
        let mut session = server.session(&cx, "");
        let report = refresh_recent_listening(&mut session, &cx, &mut store, NOW)
            .await
            .unwrap();
        assert!(report.missing_scope);
        assert_eq!(report.checked_at, None);
        assert_eq!(store.spotify_heard().unwrap(), before);
        let mut session = server.session(&cx, RECENT_SCOPE);
        let report = refresh_recent_listening(&mut session, &cx, &mut MemStore::default(), NOW)
            .await
            .unwrap();
        assert!(report.requires_refresh);
        assert_eq!(report.checked_at, None);
        assert!(server.calls().is_empty());
    });
}

#[test]
fn a_profile_change_on_either_side_of_the_window_cannot_publish_it() {
    for owners in [vec!["different-owner"], vec![OWNER, "different-owner"]] {
        let server = Server::new();
        server.state.lock().unwrap().owners = owners.iter().map(|id| (*id).to_owned()).collect();
        server.history(json!({"items": [item("spotify:track:Wrong", NOW)]}));
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let mut store = owned_store();
            let before = store.spotify_heard().unwrap();
            let mut session = server.session(&cx, RECENT_SCOPE);
            let report = refresh_recent_listening(&mut session, &cx, &mut store, NOW)
                .await
                .unwrap();
            assert!(report.requires_refresh);
            assert_eq!((report.checked_at, report.accepted), (None, 0));
            assert_eq!(store.spotify_heard().unwrap(), before);
        });
        let count = usize::from(owners.len() == 2);
        assert_eq!(
            server
                .calls()
                .iter()
                .filter(|call| *call == HISTORY)
                .count(),
            count
        );
    }
}

#[test]
fn a_returning_accounts_new_generation_rejects_the_old_inflight_window() {
    let server = Server::new();
    let path = server.data.join("adoption.db");
    let change_path = path.clone();
    server.state.lock().unwrap().before_history = Some(Box::new(move || {
        let mut current = SqliteStore::open(&change_path).unwrap();
        current
            .adopt_spotify_library_owner("temporary-owner")
            .unwrap();
        current.adopt_spotify_library_owner(OWNER).unwrap();
        current
            .save_spotify_heard(&[("spotify:track:Current".into(), NOW)])
            .unwrap();
        current.close().unwrap();
    }));
    server.history(json!({"items": [item("spotify:track:PreviousAdoption", NOW)]}));
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = SqliteStore::open(&path).unwrap();
        let (original, _) = store.adopt_spotify_library_owner(OWNER).unwrap();
        let mut session = server.session(&cx, RECENT_SCOPE);
        let report = refresh_recent_listening(&mut session, &cx, &mut store, NOW)
            .await
            .unwrap();
        assert!(report.requires_refresh);
        assert_eq!(report.generation, Some(original.generation));
        assert_eq!((report.checked_at, report.accepted), (None, 0));
        assert_ne!(store.spotify_library_owner().unwrap(), Some(original));
        assert_eq!(
            store.spotify_heard().unwrap(),
            [("spotify:track:Current".into(), NOW)]
        );
        store.close().unwrap();
    });
}

#[test]
fn malformed_oversized_and_future_windows_preserve_the_previous_observation() {
    let server = Server::new();
    let too_many: Vec<_> = (0..=RECENT_WINDOW)
        .map(|number| item(&format!("spotify:track:TooMany{number}"), NOW))
        .collect();
    let pages = [
        json!({}),
        json!({"items": null}),
        json!({"items": [{"played_at": "not-a-timestamp", "track": {"uri": "spotify:track:Invalid"}}]}),
        json!({"items": [item("spotify:track:Future", NOW + CLOCK_SKEW_SECS + 1)]}),
        json!({"items": [item("spotify:track:BeforeEpoch", -1)]}),
        json!({"items": [item("spotify:track:Bad/Id", NOW)]}),
        json!({"items": too_many}),
    ];
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = owned_store();
        let before = store.spotify_heard().unwrap();
        let mut session = server.session(&cx, RECENT_SCOPE);
        for page in pages {
            server.history(page);
            assert!(
                refresh_recent_listening(&mut session, &cx, &mut store, NOW)
                    .await
                    .is_err()
            );
            assert_eq!(store.spotify_heard().unwrap(), before);
        }
    });
    assert!(observation(&vec![b' '; MAX_PAGE_BYTES + 1], NOW).is_err());
}

#[test]
fn a_latest_fifty_window_reports_its_limit_without_following_a_continuation_url() {
    let server = Server::new();
    let tracks: Vec<_> = (0..RECENT_WINDOW)
        .map(|number| item(&format!("spotify:track:Window{number}"), NOW))
        .collect();
    server.history(json!({"items": tracks, "next": "https://outside.invalid/older-plays"}));
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = owned_store();
        let mut session = server.session(&cx, RECENT_SCOPE);
        let report = refresh_recent_listening(&mut session, &cx, &mut store, NOW)
            .await
            .unwrap();
        assert_eq!(
            (report.received, report.accepted),
            (RECENT_WINDOW, RECENT_WINDOW)
        );
        assert!(report.more_available);
        assert_eq!(store.spotify_heard().unwrap().len(), RECENT_WINDOW + 1);
        assert_eq!(server.calls(), ["/v1/me", HISTORY, "/v1/me"]);
    });
}

#[test]
fn server_rate_limits_pause_later_reads_without_erasing_heard_tracks() {
    let server = Server::new();
    server.state.lock().unwrap().status = 429;
    runtime().block_on(async {
        let cx = Cx::current().unwrap();
        let mut store = owned_store();
        let before = store.spotify_heard().unwrap();
        let mut session = server.session(&cx, RECENT_SCOPE);
        assert!(matches!(
            refresh_recent_listening(&mut session, &cx, &mut store, NOW).await,
            Err(SpotifyError::Api { status: 429, .. })
        ));
        assert!(session.retry_after_secs() >= 3590);
        let requests = server.calls();
        assert_eq!(requests, ["/v1/me", HISTORY]);
        assert!(
            refresh_recent_listening(&mut session, &cx, &mut store, NOW)
                .await
                .is_err()
        );
        assert_eq!(server.calls(), requests);
        assert_eq!(store.spotify_heard().unwrap(), before);
    });
}
