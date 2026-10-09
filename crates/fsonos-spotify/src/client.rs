//! Spotify Web API client (read-only, user's own library).
//!
//! Scope needed: `user-library-read` (saved albums + liked tracks) — nothing
//! else is ever requested. Auth is Authorization Code with PKCE (RFC 7636):
//! there is no client secret, and the refresh token is cached only in the
//! local, git-ignored auth cache. This file is the pure half of the client —
//! endpoint URLs, token request bodies, the PKCE pair, callback parsing, token
//! bookkeeping and the response shapes — so the HTTPS half
//! ([`crate::session`]) is a thin send/receive loop around it. The Web API never
//! starts playback; Sonos renders Spotify itself via SMAPI.

use std::fmt;
use std::fmt::Write as _;
use std::fs;
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::SpotifyError;
use crate::library::{LibraryItem, Origin};

pub const AUTHORIZE_URL: &str = "https://accounts.spotify.com/authorize";
pub const TOKEN_URL: &str = "https://accounts.spotify.com/api/token";
pub const API_BASE: &str = "https://api.spotify.com/v1";
/// The one scope this client ever requests.
pub const SCOPE: &str = "user-library-read";
/// Maximum page size of the library endpoints.
pub const PAGE_LIMIT: u32 = 50;
/// Content type of the token endpoint request bodies.
pub const FORM_CONTENT_TYPE: &str = "application/x-www-form-urlencoded";

// ── PKCE ───────────────────────────────────────────────────────────────────

/// A PKCE `code_verifier` / `code_challenge` pair (S256). The challenge is the
/// base64url-unpadded SHA-256 of the verifier. The verifier is a short-lived
/// secret: redacted from `Debug`, never persisted.
#[derive(Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// A fresh pair from 32 bytes of OS randomness (a 43-character verifier).
    pub fn generate() -> Result<Self, SpotifyError> {
        Ok(Self::from_entropy(&os_random::<32>()?))
    }

    /// The pair whose verifier is `base64url(bytes)`; 32–96 bytes give the
    /// 43–128 characters RFC 7636 §4.1 requires.
    #[must_use]
    pub fn from_entropy(bytes: &[u8]) -> Self {
        assert!(
            (32..=96).contains(&bytes.len()),
            "PKCE entropy must be 32-96 bytes"
        );
        Self::from_verifier(base64url(bytes)).expect("base64url of 32-96 bytes is a valid verifier")
    }

    /// The pair for an existing verifier, validated per RFC 7636 §4.1.
    pub fn from_verifier(verifier: impl Into<String>) -> Result<Self, SpotifyError> {
        let verifier = verifier.into();
        let valid = (43..=128).contains(&verifier.len())
            && verifier
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~'));
        if !valid {
            return Err(SpotifyError::Auth(
                "PKCE verifier must be 43-128 unreserved characters".into(),
            ));
        }
        let challenge = base64url(&sha256(verifier.as_bytes()));
        Ok(Self {
            verifier,
            challenge,
        })
    }
}

impl fmt::Debug for Pkce {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Pkce")
            .field("verifier", &"<redacted>")
            .field("challenge", &self.challenge)
            .finish()
    }
}

/// A random OAuth `state` value, binding the callback to this flow (CSRF).
pub fn random_state() -> Result<String, SpotifyError> {
    Ok(base64url(&os_random::<16>()?))
}

/// `N` bytes from the OS CSPRNG (`/dev/urandom` on macOS and Linux).
fn os_random<const N: usize>() -> Result<[u8; N], SpotifyError> {
    let mut buf = [0u8; N];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut buf)?;
    Ok(buf)
}

// ── Authorization Code flow ────────────────────────────────────────────────

/// Configuration for the Spotify client. The client id is not a secret for
/// PKCE flows, but the refresh token IS and is stored only in the local,
/// git-ignored auth cache.
#[derive(Debug, Clone)]
pub struct SpotifyConfig {
    pub client_id: String,
    pub redirect_uri: String,
}

impl SpotifyConfig {
    /// Check the config before starting a flow. Spotify accepts HTTPS redirect
    /// URIs, or plain HTTP only to a loopback IP literal (`127.0.0.1` /
    /// `[::1]`, not `localhost`).
    pub fn validate(&self) -> Result<(), SpotifyError> {
        if self.client_id.is_empty() || !self.client_id.bytes().all(|b| b.is_ascii_alphanumeric()) {
            return Err(SpotifyError::Config(
                "client id must be the app's alphanumeric Spotify client id".into(),
            ));
        }
        let uri = self.redirect_uri.as_str();
        let loopback = [
            "http://127.0.0.1:",
            "http://127.0.0.1/",
            "http://[::1]:",
            "http://[::1]/",
        ]
        .iter()
        .any(|prefix| uri.starts_with(prefix));
        if !uri.starts_with("https://") && !loopback {
            return Err(SpotifyError::Config(format!(
                "redirect URI must be https:// or http://127.0.0.1 loopback, got {uri:?}"
            )));
        }
        Ok(())
    }

    /// The URL to open in the owner's browser to grant `user-library-read`.
    #[must_use]
    pub fn authorize_url(&self, pkce: &Pkce, state: &str) -> String {
        let query = form_encode(&[
            ("client_id", &self.client_id),
            ("response_type", "code"),
            ("redirect_uri", &self.redirect_uri),
            ("code_challenge_method", "S256"),
            ("code_challenge", &pkce.challenge),
            ("scope", SCOPE),
            ("state", state),
        ]);
        format!("{AUTHORIZE_URL}?{query}")
    }

    /// Body for `POST` [`TOKEN_URL`] exchanging the callback's `code`.
    #[must_use]
    pub fn code_exchange_body(&self, code: &str, pkce: &Pkce) -> String {
        form_encode(&[
            ("grant_type", "authorization_code"),
            ("code", code),
            ("redirect_uri", &self.redirect_uri),
            ("client_id", &self.client_id),
            ("code_verifier", &pkce.verifier),
        ])
    }

