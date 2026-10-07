//! The owner's live Spotify session: the PKCE authorization flow, the local
//! token cache, and authorized Web API reads over the asupersync HTTPS client.
//!
//! Everything protocol-shaped (URLs, bodies, PKCE, callback parsing, token
//! bookkeeping, paging) is pure logic in [`crate::client`] and
//! [`crate::library`]; this module is the thin I/O loop around it. It reads
//! only the owner's own library (scope `user-library-read`), sends the bearer
//! token only to the Web API, and never starts playback.

use std::fmt;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use asupersync::Cx;
use asupersync::http::Client;
use asupersync::time::{sleep, wall_now};

use crate::SpotifyError;
use crate::client::{
    CachedToken, Endpoints, FORM_CONTENT_TYPE, Pkce, SpotifyConfig, TokenCache, TokenResponse,
    api_error, parse_callback, random_state, retry_after_secs, token_error,
};
use crate::library::{LibraryItem, LibraryRead};

/// Per-request ceiling; the Cx budget can shorten it further.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Refresh the access token this long before it expires.
const REFRESH_MARGIN_SECS: i64 = 60;
/// How many 429s one GET waits out before giving up.
const MAX_RATE_LIMIT_WAITS: u32 = 3;

/// A started authorization: send the owner to [`Self::url`], then hand the
/// redirect they land on to [`Session::complete_authorization`]. Holds the
/// PKCE verifier and `state`, so it stays in memory and is never logged.
pub struct PendingAuthorization {
    url: String,
    pkce: Pkce,
    state: String,
}

impl PendingAuthorization {
    /// The Spotify consent page for `user-library-read`.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.url
    }
}

impl fmt::Debug for PendingAuthorization {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PendingAuthorization")
            .field("url", &self.url)
            .field("pkce", &self.pkce)
            .field("state", &"<redacted>")
            .finish()
    }
}

/// The owner's Spotify session.
pub struct Session {
    config: SpotifyConfig,
    endpoints: Endpoints,
    cache: TokenCache,
    http: Client,
    token: Option<CachedToken>,
}

impl fmt::Debug for Session {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("config", &self.config)
            .field("endpoints", &self.endpoints)
            .field("cache", &self.cache)
            .field("token", &self.token)
            .finish_non_exhaustive()
    }
}

impl Session {
    /// Open the session, picking up any token already in the cache.
    pub fn open(
        config: SpotifyConfig,
        cache: TokenCache,
        http: Client,
    ) -> Result<Self, SpotifyError> {
        config.validate()?;
        let token = cache.load()?;
        Ok(Self {
            config,
            endpoints: Endpoints::default(),
            cache,
            http,
            token,
        })
    }

    /// Talk to other hosts than Spotify's (tests use a loopback server).
    #[must_use]
    pub fn with_endpoints(mut self, endpoints: Endpoints) -> Self {
        self.endpoints = endpoints;
        self
    }

    /// Whether a token is cached (the owner has authorized this app).
    #[must_use]
    pub fn is_authorized(&self) -> bool {
        self.token.is_some()
    }

    /// Start the Authorization Code + PKCE flow.
    pub fn begin_authorization(&self) -> Result<PendingAuthorization, SpotifyError> {
        let pkce = Pkce::generate()?;
        let state = random_state()?;
        let url = self.config.authorize_url(&pkce, &state);
        Ok(PendingAuthorization { url, pkce, state })
    }

    /// Finish the flow with the redirect the owner's browser landed on (a
    /// request target, full URL, or bare query): check `state`, exchange the
    /// code, and cache the token.
    pub async fn complete_authorization(
        &mut self,
        cx: &Cx,
        pending: &PendingAuthorization,
        callback: &str,
    ) -> Result<(), SpotifyError> {
        let code = parse_callback(callback, &pending.state)?;
        let body = self.config.code_exchange_body(&code, &pending.pkce);
        let response = self.post_token(cx, body).await?;
        let token = CachedToken::from_exchange(response, unix_now())?;
        self.cache.store(&token)?;
        self.token = Some(token);
        Ok(())
    }

