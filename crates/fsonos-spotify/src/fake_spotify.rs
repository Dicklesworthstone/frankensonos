//! A fake Spotify (accounts + Web API) served by a real asupersync
//! `Http1Listener` on loopback, for tests of the I/O half of the client: no
//! mocks of the HTTP client, no network beyond 127.0.0.1. It verifies PKCE
//! server-side, rotates tokens on refresh, can rate-limit once, and serves
//! the library and album-track fixtures with paging links rewritten to
//! itself.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread;
use std::time::Duration;

use asupersync::http::h1::server::HostPolicy;
use asupersync::http::h1::types::{Method, Request, Response};
use asupersync::http::h1::{Http1Config, Http1Listener, Http1ListenerConfig};
use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};

use crate::client::{Endpoints, SCOPE, SpotifyConfig, base64url, parse_query, sha256};

pub(crate) const CLIENT_ID: &str = "0123456789abcdef0123456789abcdef";
pub(crate) const REDIRECT: &str = "http://127.0.0.1:8099/auth/spotify/callback";

pub(crate) fn runtime() -> Runtime {
    RuntimeBuilder::current_thread()
        .with_reactor(create_reactor().expect("reactor"))
        .blocking_threads(0, 4)
        .build()
        .expect("runtime")
}

/// What the fake Spotify has seen and will accept.
#[derive(Default)]
pub(crate) struct Fake {
    pub(crate) base: String,
    pub(crate) expected_challenge: Option<String>,
    pub(crate) access: String,
    pub(crate) refresh: String,
    pub(crate) refreshes: usize,
    pub(crate) rate_limit_tracks_once: bool,
    /// Serve this liked-tracks page instead of the fixture.
    pub(crate) liked_tracks: Option<String>,
    /// Answer this many album-tracks requests (any album) with a 429.
    pub(crate) rate_limit_albums: u32,
    pub(crate) log: Vec<String>,
}

pub(crate) fn json(status: u16, body: impl Into<Vec<u8>>) -> Response {
    Response::new(status, "", body).with_header("Content-Type", "application/json")
}

fn token_json(access: &str, refresh: Option<&str>) -> Response {
    let mut body = serde_json::json!({
        "access_token": access, "token_type": "Bearer", "scope": SCOPE, "expires_in": 3600,
    });
    if let Some(refresh) = refresh {
        body["refresh_token"] = refresh.into();
    }
    json(200, body.to_string())
}

fn respond(fake: &Mutex<Fake>, req: &Request) -> Response {
    let mut fake = fake.lock().unwrap();
    fake.log.push(format!("{:?} {}", req.method, req.uri));
    if req.method == Method::Post && req.uri == "/api/token" {
        let form = parse_query(std::str::from_utf8(&req.body).unwrap()).unwrap();
        let get = |k: &str| {
            form.iter()
                .find(|(key, _)| key == k)
                .map(|(_, v)| v.as_str())
        };
        if get("client_id") != Some(CLIENT_ID) {
            return json(400, r#"{"error":"invalid_client"}"#);
        }
        return match get("grant_type") {
            Some("authorization_code") => {
                // Real PKCE check: S256(verifier) must equal the challenge
                // from the authorize URL.
                let challenge = get("code_verifier").map(|v| base64url(&sha256(v.as_bytes())));
                if get("code") != Some("good-code")
                    || get("redirect_uri") != Some(REDIRECT)
                    || challenge != fake.expected_challenge
                {
                    return json(400, r#"{"error":"invalid_grant"}"#);
                }
                "access-1".clone_into(&mut fake.access);
                "refresh-1".clone_into(&mut fake.refresh);
                token_json("access-1", Some("refresh-1"))
            }
            Some("refresh_token") if get("refresh_token") == Some(fake.refresh.as_str()) => {
                fake.refreshes += 1;
                fake.access = format!("access-r{}", fake.refreshes);
                fake.refresh = format!("refresh-r{}", fake.refreshes);
                token_json(&fake.access.clone(), Some(&fake.refresh.clone()))
            }
            _ => json(
                400,
                r#"{"error":"invalid_grant","error_description":"Invalid refresh token"}"#,
            ),
        };
    }
    if req.header_value("authorization") != Some(format!("Bearer {}", fake.access).as_str()) {
        return json(
            401,
            r#"{"error":{"status":401,"message":"The access token expired"}}"#,
        );
    }
    let rewrite = |body: &[u8]| {
        String::from_utf8(body.to_vec())
            .unwrap()
            .replace("https://api.spotify.com/v1", &fake.base)
    };
    let uri = req.uri.as_str();
    if uri.starts_with("/v1/me/albums") && uri.contains("offset=0") {
        json(
            200,
            rewrite(include_bytes!("../tests/fixtures/saved_albums_page.json")),
        )
    } else if uri.starts_with("/v1/me/albums") {
        json(
            200,
            r#"{"items":[],"next":null,"offset":50,"limit":50,"total":51}"#,
        )
    } else if uri.starts_with("/v1/me/tracks") {
        if fake.rate_limit_tracks_once {
            fake.rate_limit_tracks_once = false;
            return json(429, "").with_header("Retry-After", "1");
        }
        if let Some(page) = &fake.liked_tracks {
            return json(200, page.clone());
        }
        json(
            200,
            rewrite(include_bytes!("../tests/fixtures/saved_tracks_page.json")),
        )
    } else if uri.starts_with("/v1/albums/") && fake.rate_limit_albums > 0 {
        fake.rate_limit_albums -= 1;
        json(429, "").with_header("Retry-After", "1")
    } else if uri.starts_with("/v1/albums/") {
        album_tracks(uri, rewrite)
    } else {
        not_found()
    }
}

fn not_found() -> Response {
    json(404, r#"{"error":{"status":404,"message":"Not found"}}"#)
}

/// `GET /v1/albums/{id}/tracks` for the fake's albums.
fn album_tracks(uri: &str, rewrite: impl Fn(&[u8]) -> String) -> Response {
    if uri.starts_with("/v1/albums/FakeAlbum0000000000009/tracks") {
        let page: &[u8] = if uri.contains("offset=3") {
            include_bytes!("../tests/fixtures/album_tracks_page2.json")
        } else {
            include_bytes!("../tests/fixtures/album_tracks_page1.json")
        };
        json(200, rewrite(page))
    } else if uri.starts_with("/v1/albums/FakeAlbum0000000000002/tracks") {
        json(
            200,
            r#"{"items":[{"artists":[{"name":"Frédéric Chopin"}],"duration_ms":330000,
                "id":"FakeTrack0000000000008","is_playable":true,
                "name":"Nocturnes, Op. 48: No. 1 in C Minor",
                "uri":"spotify:track:FakeTrack0000000000008"}],
                "next":null,"offset":50,"limit":50,"total":60}"#,
        )
    } else if uri.starts_with("/v1/albums/FakeAlbumUnplayable001/tracks") {
        json(
            200,
            r#"{"items":[{"artists":[{"name":"Gustav Mahler"}],"duration_ms":604000,
                "id":"FakeUnplayable00000002","is_playable":false,
                "name":"Symphony No. 5 in C-Sharp Minor: V. Rondo-Finale",
                "uri":"spotify:track:FakeUnplayable00000002"}],
                "next":null,"offset":0,"limit":50,"total":1}"#,
        )
    } else {
        not_found()
    }
}