    /// Body for `POST` [`TOKEN_URL`] refreshing the access token.
    #[must_use]
    pub fn refresh_body(&self, refresh_token: &str) -> String {
        form_encode(&[
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh_token),
            ("client_id", &self.client_id),
        ])
    }
}

/// Extract the authorization code from the redirect the browser lands on: a
/// request target (`/auth/spotify/callback?code=…&state=…`), a full URL, or a
/// bare query. The `state` must match the one this flow sent.
pub fn parse_callback(target: &str, expected_state: &str) -> Result<String, SpotifyError> {
    let query = target.split_once('?').map_or(target, |(_, q)| q);
    let query = query.split('#').next().unwrap_or_default();
    let params = parse_query(query)?;
    let get = |key: &str| {
        params
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    if get("state") != Some(expected_state) {
        return Err(SpotifyError::Auth(
            "callback state mismatch (stale or forged redirect)".into(),
        ));
    }
    if let Some(error) = get("error") {
        return Err(SpotifyError::Auth(format!(
            "authorization not granted: {error}"
        )));
    }
    get("code")
        .filter(|code| !code.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| SpotifyError::Auth("callback carried no code".into()))
}

// ── Tokens ─────────────────────────────────────────────────────────────────

/// The token endpoint's JSON response. Tokens are redacted from `Debug`.
#[derive(Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    pub token_type: String,
    #[serde(default)]
    pub scope: String,
    pub expires_in: i64,
    /// Always present on the code exchange; on refresh only when rotated.
    #[serde(default)]
    pub refresh_token: Option<String>,
}

impl TokenResponse {
    pub fn parse(body: &[u8]) -> Result<Self, SpotifyError> {
        serde_json::from_slice(body)
            .map_err(|e| SpotifyError::Decode(format!("token response: {e}")))
    }
}

impl fmt::Debug for TokenResponse {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenResponse")
            .field("access_token", &"<redacted>")
            .field("token_type", &self.token_type)
            .field("scope", &self.scope)
            .field("expires_in", &self.expires_in)
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .finish()
    }
}

/// The persisted credential: the owner's own tokens, kept only in the local
/// git-ignored auth cache. Tokens are redacted from `Debug`.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CachedToken {
    pub access_token: String,
    pub refresh_token: String,
    /// Unix seconds.
    pub expires_at: i64,
    pub scope: String,
}

impl CachedToken {
    /// Adopt the code-exchange response: a bearer token that carries a
    /// refresh token and the library scope.
    pub fn from_exchange(resp: TokenResponse, now: i64) -> Result<Self, SpotifyError> {
        check_bearer(&resp)?;
        if !resp.scope.split_whitespace().any(|s| s == SCOPE) {
            return Err(SpotifyError::Auth(format!(
                "granted scope {:?} lacks {SCOPE}",
                resp.scope
            )));
        }
        let refresh_token = resp
            .refresh_token
            .filter(|t| !t.is_empty())
            .ok_or_else(|| SpotifyError::Auth("token exchange returned no refresh token".into()))?;
        Ok(Self {
            access_token: resp.access_token,
            refresh_token,
            expires_at: now.saturating_add(resp.expires_in),
            scope: resp.scope,
        })
    }

    /// Apply a refresh response. Spotify may rotate the refresh token; keep
    /// the old one only when no new one came back.
    pub fn refreshed(&self, resp: TokenResponse, now: i64) -> Result<Self, SpotifyError> {
        check_bearer(&resp)?;
        Ok(Self {
            access_token: resp.access_token,
            refresh_token: resp
                .refresh_token
                .filter(|t| !t.is_empty())
                .unwrap_or_else(|| self.refresh_token.clone()),
            expires_at: now.saturating_add(resp.expires_in),
            scope: if resp.scope.is_empty() {
                self.scope.clone()
            } else {
                resp.scope
            },
        })
    }

    /// Whether the access token stays valid for `margin_secs` more seconds.
    #[must_use]
    pub fn is_fresh(&self, now: i64, margin_secs: i64) -> bool {
        now.saturating_add(margin_secs) < self.expires_at
    }

    /// The `Authorization` header value for Web API requests.
    #[must_use]
    pub fn authorization(&self) -> String {
        format!("Bearer {}", self.access_token)
    }
}

impl fmt::Debug for CachedToken {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("CachedToken")
            .field("access_token", &"<redacted>")
            .field("refresh_token", &"<redacted>")
            .field("expires_at", &self.expires_at)
            .field("scope", &self.scope)
            .finish()
    }
}

fn check_bearer(resp: &TokenResponse) -> Result<(), SpotifyError> {
    if resp.token_type.eq_ignore_ascii_case("bearer") && !resp.access_token.is_empty() {
        Ok(())
    } else {
        Err(SpotifyError::Auth(format!(
            "unexpected token type {:?}",
            resp.token_type
        )))
    }
}

/// Map a non-2xx token endpoint response (`{"error": "invalid_grant", …}`).
#[must_use]
pub fn token_error(status: u16, body: &[u8]) -> SpotifyError {
    #[derive(Deserialize)]
    struct Body {
        error: String,
        #[serde(default)]
        error_description: Option<String>,
    }
    match serde_json::from_slice::<Body>(body) {
        Ok(b) => SpotifyError::Auth(format!(
            "token endpoint {status}: {}{}",
            b.error,
            b.error_description
                .map(|d| format!(" ({d})"))
                .unwrap_or_default()
        )),
        Err(_) => SpotifyError::Auth(format!("token endpoint {status}")),
    }
}

/// Map a non-2xx Web API response, keeping Spotify's message.
#[must_use]
pub fn api_error(status: u16, body: &[u8]) -> SpotifyError {
    #[derive(Deserialize)]
    struct Envelope {
        error: Detail,
    }
    #[derive(Deserialize)]
    struct Detail {
        message: String,
    }
    let body = serde_json::from_slice::<Envelope>(body).map_or_else(
        |_| String::from_utf8_lossy(body).chars().take(300).collect(),
        |e| e.error.message,
    );
    SpotifyError::Api { status, body }
}