    /// A valid access token, refreshed (and re-cached) when it is about to
    /// expire.
    pub async fn access_token(&mut self, cx: &Cx) -> Result<String, SpotifyError> {
        let fresh = self
            .token
            .as_ref()
            .ok_or_else(not_authorized)?
            .is_fresh(unix_now(), REFRESH_MARGIN_SECS);
        if !fresh {
            self.refresh(cx).await?;
        }
        self.token
            .as_ref()
            .map(|t| t.access_token.clone())
            .ok_or_else(not_authorized)
    }

    /// GET a Web API URL with the owner's token. A 401 refreshes the token
    /// and retries once; a 429 waits out `Retry-After` (a few times); any
    /// other failure maps to [`SpotifyError::Api`].
    pub async fn get(&mut self, cx: &Cx, url: &str) -> Result<Vec<u8>, SpotifyError> {
        if !self.endpoints.is_api_url(url) {
            return Err(SpotifyError::Config(format!(
                "refusing to send the Spotify token outside the Web API: {url}"
            )));
        }
        let mut refreshed = false;
        let mut waits = 0;
        loop {
            let token = self.access_token(cx).await?;
            let response = self
                .http
                .get(url)
                .bearer_auth(&token)
                .header("Accept", "application/json")
                .timeout(REQUEST_TIMEOUT)
                .send(cx)
                .await
                .map_err(|e| SpotifyError::Http(e.to_string()))?;
            match response.status {
                200..=299 => return Ok(response.body),
                401 if !refreshed => {
                    refreshed = true;
                    self.refresh(cx).await?;
                }
                429 if waits < MAX_RATE_LIMIT_WAITS => {
                    waits += 1;
                    let secs = retry_after_secs(response.header_value("Retry-After"));
                    sleep(wall_now(), Duration::from_secs(secs)).await;
                }
                status => return Err(api_error(status, &response.body)),
            }
        }
    }

    /// Read the owner's whole library: every saved album's tracks and every
    /// liked track (read-only).
    pub async fn read_library(&mut self, cx: &Cx) -> Result<Vec<LibraryItem>, SpotifyError> {
        let mut read = LibraryRead::new(&self.endpoints);
        while let Some(url) = read.next_url().map(str::to_owned) {
            let body = self.get(cx, &url).await?;
            read.ingest(&body)?;
        }
        Ok(read.into_items())
    }

    async fn refresh(&mut self, cx: &Cx) -> Result<(), SpotifyError> {
        let current = self.token.as_ref().ok_or_else(not_authorized)?;
        let body = self.config.refresh_body(&current.refresh_token);
        let response = self.post_token(cx, body).await?;
        let next = current.refreshed(response, unix_now())?;
        // Persist first: a rotated refresh token must not be lost.
        self.cache.store(&next)?;
        self.token = Some(next);
        Ok(())
    }

    async fn post_token(&self, cx: &Cx, body: String) -> Result<TokenResponse, SpotifyError> {
        let response = self
            .http
            .post(self.endpoints.token.as_str())
            .header("Content-Type", FORM_CONTENT_TYPE)
            .body(body)
            .timeout(REQUEST_TIMEOUT)
            .send(cx)
            .await
            .map_err(|e| SpotifyError::Http(e.to_string()))?;
        if response.is_success() {
            TokenResponse::parse(&response.body)
        } else {
            Err(token_error(response.status, &response.body))
        }
    }
}

fn not_authorized() -> SpotifyError {
    SpotifyError::Auth("not authorized yet: run the Spotify login first".into())
}

fn unix_now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

#[cfg(test)]
mod tests {
    //! Against a fake Spotify (accounts + Web API) served by a real asupersync
    //! `Http1Listener` on loopback: no mocks of the client, no network
    //! beyond 127.0.0.1.

    use std::net::SocketAddr;
    use std::path::PathBuf;
    use std::sync::{Arc, Mutex, mpsc};
    use std::thread;

    use asupersync::http::h1::server::HostPolicy;
    use asupersync::http::h1::types::{Method, Request, Response};
    use asupersync::http::h1::{Http1Config, Http1Listener, Http1ListenerConfig};
    use asupersync::runtime::{Runtime, RuntimeBuilder, reactor::create_reactor};

    use super::*;
    use crate::client::{SCOPE, base64url, parse_query, sha256};

    const CLIENT_ID: &str = "0123456789abcdef0123456789abcdef";
    const REDIRECT: &str = "http://127.0.0.1:8099/auth/spotify/callback";

