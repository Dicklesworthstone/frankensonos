//! Full-refresh history and current library intake and enrichment through real HTTP.
//! Status reads never schedule work or discover speakers; the write route
//! preserves the engine's cooldown response and existing policy boundary.

use asupersync::Cx;
use asupersync::http::Client as HttpClient;
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};
use fastapi::{ServerConfig, TcpServer};
use fsonos_api::dj::{DjEngine, DjMoodsDto, DjSpeakers, DjStatusDto, DjSteer};
use fsonos_api::plan::DjAction;
use fsonos_api::surface::dj_sync::{
    FollowedArtistsDto, FollowedArtistsPhase, GenreEnrichmentDto, LibrarySyncDto,
    SavedLibraryIntakeDto, SavedLibraryIntakePhase, SyncedDto,
};
use fsonos_api::{Failure, Identity, OutcomeDto, Surface, WebPolicy};
use fsonos_core::clock::{Clock, SystemClock};
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::store::Store;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::PlayerId;
use serde_json::Value;
use std::net::{IpAddr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::Duration;

struct NoLan;

impl Transport for NoLan {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        panic!("sync status must not contact a speaker")
    }
}

struct SyncProbe {
    status: Arc<Mutex<LibrarySyncDto>>,
    requests: Arc<AtomicUsize>,
}

impl DjEngine for SyncProbe {
    fn act(
        &self,
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        _: DjAction,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        panic!("sync status must not control playback")
    }

    fn steer(
        &self,
        _: DjSpeakers<'_>,
        _: &mut dyn Store,
        _: &DjSteer,
        _: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        panic!("sync status must not steer a DJ")
    }

    fn status(
        &self,
        _: DjSpeakers<'_>,
        _: &dyn Store,
        _: Option<u32>,
        _: &dyn Clock,
    ) -> Result<DjStatusDto, Failure> {
        panic!("sync status must not read playback")
    }

    fn moods(
        &self,
        _: Option<DjSpeakers<'_>>,
        _: &dyn Store,
        _: &dyn Clock,
    ) -> Result<DjMoodsDto, Failure> {
        panic!("sync status must not read DJ moods")
    }

    fn feeds(&self, _: &PlayerId) -> bool {
        false
    }

    fn on_playback(&self, _: DjSpeakers<'_>, _: &mut dyn Store, _: &PlayerPlayback, _: &dyn Clock) {
        panic!("sync status must not process playback")
    }

    fn sync_library(&self) -> Result<LibrarySyncDto, Failure> {
        self.requests.fetch_add(1, Ordering::SeqCst);
        self.library_sync()
    }

    fn library_sync(&self) -> Result<LibrarySyncDto, Failure> {
        Ok(self.status.lock().unwrap().clone())
    }
}

fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().unwrap())
        .blocking_threads(0, 4)
        .build()
        .unwrap()
}

fn state(genres: GenreEnrichmentDto) -> LibrarySyncDto {
    LibrarySyncDto::new(
        genres.running,
        Some(SyncedDto {
            at: 1_790_000_000,
            tracks: 100,
            candidates: 80,
            classical: 12,
            retired: 3,
            genre_artists_pending: 99,
            genre_stalled: 0,
            notes: Vec::new(),
        }),
        None,
    )
    .with_genre_enrichment(Some(genres))
}

fn waiting() -> GenreEnrichmentDto {
    GenreEnrichmentDto {
        pending: 12,
        completed: 7,
        retry_at: Some(1_790_000_900),
        not_before: Some(1_790_003_600),
        ..Default::default()
    }
}

struct Api {
    addr: SocketAddr,
    server: Arc<TcpServer>,
    thread: Option<JoinHandle<()>>,
    status: Arc<Mutex<LibrarySyncDto>>,
    requests: Arc<AtomicUsize>,
}