/// Seconds to wait after a 429, from its `Retry-After` header (Spotify sends
/// whole seconds): 1 when absent or unparseable, capped at an hour.
#[must_use]
pub fn retry_after_secs(header: Option<&str>) -> u64 {
    header
        .and_then(|v| v.trim().parse::<u64>().ok())
        .unwrap_or(1)
        .clamp(1, 3600)
}

// ── Token cache ────────────────────────────────────────────────────────────

/// The on-disk token cache: one JSON file in the daemon's data directory
/// (`--data-dir` / `FSONOS_DATA_DIR`, never the repo). It holds the refresh
/// token, so the directory is created owner-only (0700) and the file is
/// written owner-only (0600) via an atomic rename.
#[derive(Debug, Clone)]
pub struct TokenCache {
    path: PathBuf,
}

impl TokenCache {
    #[must_use]
    pub fn in_data_dir(data_dir: &Path) -> Self {
        Self {
            path: data_dir.join("auth").join("spotify-token.json"),
        }
    }

    #[must_use]
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// The cached token, or `None` before the owner has authorized.
    pub fn load(&self) -> Result<Option<CachedToken>, SpotifyError> {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e.into()),
        };
        serde_json::from_slice(&bytes)
            .map(Some)
            .map_err(|e| SpotifyError::Decode(format!("token cache {}: {e}", self.path.display())))
    }

    /// Persist `token`, atomically replacing any previous one.
    pub fn store(&self, token: &CachedToken) -> Result<(), SpotifyError> {
        if let Some(dir) = self.path.parent() {
            create_private_dir(dir)?;
        }
        let json = serde_json::to_vec_pretty(token)
            .map_err(|e| SpotifyError::Decode(format!("token cache: {e}")))?;
        let tmp = self.path.with_extension("json.tmp");
        write_private(&tmp, &json)?;
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(unix)]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    use std::os::unix::fs::DirBuilderExt;
    fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
}

#[cfg(not(unix))]
fn create_private_dir(dir: &Path) -> io::Result<()> {
    fs::create_dir_all(dir)
}

fn write_private(path: &Path, bytes: &[u8]) -> io::Result<()> {
    let mut options = fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(path)?;
    file.write_all(bytes)?;
    file.sync_all()
}

// ── Library endpoints ──────────────────────────────────────────────────────

/// Where requests go: Spotify's hosts by default (tests point them at a
/// loopback server). The bearer token is only ever sent under `api`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoints {
    /// The OAuth token endpoint.
    pub token: String,
    /// The Web API base, without a trailing slash.
    pub api: String,
}

impl Default for Endpoints {
    fn default() -> Self {
        Self {
            token: TOKEN_URL.into(),
            api: API_BASE.into(),
        }
    }
}

impl Endpoints {
    /// A page of the owner's saved albums, each embedding its first 50
    /// tracks. `market=from_token` makes Spotify relink tracks and report
    /// `is_playable`.
    #[must_use]
    pub fn saved_albums(&self, offset: u32) -> String {
        format!(
            "{}/me/albums?limit={PAGE_LIMIT}&offset={offset}&market=from_token",
            self.api
        )
    }

    /// A page of the owner's liked tracks.
    #[must_use]
    pub fn saved_tracks(&self, offset: u32) -> String {
        format!(
            "{}/me/tracks?limit={PAGE_LIMIT}&offset={offset}&market=from_token",
            self.api
        )
    }

    /// One artist (`GET /artists/{id}`): its genre tags. The batch form is
    /// gone for development-mode apps (February 2026), so it is one request
    /// per artist.
    #[must_use]
    pub fn artist(&self, artist_id: &str) -> String {
        format!("{}/artists/{}", self.api, percent_encode(artist_id))
    }

    /// A page of one album's tracks, for albums longer than their embedded
    /// page.
    #[must_use]
    pub fn album_tracks(&self, album_id: &str, offset: u32) -> String {
        format!(
            "{}/albums/{}/tracks?limit={PAGE_LIMIT}&offset={offset}&market=from_token",
            self.api,
            percent_encode(album_id)
        )
    }

    /// Whether `url` (e.g. a page's `next`) is under the Web API base — the
    /// bearer token is only ever sent there.
    #[must_use]
    pub fn is_api_url(&self, url: &str) -> bool {
        url.strip_prefix(self.api.as_str())
            .is_some_and(|rest| rest.starts_with('/'))
    }
}

/// A Web API paging object. Follow `next` (after [`Endpoints::is_api_url`])
/// until `None`.
#[derive(Debug, Clone, Deserialize)]
pub struct Paging<T> {
    pub items: Vec<T>,
    #[serde(default)]
    pub next: Option<String>,
    #[serde(default)]
    pub offset: u32,
    #[serde(default)]
    pub limit: u32,
    #[serde(default)]
    pub total: u32,
}

impl<T: serde::de::DeserializeOwned> Paging<T> {
    pub fn parse(body: &[u8]) -> Result<Self, SpotifyError> {
        serde_json::from_slice(body)
            .map_err(|e| SpotifyError::Decode(format!("paging object: {e}")))
    }
}

/// `GET /artists/{id}`: the fields the DJ reads. Development-mode apps
/// (February 2026) lose `followers` and `popularity`, not `genres`.
#[derive(Debug, Clone, Deserialize)]
pub struct Artist {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub genres: Vec<String>,
}

impl Artist {
    pub fn parse(body: &[u8]) -> Result<Self, SpotifyError> {
        serde_json::from_slice(body).map_err(|e| SpotifyError::Decode(format!("artist: {e}")))
    }
}

