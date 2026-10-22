//! Ordered cached playlist observations through the real HTTP listener.
//! Browsing needs neither a Spotify grant nor discovery or speaker I/O.

use asupersync::Cx;
use asupersync::http::Client as HttpClient;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::surface::dj_sync::playlist::PlaylistPage;
use fsonos_api::{Identity, Surface, WebPolicy};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::{CachedPlaylist, MemStore, PlaylistTrack, Store};
use serde_json::{Value, json};
use std::net::SocketAddr;
use std::sync::{Arc, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

const PLAYLIST: &str = "spotify:playlist:Ordered";
const FETCHED_AT: i64 = 1_790_000_000;

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().unwrap())
        .blocking_threads(0, 4)
        .build()
        .unwrap()
}

struct NoLan;

impl fsonos_proto::Transport for NoLan {
    fn soap_post(
        &self,
        _: std::net::IpAddr,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<String, fsonos_proto::ProtoError> {
        panic!("cached playlist browsing must not contact a speaker");
    }
}

struct Api {
    addr: SocketAddr,
    server: Arc<TcpServer>,
    worker: Option<JoinHandle<()>>,
}

impl Api {
    fn start(store: MemStore, policy: Policy) -> Self {
        let surface = Surface::new(
            Box::new(NoLan),
            Box::new(|_| panic!("cached playlist browsing must not discover speakers")),
            policy,
            Box::new(SystemClock),
        )
        .with_action_log(Box::new(store), "playlist-http");
        let web = WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]);
        let app = Arc::new(fsonos_api::app(
            &Arc::new(surface),
            &Identity::fixed(Client::LoopbackHttp),
            &web,
        ));
        let server = Arc::new(TcpServer::new(
            ServerConfig::new("127.0.0.1:0").with_allowed_hosts(web.hosts().to_vec()),
        ));
        let (tx, rx) = mpsc::channel();
        let running = Arc::clone(&server);
        let worker = thread::spawn(move || {
            runtime().block_on(async move {
                let cx = Cx::current().unwrap();
                let listener = asupersync::net::TcpListener::bind("127.0.0.1:0")
                    .await
                    .unwrap();
                tx.send(listener.local_addr().unwrap()).unwrap();
                let _ = running.serve_on_app(&cx, listener, app).await;
            });
        });
        Self {
            addr: rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            server,
            worker: Some(worker),
        }
    }

    fn get(&self, path: &str) -> (u16, Value) {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let response = HttpClient::default_for_runtime(&cx)
                .get(format!("http://{}{path}", self.addr))
                .send(&cx)
                .await
                .unwrap();
            (
                response.status,
                serde_json::from_slice(&response.body).unwrap(),
            )
        })
    }

    fn page(&self, path: &str) -> PlaylistPage {
        let (status, body) = self.get(path);
        assert_eq!(status, 200, "{path}: {body}");
        serde_json::from_value(body).unwrap()
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.server.shutdown();
        drop(std::net::TcpStream::connect(self.addr));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

fn path(id: &str, suffix: &str) -> String {
    format!("/library/playlist?playlist_uri=spotify%3Aplaylist%3A{id}{suffix}")
}

fn track(position: u32, id: &str, title: &str) -> PlaylistTrack {
    PlaylistTrack {
        position,
        source_uri: format!("spotify:track:{id}"),
        title: title.into(),
        artist: Some("Élodie & the Ink Quartet".into()),
        album: Some("Silver sessions".into()),
        album_uri: Some("spotify:album:Silver".into()),
        disc_number: Some(2),
        track_number: Some(7),
        duration_secs: Some(181),
        explicit: id == "Repeated",
    }
}

fn tracks() -> Vec<PlaylistTrack> {
    vec![
        track(1, "Repeated", "Zinc & lights"),
        track(4, "Repeated", "Zinc & lights"),
        track(6, "Middle", "Copper interlude"),
        track(9, "Last", "Amber finale"),
    ]
}

fn observation(at: i64, tracks: Vec<PlaylistTrack>) -> CachedPlaylist {
    CachedPlaylist::new(PLAYLIST.into(), at, Some(12), 9, false, tracks)
}

fn store(contents: &CachedPlaylist) -> MemStore {
    let mut store = MemStore::default();
    store
        .save_playlists(&[
            (PLAYLIST.into(), "Ordered listening".into()),
            ("spotify:playlist:Empty".into(), "Observed empty".into()),
            ("spotify:playlist:Unread".into(), "Not yet read".into()),
        ])
        .unwrap();
    store.save_playlist_contents(contents).unwrap();
    for contents in [
        CachedPlaylist::new(
            "spotify:playlist:Empty".into(),
            FETCHED_AT,
            Some(0),
            0,
            true,
            Vec::new(),
        ),
        CachedPlaylist::new(
            "spotify:playlist:Retained".into(),
            FETCHED_AT,
            Some(1),
            1,
            true,
            vec![track(1, "RetainedTrack", "An earlier observation")],
        ),
    ] {
        store.save_playlist_contents(&contents).unwrap();
    }
    assert!(store.library().unwrap().is_empty());
    store
}

fn api() -> Api {
    Api::start(store(&observation(FETCHED_AT, tracks())), Policy::default())
}

#[test]
fn http_pages_keep_source_order_duplicates_gaps_and_metadata() {
    let api = api();
    let first = api.page(&path("Ordered", "&limit=2"));
    assert_eq!(first.playlist_uri, PLAYLIST);
    assert_eq!(first.name, "Ordered listening");
    assert_eq!(first.q, "");
    assert_eq!(first.coverage, "partial");
    assert_eq!(first.fetched_at, Some(FETCHED_AT));
    assert_eq!((first.source_total, first.examined), (Some(12), 9));
    assert_eq!((first.cached_total, first.total), (4, 4));
    assert_eq!(
        (first.offset, first.limit, first.next_offset),
        (0, 2, Some(2))
    );
    assert_eq!(
        first
            .items
            .iter()
            .map(|item| item.position)
            .collect::<Vec<_>>(),
        [1, 4]
    );
    assert_eq!(first.items[0].source_uri, first.items[1].source_uri);
    assert_eq!(
        serde_json::to_value(&first.items[0]).unwrap(),
        json!({
            "position": 1, "source_uri": "spotify:track:Repeated",
            "title": "Zinc & lights", "subtitle": "Élodie & the Ink Quartet",
            "album": "Silver sessions", "album_uri": "spotify:album:Silver",
            "disc_number": 2, "track_number": 7, "duration_secs": 181,
            "explicit": true
        })
    );

    let revision = first.revision.as_deref().unwrap();
    let last = api.page(&path(
        "Ordered",
        &format!("&offset=2&limit=2&expected_revision={revision}"),
    ));
    assert_eq!(last.revision, first.revision);
    assert_eq!((last.offset, last.next_offset), (2, None));
    assert_eq!((last.cached_total, last.total), (4, 4));
    assert_eq!(
        last.items
            .iter()
            .map(|item| item.position)
            .collect::<Vec<_>>(),
        [6, 9]
    );
    // Exhausting this bounded cache does not claim the source was fully read.
    assert_eq!(last.coverage, "partial");
    assert_eq!((last.source_total, last.examined), (Some(12), 9));
}

#[test]
fn filters_match_metadata_without_losing_occurrences_or_coverage() {
    let api = api();
    let first = api.page(&path("Ordered", "&q=++ELODIE+lights+silver++&limit=1"));
    assert_eq!(first.q, "ELODIE lights silver");
    assert_eq!((first.cached_total, first.total), (4, 2));
    assert_eq!(first.items[0].position, 1);
    assert_eq!(first.next_offset, Some(1));
    let revision = first.revision.as_deref().unwrap();
    let last = api.page(&path(
        "Ordered",
        &format!("&q=ELODIE+lights+silver&offset=1&limit=1&expected_revision={revision}"),
    ));
    assert_eq!(last.items[0].position, 4);
    assert_eq!(last.items[0].source_uri, first.items[0].source_uri);
    assert_eq!(last.next_offset, None);
    assert_eq!(last.revision, first.revision);
    let absent = api.page(&path("Ordered", "&q=unmatched"));
    assert!(absent.items.is_empty());
    assert_eq!((absent.cached_total, absent.total), (4, 0));
    assert_eq!(absent.coverage, "partial");
    assert_eq!((absent.source_total, absent.examined), (Some(12), 9));
    assert!(absent.revision.is_some());
}

#[test]
fn changed_observations_reject_previous_revisions_until_the_caller_restarts() {
    let before = api();
    let first = before.page(&path("Ordered", "&limit=2"));
    let old_revision = first.revision.unwrap();
    drop(before);

    // Reopen with the refreshed observation, as after a daemon restart.
    // No previous suffix can be combined with the new cached prefix.
    let mut changed = tracks();
    changed[3].title = "A replacement finale".into();
    let after = Api::start(
        store(&observation(FETCHED_AT + 1, changed)),
        Policy::default(),
    );
    for offset in [0, 2] {
        let (status, failure) = after.get(&path(
            "Ordered",
            &format!("&offset={offset}&limit=2&expected_revision={old_revision}"),
        ));
        assert_eq!(status, 422, "{failure}");
        assert_eq!(failure["code"], "INVALID_ARGUMENT");
        assert!(failure["hint"].as_str().is_some_and(|hint| {
            hint.contains("offset") || hint.to_lowercase().contains("restart")
        }));
        assert!(failure.get("items").is_none());
    }
    let fresh = after.page(&path("Ordered", ""));
    assert_ne!(fresh.revision.as_deref(), Some(old_revision.as_str()));
    assert_eq!(fresh.fetched_at, Some(FETCHED_AT + 1));
    assert_eq!(fresh.items[3].title, "A replacement finale");
}

#[test]
fn observed_empty_unread_retained_and_unknown_playlists_are_distinct() {
    let api = api();
    let empty = api.page(&path("Empty", ""));
    assert_eq!(empty.coverage, "complete");
    assert_eq!(empty.fetched_at, Some(FETCHED_AT));
    assert_eq!((empty.source_total, empty.examined), (Some(0), 0));
    assert_eq!((empty.cached_total, empty.total), (0, 0));
    assert!(empty.revision.is_some());
    assert!(empty.items.is_empty());
    assert!(empty.next_offset.is_none());

    let unread = api.page(&path("Unread", ""));
    assert_eq!(unread.name, "Not yet read");
    assert_eq!(unread.coverage, "not_cached");
    assert_eq!((unread.source_total, unread.examined), (None, 0));
    assert_eq!((unread.cached_total, unread.total), (0, 0));
    assert!(unread.fetched_at.is_none());
    assert!(unread.revision.is_none());
    assert!(unread.items.is_empty());
    assert!(unread.next_offset.is_none());

    let retained = api.page(&path("Retained", ""));
    assert_eq!(retained.name, "spotify:playlist:Retained");
    assert_eq!(retained.coverage, "complete");
    assert_eq!(retained.items[0].source_uri, "spotify:track:RetainedTrack");
    let (status, failure) = api.get(&path("Unknown", ""));
    assert_eq!(status, 404, "{failure}");
    assert_eq!(failure["code"], "NO_MATCH");
}

#[test]
fn malformed_queries_are_rejected_and_the_filter_bound_is_enforced() {
    let api = api();
    for suffix in [
        "&limit=0",
        "&limit=51",
        "&limit=many",
        "&offset=-1",
        "&offset=many",
        "&offset=1",
        "&offset=1&expected_revision=",
        "&q=%FF",
        "&q=one&q=two",
        "&limit=1&limit=2",
        "&offset=0&offset=0",
        "&expected_revision=one&expected_revision=two",
        "&playlist_uri=spotify%3Aplaylist%3AEmpty",
        "&unknown=1",
    ] {
        let (status, failure) = api.get(&path("Ordered", suffix));
        assert_eq!(status, 422, "{suffix}: {failure}");
        assert_eq!(failure["code"], "INVALID_ARGUMENT");
    }
    for path in [
        "/library/playlist",
        "/library/playlist?playlist_uri=",
        "/library/playlist?playlist_uri=spotify%3Aalbum%3AOrdered",
    ] {
        let (status, failure) = api.get(path);
        assert_eq!(status, 422, "{path}: {failure}");
    }
    let (status, failure) = api.get(&path("Ordered", &format!("&q={}", "x".repeat(513))));
    assert_eq!(status, 422, "{failure}");
    let boundary = api.page(&path("Ordered", &format!("&q={}", "x".repeat(512))));
    assert_eq!(boundary.q.len(), 512);
    assert_eq!(boundary.total, 0);
}

#[test]
fn playlist_contents_use_library_read_permission_and_describe_the_http_contract() {
    let policy = Policy::from_toml("[clients.loopback-http]\ndeny = ['browse_library']").unwrap();
    let denied = Api::start(store(&observation(FETCHED_AT, tracks())), policy);
    let (status, failure) = denied.get(&path("Ordered", ""));
    assert_eq!(status, 403, "{failure}");
    assert_eq!(failure["code"], "POLICY_DENIED");
    drop(denied);

    let api = api();
    let (status, spec) = api.get("/openapi.json");
    assert_eq!(status, 200);
    let operation = &spec["paths"]["/library/playlist"]["get"];
    assert_eq!(operation["operationId"], "playlist_contents");
    let parameters = operation["parameters"].as_array().unwrap();
    let mut names = parameters
        .iter()
        .filter_map(|parameter| parameter["name"].as_str())
        .collect::<Vec<_>>();
    names.sort_unstable();
    assert_eq!(
        names,
        ["expected_revision", "limit", "offset", "playlist_uri", "q"]
    );
    assert!(
        parameters.iter().any(|parameter| {
            parameter["name"] == "playlist_uri" && parameter["required"] == true
        })
    );
    assert!(!operation["responses"]["200"].is_null());
}