impl Api {
    fn start(client: Client) -> Self {
        let status = Arc::new(Mutex::new(state(waiting())));
        let requests = Arc::new(AtomicUsize::new(0));
        let surface = Arc::new(
            Surface::new(
                Box::new(NoLan),
                Box::new(|_| panic!("sync status must not discover speakers")),
                Policy::default(),
                Box::new(SystemClock),
            )
            .with_dj(Box::new(SyncProbe {
                status: Arc::clone(&status),
                requests: Arc::clone(&requests),
            })),
        );
        let web = WebPolicy::for_listener("127.0.0.1:0".parse().unwrap(), &[]);
        let app = Arc::new(fsonos_api::app(&surface, &Identity::fixed(client), &web));
        let config = ServerConfig::new("127.0.0.1:0").with_allowed_hosts(web.hosts().to_vec());
        let server = Arc::new(TcpServer::new(config));
        let (tx, rx) = mpsc::channel();
        let thread = {
            let serving = Arc::clone(&server);
            thread::spawn(move || {
                runtime().block_on(async move {
                    let cx = Cx::current().unwrap();
                    let listener = asupersync::net::TcpListener::bind("127.0.0.1:0")
                        .await
                        .unwrap();
                    tx.send(listener.local_addr().unwrap()).unwrap();
                    let _ = serving.serve_on_app(&cx, listener, app).await;
                });
            })
        };
        Self {
            addr: rx.recv_timeout(Duration::from_secs(10)).unwrap(),
            server,
            thread: Some(thread),
            status,
            requests,
        }
    }

    fn request(&self, method: &str, path: &str) -> (u16, Value) {
        runtime().block_on(async {
            let cx = Cx::current().unwrap();
            let client = HttpClient::default_for_runtime(&cx);
            let url = format!("http://{}{path}", self.addr);
            let request = if method == "POST" {
                client
                    .post(url)
                    .header("Content-Type", "application/json")
                    .body(b"{}".to_vec())
            } else {
                client.get(url)
            };
            let response = request.send(&cx).await.unwrap();
            (
                response.status,
                serde_json::from_slice(&response.body).unwrap(),
            )
        })
    }
}

impl Drop for Api {
    fn drop(&mut self) {
        self.server.shutdown();
        drop(std::net::TcpStream::connect(self.addr));
        self.thread.take().unwrap().join().unwrap();
    }
}

#[test]
fn sync_http_preserves_current_progress_and_last_full_refresh_separately() {
    let api = Api::start(Client::Cli);
    let expected = state(waiting());
    let (status, body) = api.request("GET", "/dj/sync");
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_value::<LibrarySyncDto>(body).unwrap(),
        expected
    );
    assert_eq!(
        api.requests.load(Ordering::SeqCst),
        0,
        "reads start no work"
    );

    let (status, body) = api.request("POST", "/dj/sync");
    assert_eq!(status, 200);
    assert_eq!(
        serde_json::from_value::<LibrarySyncDto>(body).unwrap(),
        expected
    );
    assert_eq!(api.requests.load(Ordering::SeqCst), 1);

    for genres in [
        GenreEnrichmentDto {
            running: true,
            pending: 5,
            ..Default::default()
        },
        GenreEnrichmentDto {
            completed: 5,
            ..Default::default()
        },
        GenreEnrichmentDto {
            requires_refresh: true,
            ..Default::default()
        },
    ] {
        let expected = state(genres);
        *api.status.lock().unwrap() = expected.clone();
        let (status, body) = api.request("GET", "/dj/sync");
        assert_eq!(status, 200);
        assert_eq!(body["last"]["at"], 1_790_000_000);
        assert_eq!(
            serde_json::from_value::<LibrarySyncDto>(body).unwrap(),
            expected
        );
    }
    assert_eq!(api.requests.load(Ordering::SeqCst), 1);
    let (status, spec) = api.request("GET", "/openapi.json");
    assert_eq!(status, 200);
    for method in ["get", "post"] {
        let schema = &spec["paths"]["/dj/sync"][method]["responses"]["200"]["content"]["application/json"]
            ["schema"];
        let schema = schema["$ref"]
            .as_str()
            .and_then(|reference| reference.rsplit('/').next())
            .map_or(schema, |name| &spec["components"]["schemas"][name]);
        assert!(
            schema["properties"].get("genre_enrichment").is_some(),
            "{schema}"
        );
        assert!(
            schema["properties"].get("library_intake").is_some(),
            "{schema}"
        );
        assert!(
            schema["properties"].get("followed_artists").is_some(),
            "{schema}"
        );
    }
}