/// `GET /me/albums` item. Optional fields tolerate both the classic shape and
/// the February 2026 development-mode shape (no `label`/`popularity`).
#[derive(Debug, Clone, Deserialize)]
pub struct SavedAlbum {
    #[serde(default)]
    pub added_at: Option<String>,
    pub album: Album,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Album {
    pub id: String,
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub album_type: Option<String>,
    #[serde(default)]
    pub artists: Vec<SimplifiedArtist>,
    #[serde(default)]
    pub genres: Vec<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
    #[serde(default)]
    pub total_tracks: u32,
    #[serde(default)]
    pub tracks: Option<Paging<SimplifiedTrack>>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SimplifiedArtist {
    #[serde(default)]
    pub id: Option<String>,
    pub name: String,
    #[serde(default)]
    pub uri: Option<String>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SimplifiedTrack {
    #[serde(default)]
    pub id: Option<String>,
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub artists: Vec<SimplifiedArtist>,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub disc_number: u32,
    #[serde(default)]
    pub track_number: u32,
    #[serde(default)]
    pub explicit: bool,
    #[serde(default)]
    pub is_local: bool,
    #[serde(default)]
    pub is_playable: Option<bool>,
}

/// `GET /me/tracks` item. `track` is tolerated as null so one unavailable
/// entry can't fail a whole page.
#[derive(Debug, Clone, Deserialize)]
pub struct SavedTrack {
    #[serde(default)]
    pub added_at: Option<String>,
    #[serde(default)]
    pub track: Option<FullTrack>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct FullTrack {
    #[serde(default)]
    pub id: Option<String>,
    pub uri: String,
    pub name: String,
    #[serde(default)]
    pub artists: Vec<SimplifiedArtist>,
    pub album: SimplifiedAlbum,
    #[serde(default)]
    pub duration_ms: u64,
    #[serde(default)]
    pub disc_number: u32,
    #[serde(default)]
    pub track_number: u32,
    #[serde(default)]
    pub explicit: bool,
    #[serde(default)]
    pub is_local: bool,
    #[serde(default)]
    pub is_playable: Option<bool>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct SimplifiedAlbum {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(default)]
    pub uri: Option<String>,
    pub name: String,
    #[serde(default)]
    pub artists: Vec<SimplifiedArtist>,
    #[serde(default)]
    pub album_type: Option<String>,
    #[serde(default)]
    pub release_date: Option<String>,
}

impl SavedAlbum {
    /// This album's embedded tracks as library items (see
    /// [`Album::library_items`]), stamped with when the album was saved.
    #[must_use]
    pub fn library_items(&self) -> Vec<LibraryItem> {
        let added_at = self.added_unix();
        let mut items = self.album.library_items();
        for item in &mut items {
            item.added_at = added_at;
        }
        items
    }

    /// `added_at` as Unix seconds.
    #[must_use]
    pub fn added_unix(&self) -> Option<i64> {
        unix_seconds(self.added_at.as_deref())
    }
}

impl Album {
    /// The embedded first page of tracks as library items. When
    /// `tracks.next` is set, page [`Endpoints::album_tracks`] and convert the rest
    /// with [`Self::library_item`].
    #[must_use]
    pub fn library_items(&self) -> Vec<LibraryItem> {
        self.tracks
            .iter()
            .flat_map(|page| &page.items)
            .filter_map(|track| self.library_item(track))
            .collect()
    }

    /// One of this album's tracks as a library item; `None` if Sonos can't
    /// render it (a local file, or unplayable in the owner's market).
    #[must_use]
    pub fn library_item(&self, track: &SimplifiedTrack) -> Option<LibraryItem> {
        if !renderable(&track.uri, track.is_local, track.is_playable) {
            return None;
        }
        Some(LibraryItem {
            source_uri: track.uri.clone(),
            title: track.name.clone(),
            artists: names(&track.artists),
            artist_id: lead_id(&track.artists),
            album: Some(self.name.clone()),
            album_uri: Some(self.uri.clone()),
            album_artists: names(&self.artists),
            disc_number: position(track.disc_number),
            track_number: position(track.track_number),
            added_at: None,
            genres: self.genres.clone(),
            release_year: release_year(self.release_date.as_deref()),
            label: self.label.clone(),
            duration_secs: secs(track.duration_ms),
            explicit: track.explicit,
            origin: Origin::SavedAlbum,
        })
    }
}

impl SavedTrack {
    /// The liked track as a library item; `None` if absent or unrenderable.
    #[must_use]
    pub fn library_item(&self) -> Option<LibraryItem> {
        let track = self.track.as_ref()?;
        if !renderable(&track.uri, track.is_local, track.is_playable) {
            return None;
        }
        Some(LibraryItem {
            source_uri: track.uri.clone(),
            title: track.name.clone(),
            artists: names(&track.artists),
            artist_id: lead_id(&track.artists),
            album: Some(track.album.name.clone()),
            album_uri: track.album.uri.clone(),
            album_artists: names(&track.album.artists),
            disc_number: position(track.disc_number),
            track_number: position(track.track_number),
            added_at: unix_seconds(self.added_at.as_deref()),
            genres: Vec::new(),
            release_year: release_year(track.album.release_date.as_deref()),
            label: None,
            duration_secs: secs(track.duration_ms),
            explicit: track.explicit,
            origin: Origin::LikedTrack,
        })
    }
}

/// The year of a Spotify `release_date` (`"1982"`, `"2019-05"`,
/// `"2019-05-10"`); `None` when absent or unknown (`"0000"`).
fn release_year(date: Option<&str>) -> Option<u16> {
    date?.get(..4)?.parse::<u16>().ok().filter(|&year| year > 0)
}

/// The lead (first credited) artist's id, when the read gave one.
fn lead_id(artists: &[SimplifiedArtist]) -> Option<String> {
    artists
        .first()
        .and_then(|a| a.id.clone())
        .filter(|id| !id.is_empty())
}

/// Sonos renders only real Spotify tracks the owner's market can play.
pub(crate) fn renderable(uri: &str, is_local: bool, is_playable: Option<bool>) -> bool {
    uri.starts_with("spotify:track:") && !is_local && is_playable != Some(false)
}

/// An RFC 3339 timestamp (Spotify's `added_at`) as Unix seconds.
fn unix_seconds(rfc3339: Option<&str>) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(rfc3339?)
        .ok()
        .map(|t| t.timestamp())
}

/// Spotify reports 0 for an unknown disc/track position.
fn position(n: u32) -> Option<u32> {
    (n > 0).then_some(n)
}

fn names(artists: &[SimplifiedArtist]) -> Vec<String> {
    artists.iter().map(|a| a.name.clone()).collect()
}

pub(crate) fn secs(duration_ms: u64) -> Option<u32> {
    if duration_ms == 0 {
        return None;
    }
    u32::try_from(duration_ms.div_ceil(1000)).ok()
}

// ── Encoding helpers ───────────────────────────────────────────────────────

/// RFC 3986 percent-encoding of everything but the unreserved set (valid in
/// both query strings and form bodies).
fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
            out.push(char::from(b));
        } else {
            let _ = write!(out, "%{b:02X}");
        }
    }
    out
}