/// A fake Spotify on a loopback port, served from its own thread.
pub(crate) struct FakeSpotify {
    pub(crate) addr: SocketAddr,
    pub(crate) state: Arc<Mutex<Fake>>,
    shutdown: Box<dyn FnOnce()>,
    thread: thread::JoinHandle<()>,
}

impl FakeSpotify {
    pub(crate) fn start() -> Self {
        let state = Arc::new(Mutex::new(Fake::default()));
        let shared = Arc::clone(&state);
        let config = Http1ListenerConfig::default().http_config(
            Http1Config::default().host_policy(HostPolicy::allow_list(vec!["127.0.0.1".into()])),
        );
        let (ready_tx, ready_rx) = mpsc::channel();
        let thread = thread::spawn(move || {
            let rt = runtime();
            let handle = rt.handle();
            rt.block_on(async move {
                let listener = Http1Listener::bind_with_config(
                    "127.0.0.1:0",
                    move |req: Request| {
                        let shared = Arc::clone(&shared);
                        async move { respond(&shared, &req) }
                    },
                    config,
                )
                .await
                .expect("bind loopback listener");
                let addr = listener.local_addr().expect("local addr");
                ready_tx
                    .send((addr, listener.shutdown_signal()))
                    .expect("report addr");
                listener.run(&handle).await.expect("listener run");
            });
        });
        let (addr, signal) = ready_rx
            .recv_timeout(Duration::from_secs(10))
            .expect("fake Spotify ready");
        state.lock().unwrap().base = format!("http://{addr}/v1");
        Self {
            addr,
            state,
            shutdown: Box::new(move || signal.trigger_immediate()),
            thread,
        }
    }

    pub(crate) fn endpoints(&self) -> Endpoints {
        Endpoints {
            token: format!("http://{}/api/token", self.addr),
            api: format!("http://{}/v1", self.addr),
        }
    }

    pub(crate) fn stop(self) -> Fake {
        (self.shutdown)();
        self.thread.join().expect("server thread");
        Arc::try_unwrap(self.state)
            .ok()
            .expect("server released its state")
            .into_inner()
            .unwrap()
    }
}

pub(crate) fn scratch_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "fsonos-spotify-session-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

pub(crate) fn config() -> SpotifyConfig {
    SpotifyConfig {
        client_id: CLIENT_ID.into(),
        redirect_uri: REDIRECT.into(),
    }
}

pub(crate) fn query_param(url: &str, key: &str) -> String {
    let query = url.split_once('?').unwrap().1;
    parse_query(query)
        .unwrap()
        .into_iter()
        .find(|(k, _)| k == key)
        .unwrap()
        .1
}