    fn runtime() -> Runtime {
        RuntimeBuilder::current_thread()
            .with_reactor(create_reactor().expect("reactor"))
            .blocking_threads(0, 4)
            .build()
            .expect("runtime")
    }

    /// What the fake Spotify has seen and will accept.
    #[derive(Default)]
    struct Fake {
        base: String,
        expected_challenge: Option<String>,
        access: String,
        refresh: String,
        refreshes: usize,
        rate_limit_tracks_once: bool,
        log: Vec<String>,
    }

    fn json(status: u16, body: impl Into<Vec<u8>>) -> Response {
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
            json(
                200,
                rewrite(include_bytes!("../tests/fixtures/saved_tracks_page.json")),
            )
        } else if uri.starts_with("/v1/albums/FakeAlbum0000000000002/tracks") {
            json(
                200,
                r#"{"items":[{"artists":[{"name":"Frédéric Chopin"}],"duration_ms":330000,
                    "id":"FakeTrack0000000000008","is_playable":true,
                    "name":"Nocturnes, Op. 48: No. 1 in C Minor",
                    "uri":"spotify:track:FakeTrack0000000000008"}],
                    "next":null,"offset":50,"limit":50,"total":60}"#,
            )
        } else {
            json(404, r#"{"error":{"status":404,"message":"Not found"}}"#)
        }
    }

    /// A fake Spotify on a loopback port, served from its own thread.
    struct FakeSpotify {
        addr: SocketAddr,
        state: Arc<Mutex<Fake>>,
        shutdown: Box<dyn FnOnce()>,
        thread: thread::JoinHandle<()>,
    }

    impl FakeSpotify {
        fn start() -> Self {
            let state = Arc::new(Mutex::new(Fake::default()));
            let shared = Arc::clone(&state);
            let config = Http1ListenerConfig::default().http_config(
                Http1Config::default()
                    .host_policy(HostPolicy::allow_list(vec!["127.0.0.1".into()])),
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

        fn endpoints(&self) -> Endpoints {
            Endpoints {
                token: format!("http://{}/api/token", self.addr),
                api: format!("http://{}/v1", self.addr),
            }
        }

        fn stop(self) -> Fake {
            (self.shutdown)();
            self.thread.join().expect("server thread");
            Arc::try_unwrap(self.state)
                .ok()
                .expect("server released its state")
                .into_inner()
                .unwrap()
        }
    }

    fn scratch_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "fsonos-spotify-session-{}-{name}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn config() -> SpotifyConfig {
        SpotifyConfig {
            client_id: CLIENT_ID.into(),
            redirect_uri: REDIRECT.into(),
        }
    }

    fn query_param(url: &str, key: &str) -> String {
        let query = url.split_once('?').unwrap().1;
        parse_query(query)
            .unwrap()
            .into_iter()
            .find(|(k, _)| k == key)
            .unwrap()
            .1
    }

    #[test]
    fn authorize_refresh_and_read_the_library() {
        let spotify = FakeSpotify::start();
        let data_dir = scratch_dir("full-flow");
        let cache = TokenCache::in_data_dir(&data_dir);
        let endpoints = spotify.endpoints();
        let state = Arc::clone(&spotify.state);

        let items = runtime().block_on(async move {
            let cx = Cx::current().expect("ambient Cx");
            let http = Client::default_for_runtime(&cx);
            let mut session = Session::open(config(), cache.clone(), http)
                .unwrap()
                .with_endpoints(endpoints);
            assert!(!session.is_authorized());
            assert!(
                session.access_token(&cx).await.is_err(),
                "no token before login"
            );

            // The owner opens the consent URL; Spotify redirects back.
            let pending = session.begin_authorization().unwrap();
            assert_eq!(query_param(pending.url(), "scope"), SCOPE);
            assert_eq!(query_param(pending.url(), "code_challenge_method"), "S256");
            state.lock().unwrap().expected_challenge =
                Some(query_param(pending.url(), "code_challenge"));
            let st = query_param(pending.url(), "state");

            // A forged redirect (wrong state) is refused before any exchange.
            let forged = format!("/auth/spotify/callback?code=good-code&state=x{st}");
            assert!(
                session
                    .complete_authorization(&cx, &pending, &forged)
                    .await
                    .is_err()
            );
            assert!(!session.is_authorized());

            let callback = format!("{REDIRECT}?code=good-code&state={st}");
            session
                .complete_authorization(&cx, &pending, &callback)
                .await
                .unwrap();
            assert!(session.is_authorized());
            assert_eq!(cache.load().unwrap().unwrap().refresh_token, "refresh-1");

            // The server revokes the access token: the next read gets a 401,
            // refreshes (rotating the refresh token), and retries. Liked
            // tracks are also rate-limited once.
            "revoked".clone_into(&mut state.lock().unwrap().access);
            state.lock().unwrap().rate_limit_tracks_once = true;
            let items = session.read_library(&cx).await.unwrap();
            let cached = cache.load().unwrap().unwrap();
            assert_eq!(
                (cached.access_token.as_str(), cached.refresh_token.as_str()),
                ("access-r1", "refresh-r1")
            );

            // The bearer token never leaves the Web API.
            let err = session
                .get(&cx, "https://example.invalid/v1/me")
                .await
                .unwrap_err();
            assert!(err.to_string().contains("outside the Web API"), "{err}");
            items
        });

        let fake = spotify.stop();
        assert_eq!(fake.refreshes, 1);
        let titles: Vec<&str> = items.iter().map(|i| i.title.as_str()).collect();
        assert_eq!(titles.len(), 6, "{titles:?}");
        assert!(titles.contains(&"Suite bergamasque, L. 75: III. Clair de lune"));
        assert!(titles.contains(&"Nocturnes, Op. 48: No. 1 in C Minor"));
        let token_posts = fake
            .log
            .iter()
            .filter(|l| l.ends_with("/api/token"))
            .count();
        assert_eq!(
            token_posts, 2,
            "one bad-state attempt never reached the server: {:?}",
            fake.log
        );
        let track_gets = fake
            .log
            .iter()
            .filter(|l| l.contains("/v1/me/tracks"))
            .count();
        assert_eq!(track_gets, 2, "429 then success: {:?}", fake.log);
        std::fs::remove_dir_all(&data_dir).unwrap();
    }

    #[test]
    fn expired_token_refreshes_before_the_request_and_bad_refresh_surfaces() {
        let spotify = FakeSpotify::start();
        let data_dir = scratch_dir("expiry");
        let cache = TokenCache::in_data_dir(&data_dir);
        {
            let mut fake = spotify.state.lock().unwrap();
            "refresh-1".clone_into(&mut fake.refresh);
        }
        // A cached token that expired an hour ago.
        cache
            .store(&CachedToken {
                access_token: "access-old".into(),
                refresh_token: "refresh-1".into(),
                expires_at: unix_now() - 3600,
                scope: SCOPE.into(),
            })
            .unwrap();
        let endpoints = spotify.endpoints();
        let state = Arc::clone(&spotify.state);

        runtime().block_on(async move {
            let cx = Cx::current().expect("ambient Cx");
            let http = Client::default_for_runtime(&cx);
            let mut session = Session::open(config(), cache.clone(), http)
                .unwrap()
                .with_endpoints(endpoints.clone());
            assert!(session.is_authorized(), "picked up from the cache");
            assert_eq!(session.access_token(&cx).await.unwrap(), "access-r1");
            let body = session.get(&cx, &endpoints.saved_albums(50)).await.unwrap();
            assert!(String::from_utf8(body).unwrap().contains("\"items\":[]"));

            // Not found is an API error, not a retry loop.
            let err = session
                .get(&cx, &format!("{}/nope", endpoints.api))
                .await
                .unwrap_err();
            assert!(
                matches!(err, SpotifyError::Api { status: 404, .. }),
                "{err}"
            );

            // The owner revoked the app: refresh fails with Spotify's reason.
            "someone-else".clone_into(&mut state.lock().unwrap().refresh);
            "revoked".clone_into(&mut state.lock().unwrap().access);
            let err = session
                .get(&cx, &endpoints.saved_albums(50))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("invalid_grant"), "{err}");
        });

        let fake = spotify.stop();
        assert_eq!(fake.refreshes, 1);
        std::fs::remove_dir_all(&data_dir).unwrap();
    }
}