fn form_encode(pairs: &[(&str, &str)]) -> String {
    pairs
        .iter()
        .map(|(k, v)| format!("{}={}", percent_encode(k), percent_encode(v)))
        .collect::<Vec<_>>()
        .join("&")
}

pub(crate) fn parse_query(query: &str) -> Result<Vec<(String, String)>, SpotifyError> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .map(|pair| {
            let (k, v) = pair.split_once('=').unwrap_or((pair, ""));
            Ok((percent_decode(k)?, percent_decode(v)?))
        })
        .collect()
}

/// Decode `application/x-www-form-urlencoded` (`+` is a space). Errors never
/// echo the input: a callback query carries the authorization code.
fn percent_decode(s: &str) -> Result<String, SpotifyError> {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'+' => out.push(b' '),
            b'%' => {
                let byte = bytes
                    .get(i + 1..i + 3)
                    .filter(|hex| hex.iter().all(u8::is_ascii_hexdigit))
                    .and_then(|hex| std::str::from_utf8(hex).ok())
                    .and_then(|hex| u8::from_str_radix(hex, 16).ok())
                    .ok_or_else(|| {
                        SpotifyError::Decode("malformed percent-escape in query".into())
                    })?;
                out.push(byte);
                i += 2;
            }
            b => out.push(b),
        }
        i += 1;
    }
    String::from_utf8(out).map_err(|_| SpotifyError::Decode("query is not UTF-8".into()))
}

/// Unpadded base64url (RFC 4648 §5), as PKCE requires.
pub(crate) fn base64url(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk
            .iter()
            .enumerate()
            .fold(0u32, |acc, (i, &b)| acc | (u32::from(b) << (16 - 8 * i)));
        for i in 0..=chunk.len() {
            out.push(char::from(ALPHABET[((n >> (18 - 6 * i)) & 0x3f) as usize]));
        }
    }
    out
}

/// SHA-256 round constants (FIPS 180-4 §4.2.2).
const K: [u32; 64] = [
    0x428a_2f98,
    0x7137_4491,
    0xb5c0_fbcf,
    0xe9b5_dba5,
    0x3956_c25b,
    0x59f1_11f1,
    0x923f_82a4,
    0xab1c_5ed5,
    0xd807_aa98,
    0x1283_5b01,
    0x2431_85be,
    0x550c_7dc3,
    0x72be_5d74,
    0x80de_b1fe,
    0x9bdc_06a7,
    0xc19b_f174,
    0xe49b_69c1,
    0xefbe_4786,
    0x0fc1_9dc6,
    0x240c_a1cc,
    0x2de9_2c6f,
    0x4a74_84aa,
    0x5cb0_a9dc,
    0x76f9_88da,
    0x983e_5152,
    0xa831_c66d,
    0xb003_27c8,
    0xbf59_7fc7,
    0xc6e0_0bf3,
    0xd5a7_9147,
    0x06ca_6351,
    0x1429_2967,
    0x27b7_0a85,
    0x2e1b_2138,
    0x4d2c_6dfc,
    0x5338_0d13,
    0x650a_7354,
    0x766a_0abb,
    0x81c2_c92e,
    0x9272_2c85,
    0xa2bf_e8a1,
    0xa81a_664b,
    0xc24b_8b70,
    0xc76c_51a3,
    0xd192_e819,
    0xd699_0624,
    0xf40e_3585,
    0x106a_a070,
    0x19a4_c116,
    0x1e37_6c08,
    0x2748_774c,
    0x34b0_bcb5,
    0x391c_0cb3,
    0x4ed8_aa4a,
    0x5b9c_ca4f,
    0x682e_6ff3,
    0x748f_82ee,
    0x78a5_636f,
    0x84c8_7814,
    0x8cc7_0208,
    0x90be_fffa,
    0xa450_6ceb,
    0xbef9_a3f7,
    0xc671_78f2,
];

