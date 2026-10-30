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

use crate::SpotifyError;
use crate::client::{
    Artist, CachedToken, Endpoints, FORM_CONTENT_TYPE, Pkce, SpotifyConfig, TokenCache,
    TokenResponse, missing_taste_scopes, parse_callback, random_state, token_error,
};
use crate::library::{LibraryItem, LibraryRead};

mod retry;

/// Per-request ceiling; the Cx budget can shorten it further.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
/// Refresh the access token this long before it expires.
const REFRESH_MARGIN_SECS: i64 = 60;
/// The longest rate limit a background read waits out ([`Session::get_brief`]).
pub const BRIEF_WAIT_SECS: u64 = 5;

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

/// What [`Session::check_grant`] found: the grant works.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GrantCheck {
    /// The access token had to be refreshed (and was re-cached) first.
    pub refreshed: bool,
    /// Seconds the access token is good for now.
    pub expires_in_secs: i64,
}

/// The owner's Spotify session.
pub struct Session {
    config: SpotifyConfig,
    endpoints: Endpoints,
    cache: TokenCache,
    http: Client,
    token: Option<CachedToken>,
    /// A server rate limit also applies to the next read on this session.
    api_cooldown: retry::Cooldown,
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
            endpoints: Endpoints::for_session(),
            cache,
            http,
            token,
            api_cooldown: retry::Cooldown::default(),
        })
    }

    /// Talk to other hosts than Spotify's (tests use a loopback server).
    #[must_use]
    pub fn with_endpoints(mut self, endpoints: Endpoints) -> Self {
        if self.endpoints.api != endpoints.api {
            self.api_cooldown = retry::Cooldown::default();
        }
        self.endpoints = endpoints;
        self
    }

    /// The hosts this session talks to.
    #[must_use]
    pub fn endpoints(&self) -> &Endpoints {
        &self.endpoints
    }

    /// Whether a token is cached (the owner has authorized this app).
    #[must_use]
    pub fn is_authorized(&self) -> bool {
        self.token.is_some()
    }

    /// The taste scopes the owner's grant lacks (all of them before sign-in).
    #[must_use]
    pub fn missing_taste_scopes(&self) -> Vec<&'static str> {
        missing_taste_scopes(self.token.as_ref().map_or("", |t| t.scope.as_str()))
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

    /// GET a Web API URL with the owner's token. Transient read failures
    /// retry with backoff, and a 401 refreshes the token at most once. All
    /// attempts, refresh and waits share a 90-second ceiling; a longer
    /// `Retry-After` is returned, never shortened into an early retry.
    pub async fn get(&mut self, cx: &Cx, url: &str) -> Result<Vec<u8>, SpotifyError> {
        self.get_waiting(cx, url, u64::MAX).await
    }

    /// [`Self::get`] for a background read (taste, artist genres, album
    /// lists): a rate limit asking for more than [`BRIEF_WAIT_SECS`] is
    /// returned as the 429 at once, so the read halts and resumes next sync
    /// rather than holding the sync for up to an hour. The entire read,
    /// including retries and refresh, is bounded to 35 seconds.
    pub async fn get_brief(&mut self, cx: &Cx, url: &str) -> Result<Vec<u8>, SpotifyError> {
        self.get_waiting(cx, url, BRIEF_WAIT_SECS).await
    }

    async fn get_waiting(
        &mut self,
        cx: &Cx,
        url: &str,
        max_wait_secs: u64,
    ) -> Result<Vec<u8>, SpotifyError> {
        retry::get(self, cx, url, max_wait_secs).await
    }

    /// A server-requested pause still applying to subsequent Web API reads.
    /// Background schedulers persist this rounded-up duration between sessions.
    #[must_use]
    pub fn retry_after_secs(&self) -> u64 {
        let left = self.api_cooldown.remaining();
        left.as_secs()
            .saturating_add(u64::from(left.subsec_nanos() != 0))
    }

    /// Whether the owner's grant still works, read-only: one liked track is
    /// read with the cached token, refreshed first when it is about to
    /// expire (or on a 401), so a revoked or lapsed sign-in shows before a
    /// library sync needs it. A rate limit comes back at once, unwaited.
    pub async fn check_grant(&mut self, cx: &Cx) -> Result<GrantCheck, SpotifyError> {
        let before = self
            .token
            .as_ref()
            .ok_or_else(not_authorized)?
            .access_token
            .clone();
        let url = format!("{}/me/tracks?limit=1", self.endpoints.api);
        self.get_waiting(cx, &url, 0).await?;
        let token = self.token.as_ref().ok_or_else(not_authorized)?;
        Ok(GrantCheck {
            refreshed: token.access_token != before,
            expires_in_secs: token.expires_at - unix_now(),
        })
    }

    /// Prove which account this session reads, including library-only grants.
    /// Identity is required before a completed read may replace personal cache
    /// data. Do not cache this across syncs or a later sign-in on the session.
    pub(crate) async fn read_owner(&mut self, cx: &Cx) -> Result<String, SpotifyError> {
        let url = self.endpoints.me();
        let body = self.get_brief(cx, &url).await?;
        let owner: crate::client::Me = crate::client::decode(&body, "the user")?;
        if owner.id.trim().is_empty()
            || owner.id.len() > 1_024
            || owner.id.chars().any(char::is_control)
        {
            return Err(SpotifyError::Decode(
                "invalid Spotify owner identity".into(),
            ));
        }
        Ok(owner.id)
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

    /// An artist's genre tags (`GET /artists/{id}`, read-only; often none).
    pub async fn read_artist_genres(
        &mut self,
        cx: &Cx,
        artist_id: &str,
    ) -> Result<Vec<String>, SpotifyError> {
        let url = self.endpoints.artist(artist_id);
        let body = self.get_brief(cx, &url).await?;
        Ok(Artist::parse(&body)?.genres)
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

    use std::sync::Arc;

    use super::*;
    use crate::client::{SCOPE, requested_scope};
    use crate::fake_spotify::{FakeSpotify, REDIRECT, config, query_param, runtime, scratch_dir};

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
            assert_eq!(query_param(pending.url(), "scope"), requested_scope());
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
    fn a_grant_check_reads_one_track_refreshing_a_stale_token_and_sees_a_revoked_grant() {
        let spotify = FakeSpotify::start();
        let data_dir = scratch_dir("grant-check");
        let cache = TokenCache::in_data_dir(&data_dir);
        {
            let mut fake = spotify.state.lock().unwrap();
            "access-1".clone_into(&mut fake.access);
            "refresh-1".clone_into(&mut fake.refresh);
        }
        let token = |access: &str, expires_at: i64| CachedToken {
            access_token: access.into(),
            refresh_token: "refresh-1".into(),
            expires_at,
            scope: SCOPE.into(),
        };
        let endpoints = spotify.endpoints();
        let state = Arc::clone(&spotify.state);

        runtime().block_on(async move {
            let cx = Cx::current().expect("ambient Cx");
            let open = |cache: &TokenCache| {
                Session::open(config(), cache.clone(), Client::default_for_runtime(&cx))
                    .unwrap()
                    .with_endpoints(endpoints.clone())
            };

            // Not signed in: nothing to check, and nothing is sent.
            let err = open(&cache).check_grant(&cx).await.unwrap_err();
            assert!(matches!(err, SpotifyError::Auth(_)), "{err}");

            // A fresh token is used as it is.
            cache.store(&token("access-1", unix_now() + 3600)).unwrap();
            let fresh = open(&cache).check_grant(&cx).await.unwrap();
            assert!(
                !fresh.refreshed && fresh.expires_in_secs > 3500,
                "{fresh:?}"
            );

            // One about to expire is refreshed first, and the rotated pair
            // re-cached for serve's next sync.
            cache.store(&token("access-1", unix_now() + 10)).unwrap();
            let due = open(&cache).check_grant(&cx).await.unwrap();
            assert!(due.refreshed && due.expires_in_secs > 3500, "{due:?}");
            let cached = cache.load().unwrap().unwrap();
            assert_eq!(
                (cached.access_token.as_str(), cached.refresh_token.as_str()),
                ("access-r1", "refresh-r1")
            );

            // The owner revoked the app: the 401's refresh is refused.
            "revoked".clone_into(&mut state.lock().unwrap().access);
            "someone-else".clone_into(&mut state.lock().unwrap().refresh);
            let err = open(&cache).check_grant(&cx).await.unwrap_err();
            assert!(err.to_string().contains("invalid_grant"), "{err}");

            // A rate limit comes back at once rather than being waited out.
            {
                let mut fake = state.lock().unwrap();
                "access-r9".clone_into(&mut fake.access);
                fake.rate_limit_tracks_once = true;
            }
            cache.store(&token("access-r9", unix_now() + 3600)).unwrap();
            let err = open(&cache).check_grant(&cx).await.unwrap_err();
            assert!(
                matches!(err, SpotifyError::Api { status: 429, .. }),
                "{err}"
            );
        });

        let fake = spotify.stop();
        let reads: Vec<&String> = fake.log.iter().filter(|l| l.contains("/v1/")).collect();
        assert!(
            reads.iter().all(|l| l.ends_with("/v1/me/tracks?limit=1")),
            "only one-track reads: {reads:?}"
        );
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