#[test]
fn saved_intake_is_observable_before_first_publication_and_during_recovery() {
    let api = Api::start(Client::Unknown);
    let mut intake = SavedLibraryIntakeDto {
        pending: true,
        phase: SavedLibraryIntakePhase::Reading,
        pages: 512,
        tracks: 24_500,
        retry_at: Some(1_790_000_900),
        not_before: Some(1_790_003_600),
        ..Default::default()
    };
    for phase in [
        SavedLibraryIntakePhase::Reading,
        SavedLibraryIntakePhase::Verifying,
        SavedLibraryIntakePhase::Ready,
    ] {
        intake.phase = phase;
        intake.verified_pages = match phase {
            SavedLibraryIntakePhase::Reading => 0,
            SavedLibraryIntakePhase::Verifying => 100,
            SavedLibraryIntakePhase::Ready => intake.pages,
        };
        let expected =
            LibrarySyncDto::new(false, None, None).with_library_intake(Some(intake.clone()));
        *api.status.lock().unwrap() = expected.clone();
        let (status, body) = api.request("GET", "/dj/sync");
        assert_eq!(status, 200);
        assert!(body.get("last").is_none());
        assert!(body.get("error").is_none());
        assert_eq!(
            serde_json::from_value::<LibrarySyncDto>(body).unwrap(),
            expected
        );
        assert_eq!(api.requests.load(Ordering::SeqCst), 0);
    }
    let completed = state(GenreEnrichmentDto::default());
    *api.status.lock().unwrap() = completed.clone();
    let (_, body) = api.request("GET", "/dj/sync");
    assert!(body.get("library_intake").is_none());
    assert_eq!(
        serde_json::from_value::<LibrarySyncDto>(body).unwrap(),
        completed
    );
    assert_eq!(api.requests.load(Ordering::SeqCst), 0);
}

#[test]
fn followed_artist_progress_keeps_directory_completion_separate_from_library_publication() {
    let api = Api::start(Client::Unknown);
    let previous = state(GenreEnrichmentDto::default());
    for artists in [
        FollowedArtistsDto {
            pending: true,
            pages: 20,
            artists: 1_000,
            retry_at: Some(1_790_000_900),
            not_before: Some(1_790_003_600),
            ..Default::default()
        },
        FollowedArtistsDto {
            pending: true,
            phase: FollowedArtistsPhase::Verifying,
            pages: 21,
            artists: 1_025,
            verified_pages: 8,
            running: true,
            ..Default::default()
        },
        FollowedArtistsDto {
            pending: true,
            phase: FollowedArtistsPhase::Ready,
            pages: 21,
            artists: 1_025,
            verified_pages: 21,
            ..Default::default()
        },
        FollowedArtistsDto {
            pages: 20,
            artists: 1_000,
            limited: true,
            ..Default::default()
        },
    ] {
        let expected = LibrarySyncDto::new(artists.running, previous.last.clone(), None)
            .with_followed_artists(Some(artists));
        *api.status.lock().unwrap() = expected.clone();
        let (status, body) = api.request("GET", "/dj/sync");
        assert_eq!(status, 200);
        assert_eq!(body["last"]["at"], 1_790_000_000);
        assert_eq!(
            serde_json::from_value::<LibrarySyncDto>(body).unwrap(),
            expected
        );
        assert_eq!(api.requests.load(Ordering::SeqCst), 0);
    }
}

#[test]
fn sync_status_keeps_the_existing_read_and_write_policy_boundary() {
    let api = Api::start(Client::Unknown);
    assert_eq!(api.request("GET", "/dj/sync").0, 200);
    let (status, body) = api.request("POST", "/dj/sync");
    assert_eq!(status, 403);
    assert_eq!(body["code"], "POLICY_DENIED");
    assert_eq!(api.requests.load(Ordering::SeqCst), 0);
}