/// SHA-256 (FIPS 180-4). Only the PKCE challenge uses it — hashing the
/// public-bound verifier — so a dependency isn't worth it; the NIST vectors
/// and RFC 7636's worked example pin it in the tests.
#[allow(clippy::many_single_char_names)]
pub(crate) fn sha256(data: &[u8]) -> [u8; 32] {
    let mut h: [u32; 8] = [
        0x6a09_e667,
        0xbb67_ae85,
        0x3c6e_f372,
        0xa54f_f53a,
        0x510e_527f,
        0x9b05_688c,
        0x1f83_d9ab,
        0x5be0_cd19,
    ];
    let bit_len = u64::try_from(data.len())
        .expect("input fits u64")
        .wrapping_mul(8);
    let mut msg = data.to_vec();
    msg.push(0x80);
    while msg.len() % 64 != 56 {
        msg.push(0);
    }
    msg.extend_from_slice(&bit_len.to_be_bytes());

    for block in msg.as_chunks::<64>().0 {
        let mut w = [0u32; 64];
        for (word, bytes) in w.iter_mut().zip(block.as_chunks::<4>().0) {
            *word = u32::from_be_bytes(*bytes);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }
        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for (&k, &wi) in K.iter().zip(&w) {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ (!e & g);
            let t1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(k)
                .wrapping_add(wi);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let t2 = s0.wrapping_add(maj);
            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(t1);
            d = c;
            c = b;
            b = a;
            a = t1.wrapping_add(t2);
        }
        for (state, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
            *state = state.wrapping_add(v);
        }
    }

    let mut out = [0u8; 32];
    for (dst, word) in out.as_chunks_mut::<4>().0.iter_mut().zip(h) {
        *dst = word.to_be_bytes();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(bytes: &[u8]) -> String {
        bytes.iter().fold(String::new(), |mut out, b| {
            let _ = write!(out, "{b:02x}");
            out
        })
    }

    fn config() -> SpotifyConfig {
        SpotifyConfig {
            client_id: "0123456789abcdef0123456789abcdef".into(),
            redirect_uri: "http://127.0.0.1:8099/auth/spotify/callback".into(),
        }
    }

    #[test]
    fn sha256_matches_nist_vectors() {
        assert_eq!(
            hex(&sha256(b"")),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            hex(&sha256(b"abc")),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(
            hex(&sha256(
                b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
            )),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        // Padding edge: exactly one block of payload forces a second block.
        assert_eq!(
            hex(&sha256(&[b'a'; 64])),
            "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"
        );
        assert_eq!(
            hex(&sha256(&vec![b'a'; 1_000_000])),
            "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0"
        );
    }

    #[test]
    fn base64url_matches_rfc4648_unpadded() {
        let cases = [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ];
        for (input, want) in cases {
            assert_eq!(base64url(input.as_bytes()), want);
        }
        // The URL-safe alphabet: 62 → '-', 63 → '_'.
        assert_eq!(base64url(&[0xfb, 0xff]), "-_8");
    }

    #[test]
    fn pkce_matches_rfc7636_appendix_b() {
        let octets = [
            116, 24, 223, 180, 151, 153, 224, 37, 79, 250, 96, 125, 216, 173, 187, 186, 22, 212,
            37, 77, 105, 214, 191, 240, 91, 88, 5, 88, 83, 132, 141, 121,
        ];
        let pkce = Pkce::from_entropy(&octets);
        assert_eq!(pkce.verifier, "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk");
        assert_eq!(
            pkce.challenge,
            "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
        );
        assert!(
            !format!("{pkce:?}").contains(&pkce.verifier),
            "verifier leaks via Debug"
        );
    }

    #[test]
    fn pkce_generation_and_validation() {
        let a = Pkce::generate().unwrap();
        let b = Pkce::generate().unwrap();
        assert_eq!(a.verifier.len(), 43);
        assert_ne!(a.verifier, b.verifier);
        assert_eq!(Pkce::from_verifier(a.verifier.clone()).unwrap(), a);
        assert!(Pkce::from_verifier("too-short").is_err());
        assert!(Pkce::from_verifier("x".repeat(129)).is_err());
        assert!(Pkce::from_verifier(format!("{}!", "a".repeat(50))).is_err());
        let state = random_state().unwrap();
        assert_eq!(state.len(), 22);
        assert_ne!(state, random_state().unwrap());
    }

    #[test]
    fn config_validation() {
        assert!(config().validate().is_ok());
        let https = SpotifyConfig {
            redirect_uri: "https://fsonos.example.ts.net/cb".into(),
            ..config()
        };
        assert!(https.validate().is_ok());
        let ipv6 = SpotifyConfig {
            redirect_uri: "http://[::1]:8099/cb".into(),
            ..config()
        };
        assert!(ipv6.validate().is_ok());
        for bad in [
            "http://localhost:8099/cb",
            "http://192.168.1.10/cb",
            "http://127.0.0.1.evil/cb",
        ] {
            let c = SpotifyConfig {
                redirect_uri: bad.into(),
                ..config()
            };
            assert!(c.validate().is_err(), "{bad} accepted");
        }
        let no_id = SpotifyConfig {
            client_id: String::new(),
            ..config()
        };
        assert!(no_id.validate().is_err());
    }

    #[test]
    fn authorize_url_and_token_bodies() {
        let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk").unwrap();
        let url = config().authorize_url(&pkce, "st/ate");
        assert_eq!(
            url,
            "https://accounts.spotify.com/authorize?client_id=0123456789abcdef0123456789abcdef\
             &response_type=code\
             &redirect_uri=http%3A%2F%2F127.0.0.1%3A8099%2Fauth%2Fspotify%2Fcallback\
             &code_challenge_method=S256\
             &code_challenge=E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM\
             &scope=user-library-read&state=st%2Fate"
        );
        assert_eq!(
            config().code_exchange_body("AQ+code", &pkce),
            "grant_type=authorization_code&code=AQ%2Bcode\
             &redirect_uri=http%3A%2F%2F127.0.0.1%3A8099%2Fauth%2Fspotify%2Fcallback\
             &client_id=0123456789abcdef0123456789abcdef\
             &code_verifier=dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"
        );
        assert_eq!(
            config().refresh_body("r/t"),
            "grant_type=refresh_token&refresh_token=r%2Ft&client_id=0123456789abcdef0123456789abcdef"
        );
    }

    #[test]
    fn callback_parsing() {
        let target = "/auth/spotify/callback?code=AQ%2Babc+d&state=s1";
        assert_eq!(parse_callback(target, "s1").unwrap(), "AQ+abc d");
        assert_eq!(
            parse_callback("code=xyz&state=s1#frag", "s1").unwrap(),
            "xyz"
        );
        let full = "http://127.0.0.1:8099/auth/spotify/callback?state=s1&code=xyz";
        assert_eq!(parse_callback(full, "s1").unwrap(), "xyz");

        let err = |t: &str| parse_callback(t, "s1").unwrap_err().to_string();
        assert!(err("/cb?code=xyz&state=other").contains("state mismatch"));
        assert!(err("/cb?code=xyz").contains("state mismatch"));
        assert!(err("/cb?error=access_denied&state=s1").contains("access_denied"));
        assert!(err("/cb?state=s1").contains("no code"));
        let bad = err("/cb?code=secret%zz&state=s1");
        assert!(
            bad.contains("percent-escape") && !bad.contains("secret"),
            "{bad}"
        );
    }

    fn token(json: &str) -> TokenResponse {
        TokenResponse::parse(json.as_bytes()).unwrap()
    }

    #[test]
    fn token_lifecycle() {
        let exchange = token(
            r#"{"access_token":"access-1","token_type":"Bearer","scope":"user-library-read",
                "expires_in":3600,"refresh_token":"refresh-1"}"#,
        );
        assert!(!format!("{exchange:?}").contains("access-1"));
        let cached = CachedToken::from_exchange(exchange, 1_000).unwrap();
        assert_eq!(cached.expires_at, 4_600);
        assert!(cached.is_fresh(4_000, 60));
        assert!(!cached.is_fresh(4_560, 60));
        assert_eq!(cached.authorization(), "Bearer access-1");
        let debug = format!("{cached:?}");
        assert!(
            !debug.contains("access-1") && !debug.contains("refresh-1"),
            "{debug}"
        );

        // Refresh without rotation keeps the refresh token and scope.
        let kept = cached
            .refreshed(
                token(r#"{"access_token":"access-2","token_type":"Bearer","expires_in":3600}"#),
                5_000,
            )
            .unwrap();
        assert_eq!(
            (kept.access_token.as_str(), kept.refresh_token.as_str()),
            ("access-2", "refresh-1")
        );
        assert_eq!(
            (kept.expires_at, kept.scope.as_str()),
            (8_600, "user-library-read")
        );
        // Rotation adopts the new refresh token.
        let rotated = kept
            .refreshed(
                token(r#"{"access_token":"access-3","token_type":"bearer","expires_in":60,"refresh_token":"refresh-2"}"#),
                9_000,
            )
            .unwrap();
        assert_eq!(rotated.refresh_token, "refresh-2");

        // The cache round-trips through JSON.
        let json = serde_json::to_string(&rotated).unwrap();
        assert_eq!(serde_json::from_str::<CachedToken>(&json).unwrap(), rotated);
    }

    #[test]
    fn token_exchange_rejections() {
        let no_scope = token(
            r#"{"access_token":"a","token_type":"Bearer","scope":"","expires_in":3600,"refresh_token":"r"}"#,
        );
        assert!(CachedToken::from_exchange(no_scope, 0).is_err());
        let no_refresh = token(
            r#"{"access_token":"a","token_type":"Bearer","scope":"user-library-read","expires_in":3600}"#,
        );
        assert!(CachedToken::from_exchange(no_refresh, 0).is_err());
        let mac = token(
            r#"{"access_token":"a","token_type":"MAC","scope":"user-library-read","expires_in":3600,"refresh_token":"r"}"#,
        );
        assert!(CachedToken::from_exchange(mac, 0).is_err());
        assert!(TokenResponse::parse(b"not json").is_err());
    }

    /// A fresh directory under the OS temp dir, unique to this test run.
    fn scratch_dir(name: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fsonos-spotify-{}-{name}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        dir
    }

    #[test]
    fn token_cache_round_trips_privately() {
        let data_dir = scratch_dir("token-cache");
        let cache = TokenCache::in_data_dir(&data_dir);
        assert!(cache.load().unwrap().is_none(), "nothing cached yet");

        let first = CachedToken {
            access_token: "access-1".into(),
            refresh_token: "refresh-1".into(),
            expires_at: 4_600,
            scope: SCOPE.into(),
        };
        cache.store(&first).unwrap();
        assert_eq!(cache.load().unwrap(), Some(first.clone()));
        let second = CachedToken {
            access_token: "access-2".into(),
            ..first
        };
        cache.store(&second).unwrap();
        assert_eq!(cache.load().unwrap(), Some(second));
        assert!(
            !cache.path().with_extension("json.tmp").exists(),
            "temp file renamed away"
        );

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = |p: &Path| fs::metadata(p).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode(cache.path()), 0o600);
            assert_eq!(mode(cache.path().parent().unwrap()), 0o700);
        }

        fs::write(cache.path(), b"{ not json").unwrap();
        let err = cache.load().unwrap_err().to_string();
        assert!(err.contains("token cache"), "{err}");
        fs::remove_dir_all(&data_dir).unwrap();
    }

    #[test]
    fn retry_after_header() {
        assert_eq!(retry_after_secs(Some("7")), 7);
        assert_eq!(retry_after_secs(Some(" 30 ")), 30);
        assert_eq!(retry_after_secs(Some("0")), 1);
        assert_eq!(retry_after_secs(Some("Wed, 21 Oct 2026 07:28:00 GMT")), 1);
        assert_eq!(retry_after_secs(None), 1);
        assert_eq!(retry_after_secs(Some("86400")), 3600);
    }

    #[test]
    fn error_bodies() {
        let api = api_error(
            429,
            br#"{"error":{"status":429,"message":"API rate limit exceeded"}}"#,
        );
        assert_eq!(api.to_string(), "api error 429: API rate limit exceeded");
        assert_eq!(
            api_error(502, b"<html>bad gateway</html>").to_string(),
            "api error 502: <html>bad gateway</html>"
        );
        let auth = token_error(
            400,
            br#"{"error":"invalid_grant","error_description":"Invalid refresh token"}"#,
        );
        assert_eq!(
            auth.to_string(),
            "auth error: token endpoint 400: invalid_grant (Invalid refresh token)"
        );
    }

    #[test]
    fn endpoint_urls() {
        let spotify = Endpoints::default();
        assert_eq!(
            spotify.saved_albums(100),
            "https://api.spotify.com/v1/me/albums?limit=50&offset=100&market=from_token"
        );
        assert_eq!(
            spotify.saved_tracks(0),
            "https://api.spotify.com/v1/me/tracks?limit=50&offset=0&market=from_token"
        );
        assert_eq!(
            spotify.album_tracks("FakeAlbum0000000000002", 50),
            "https://api.spotify.com/v1/albums/FakeAlbum0000000000002/tracks?limit=50&offset=50&market=from_token"
        );
        assert!(spotify.is_api_url("https://api.spotify.com/v1/me/albums?offset=50&limit=50"));
        assert!(!spotify.is_api_url("https://api.spotify.com/v1.evil.example/me"));
        assert!(!spotify.is_api_url("https://evil.example/v1/me/albums"));

        let local = Endpoints {
            token: "http://127.0.0.1:9/api/token".into(),
            api: "http://127.0.0.1:9/v1".into(),
        };
        assert_eq!(
            local.saved_tracks(50),
            "http://127.0.0.1:9/v1/me/tracks?limit=50&offset=50&market=from_token"
        );
        assert!(local.is_api_url("http://127.0.0.1:9/v1/me/tracks"));
        assert!(!local.is_api_url("https://api.spotify.com/v1/me/tracks"));
    }

    #[test]
    fn saved_albums_page_parses_both_api_shapes() {
        let page =
            Paging::<SavedAlbum>::parse(include_bytes!("../tests/fixtures/saved_albums_page.json"))
                .unwrap();
        assert_eq!((page.offset, page.limit, page.total), (0, 50, 51));
        let spotify = Endpoints::default();
        assert_eq!(
            page.next.as_deref().map(|u| spotify.is_api_url(u)),
            Some(true)
        );
        assert_eq!(page.items.len(), 2);

        // Classic shape: label, popularity, external ids present (and ignored).
        let goldberg = &page.items[0];
        assert_eq!(goldberg.album.label.as_deref(), Some("Fake Classics"));
        let items = goldberg.library_items();
        assert_eq!(items.len(), 3);
        let aria = &items[0];
        assert_eq!(aria.source_uri, "spotify:track:FakeTrack0000000000001");
        assert_eq!(aria.title, "Goldberg Variations, BWV 988: Aria");
        assert_eq!(aria.artists, ["Johann Sebastian Bach", "Test Pianist"]);
        assert_eq!(aria.artist_id.as_deref(), Some("FakeArtist000000000001"));
        assert_eq!(
            aria.album_uri.as_deref(),
            Some("spotify:album:FakeAlbum0000000000001")
        );
        assert_eq!(aria.duration_secs, Some(183));
        assert_eq!((aria.disc_number, aria.track_number), (Some(1), Some(1)));
        assert_eq!(items[2].track_number, Some(3));
        // 2025-01-15T20:31:02Z, the album's added_at.
        assert!(items.iter().all(|i| i.added_at == Some(1_736_973_062)));
        assert_eq!(aria.origin, Origin::SavedAlbum);
        assert!(
            items.iter().all(|i| i.release_year == Some(1982)),
            "\"1982\""
        );

        // 2026 dev-mode shape: no label/popularity; a long album whose
        // embedded page continues, and an unplayable track that is skipped.
        let long = &page.items[1].album;
        assert!(long.label.is_none());
        let tracks = long.tracks.as_ref().unwrap();
        assert_eq!(
            tracks.next.as_deref().map(|u| spotify.is_api_url(u)),
            Some(true)
        );
        assert_eq!(long.library_items().len(), 1);
        assert_eq!(
            long.library_items()[0].release_year,
            Some(2019),
            "2019-05-10"
        );
    }

    #[test]
    fn artists_parse_with_or_without_the_dev_mode_fields() {
        let classic = Artist::parse(
            br#"{"id":"FakeArtist01","name":"Nina Marsh Quartet","type":"artist",
                "genres":["cool jazz","hard bop"],"popularity":40,
                "followers":{"href":null,"total":12}}"#,
        )
        .unwrap();
        assert_eq!(classic.genres, ["cool jazz", "hard bop"]);
        let dev_mode =
            Artist::parse(br#"{"id":"FakeArtist02","name":"X","type":"artist"}"#).unwrap();
        assert!(dev_mode.genres.is_empty());
        assert!(Artist::parse(br#"{"genres":"jazz"}"#).is_err());
        assert_eq!(
            Endpoints::default().artist("FakeArtist01"),
            "https://api.spotify.com/v1/artists/FakeArtist01"
        );
    }

    #[test]
    fn release_years_parse_every_precision() {
        assert_eq!(release_year(Some("1982")), Some(1982));
        assert_eq!(release_year(Some("1971-11")), Some(1971));
        assert_eq!(release_year(Some("2019-05-10")), Some(2019));
        assert_eq!(release_year(Some("0000")), None);
        assert_eq!(release_year(Some("19")), None);
        assert_eq!(release_year(None), None);
    }

    #[test]
    fn saved_tracks_page_skips_unrenderable_entries() {
        let page =
            Paging::<SavedTrack>::parse(include_bytes!("../tests/fixtures/saved_tracks_page.json"))
                .unwrap();
        assert_eq!(page.items.len(), 4);
        let items: Vec<LibraryItem> = page
            .items
            .iter()
            .filter_map(SavedTrack::library_item)
            .collect();
        assert_eq!(
            items.len(),
            1,
            "local file, unplayable, and null entries are skipped"
        );
        let clair = &items[0];
        assert_eq!(clair.title, "Suite bergamasque, L. 75: III. Clair de lune");
        assert_eq!(
            clair.album.as_deref(),
            Some("Debussy: Préludes & Suite bergamasque")
        );
        assert_eq!(clair.origin, Origin::LikedTrack);
        assert!(crate::classical::is_classical(clair));
    }
}
