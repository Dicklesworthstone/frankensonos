//! The iPhone app's side of the daemon's Spotify: its sign-in exchange,
//! catalog search, and the browse lists read from the library cache. Sign-in
//! through the daemon's own page and the library sync are upstream's
//! (`surface::spotify_auth`, `surface::dj_sync`).

use asupersync::Cx;
use asupersync::http::Client;
use asupersync::runtime::{RuntimeBuilder, reactor::create_reactor};
use fastapi::{JsonSchema, Response, fastapi_openapi};
use fsonos_core::store::{LibraryEntry, SpotifyAlbum, SpotifyCache};
use fsonos_spotify::SpotifyError;
use fsonos_spotify::client::{Endpoints, Pkce, SpotifyConfig, TokenCache};
use fsonos_spotify::library::split_artists;
use fsonos_spotify::session::Session;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, Mutex};

use crate::failure::{ErrorCode, Failure};
use crate::surface::StoreHandle;
use crate::surface::dj_sync::LibrarySyncDto;

#[derive(Debug, Clone, Default, Serialize, JsonSchema)]
pub struct SyncDto {
    pub running: bool,
    pub done: usize,
    pub total: usize,
    pub error: Option<SyncErrorDto>,
}

#[derive(Debug, Clone, Serialize, JsonSchema)]
pub struct SyncErrorDto {
    pub detail: String,
    pub retryable: bool,
    pub retry_at: Option<i64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LibraryDto {
    pub albums: usize,
    pub tracks: usize,
    pub synced_at: Option<i64>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct StatusDto {
    pub configured: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_id: Option<String>,
    pub app_redirect_uri: String,
    pub signed_in: bool,
    pub reauthorize: bool,
    pub library: LibraryDto,
    pub sync: SyncDto,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AlbumDto {
    pub id: String,
    pub title: String,
    pub artist: String,
    pub year: Option<u32>,
    pub tracks: u32,
    pub uri: String,
    pub art_url: Option<String>,
}

impl From<SpotifyAlbum> for AlbumDto {
    fn from(album: SpotifyAlbum) -> Self {
        Self {
            id: album.id,
            title: album.title,
            artist: album.artist,
            year: album.year,
            tracks: album.tracks,
            uri: album.uri,
            art_url: album.art_url,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TrackDto {
    pub id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub uri: String,
    pub duration_secs: Option<u32>,
    pub disc: Option<u32>,
    pub number: Option<u32>,
}
impl From<&LibraryEntry> for TrackDto {
    fn from(row: &LibraryEntry) -> Self {
        Self {
            id: row
                .track
                .source_uri
                .strip_prefix("spotify:track:")
                .unwrap_or_default()
                .into(),
            title: row.track.title.clone(),
            artists: row
                .track
                .artist
                .as_deref()
                .map(split_artists)
                .unwrap_or_default(),
            uri: row.track.source_uri.clone(),
            duration_secs: row.track.duration_secs,
            disc: row.disc_number,
            number: row.track_number,
        }
    }
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct LikedTrackDto {
    pub id: String,
    pub title: String,
    pub artists: Vec<String>,
    pub uri: String,
    pub duration_secs: Option<u32>,
    pub disc: Option<u32>,
    pub number: Option<u32>,
    pub album: Option<String>,
    pub art_url: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct AlbumsDto {
    pub total: usize,
    pub items: Vec<AlbumDto>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct TracksDto {
    pub total: usize,
    pub items: Vec<LikedTrackDto>,
}

#[derive(Default)]
struct State {
    signed_in: bool,
    reauthorize: bool,
    authorizing: bool,
}

/// One authorization state shared by all daemon listeners. Tokens stay in
/// Session and the private token cache, never in DTOs or action entries.
pub struct Spotify {
    config: Option<SpotifyConfig>,
    cache: TokenCache,
    endpoints: Endpoints,
    app_redirect_uri: String,
    state: Mutex<State>,
}

pub const DEFAULT_APP_REDIRECT: &str = "frankensonos://spotify-callback";

#[derive(Deserialize, JsonSchema)]
pub struct ExchangeRequest {
    pub code: String,
    pub code_verifier: String,
    pub redirect_uri: String,
}

#[derive(Serialize, JsonSchema)]
pub struct ExchangeDto {
    pub signed_in: bool,
}

struct Authorizing<'a>(&'a Spotify);
impl Drop for Authorizing<'_> {
    fn drop(&mut self) {
        if let Ok(mut state) = self.0.state.lock() {
            state.authorizing = false;
        }
    }
}

fn validate_exchange(input: &ExchangeRequest, redirect: &str) -> Result<Pkce, Failure> {
    if input.redirect_uri != redirect {
        return Err(Failure::new(
            ErrorCode::SpotifyRedirectNotAllowed,
            "Spotify app redirect is not allowed.",
        ));
    }
    if input.code.is_empty() || input.code.len() > 512 {
        return Err(Failure::invalid("code must contain 1 to 512 bytes"));
    }
    Pkce::from_verifier(input.code_verifier.clone()).map_err(|_| {
        Failure::invalid("code_verifier must be 43 to 128 unreserved ASCII characters")
    })
}

/// Spotify's cap on results per kind for a search, and the default.
pub(crate) const SEARCH_LIMIT: usize = 10;

#[derive(Debug, Serialize, JsonSchema)]
pub struct SearchDto {
    pub albums: Vec<AlbumDto>,
    pub tracks: Vec<LikedTrackDto>,
}

fn art_of(images: &[fsonos_spotify::client::AlbumImage]) -> Option<String> {
    images.first().map(|image| image.url.clone())
}

fn album_dto(album: &fsonos_spotify::client::Album) -> AlbumDto {
    AlbumDto {
        id: album.id.clone(),
        title: album.name.clone(),
        artist: album
            .artists
            .iter()
            .map(|a| a.name.as_str())
            .collect::<Vec<_>>()
            .join(", "),
        year: album
            .release_date
            .as_deref()
            .and_then(|date| date.get(..4))
            .and_then(|year| year.parse().ok()),
        tracks: album.total_tracks,
        uri: album.uri.clone(),
        art_url: art_of(&album.images),
    }
}

fn track_dto(track: &fsonos_spotify::client::FullTrack) -> LikedTrackDto {
    let position = |n: u32| (n > 0).then_some(n);
    LikedTrackDto {
        id: track
            .uri
            .strip_prefix("spotify:track:")
            .unwrap_or_default()
            .into(),
        title: track.name.clone(),
        artists: track.artists.iter().map(|a| a.name.clone()).collect(),
        uri: track.uri.clone(),
        duration_secs: (track.duration_ms > 0)
            .then(|| u32::try_from(track.duration_ms.div_ceil(1000)).ok())
            .flatten(),
        disc: position(track.disc_number),
        number: position(track.track_number),
        album: Some(track.album.name.clone()),
        art_url: art_of(&track.album.images),
    }
}

impl Spotify {
    /// Fail unless the daemon holds a usable sign-in and is not mid-sign-in or
    /// mid-sync. `purpose` completes "Spotify sign-in is required to ...".
    fn ensure_ready(&self, purpose: &str) -> Result<(), Failure> {
        if self.config.is_none() {
            return Err(not_configured());
        }
        let state = self.state.lock().map_err(|_| internal())?;
        if !state.signed_in || state.reauthorize {
            return Err(Failure::new(
                ErrorCode::SpotifyAuthRequired,
                format!("Spotify sign-in is required to {purpose}."),
            ));
        }
        if state.authorizing {
            return Err(busy());
        }
        Ok(())
    }

    /// Search Spotify's whole catalog with the owner's sign-in.
    fn search(&self, query: &str, limit: usize) -> Result<SearchDto, Failure> {
        self.ensure_ready("search Spotify")?;
        let limit = u32::try_from(limit.clamp(1, SEARCH_LIMIT)).unwrap_or(10);
        let url = self.endpoints.search(query, limit);
        let body = std::thread::scope(|scope| {
            scope
                .spawn(|| self.run_session(async |session, cx| session.get(cx, &url).await))
                .join()
                .map_err(|_| internal())?
                .map_err(|err| {
                    if matches!(
                        err,
                        SpotifyError::Auth(_) | SpotifyError::Api { status: 401, .. }
                    ) {
                        if let Ok(mut state) = self.state.lock() {
                            state.reauthorize = true;
                        }
                        return Failure::new(
                            ErrorCode::SpotifyAuthRequired,
                            "Spotify sign-in expired; sign in again to search Spotify.",
                        );
                    }
                    // Spotify's own message says why a search was refused (a plan or
                    // app-mode limit, say); it carries no token.
                    let why = match &err {
                        SpotifyError::Api { status, body } => {
                            format!("Spotify API returned HTTP {status}: {body}")
                        }
                        other => safe_error(other, self.cache.path()),
                    };
                    Failure::new(
                        ErrorCode::UpnpFault,
                        format!("could not search Spotify: {why}"),
                    )
                })
        })?;
        let found = fsonos_spotify::client::SearchResponse::parse(&body).map_err(|err| {
            Failure::new(
                ErrorCode::UpnpFault,
                format!(
                    "could not read Spotify's search reply: {}",
                    safe_error(&err, self.cache.path())
                ),
            )
        })?;
        Ok(SearchDto {
            albums: found.albums().into_iter().map(album_dto).collect(),
            tracks: found.tracks().into_iter().map(track_dto).collect(),
        })
    }

    pub fn new(
        config: Option<SpotifyConfig>,
        data_dir: &Path,
        endpoints: Endpoints,
        app_redirect_uri: String,
    ) -> Result<Arc<Self>, Failure> {
        if let Some(config) = &config {
            config
                .validate()
                .map_err(|_| Failure::invalid("invalid Spotify configuration"))?;
        }
        let cache = TokenCache::in_data_dir(data_dir);
        let signed_in = config.is_some() && cache.load().ok().flatten().is_some();
        Ok(Arc::new(Self {
            config,
            cache,
            endpoints,
            app_redirect_uri,
            state: Mutex::new(State {
                signed_in,
                ..State::default()
            }),
        }))
    }

    async fn exchange(&self, cx: &Cx, input: ExchangeRequest, store: StoreHandle) -> Response {
        let Some(config) = self.config.clone() else {
            return not_configured().http_response();
        };
        let pkce = match validate_exchange(&input, &self.app_redirect_uri) {
            Ok(pkce) => pkce,
            Err(err) => return crate::failure::http_error(&err, 400, false),
        };
        {
            let Ok(mut state) = self.state.lock() else {
                return internal().http_response();
            };
            if state.authorizing {
                return crate::failure::http_error(&busy(), 409, true);
            }
            state.authorizing = true;
        }
        let _authorizing = Authorizing(self);
        let result = async {
            let mut session =
                Session::open(config, self.cache.clone(), Client::default_for_runtime(cx))?
                    .with_endpoints(self.endpoints.clone());
            session
                .exchange_code(cx, &input.code, &pkce, &input.redirect_uri)
                .await?;
            store
                .lock()
                .map_err(|_| SpotifyError::Config("store unavailable".into()))?
                .clear_spotify_library()?;
            Ok::<_, SpotifyError>(())
        }
        .await;
        let Ok(mut state) = self.state.lock() else {
            return internal().http_response();
        };
        match result {
            Ok(()) => {
                state.signed_in = true;
                state.reauthorize = false;
                Response::json(&ExchangeDto { signed_in: true })
                    .expect("ExchangeDto serializes")
                    .header("cache-control", b"no-store".to_vec())
            }
            Err(err) => {
                if matches!(err, SpotifyError::Io(_) | SpotifyError::Store(_)) {
                    state.signed_in = false;
                }
                let (status, retryable) = match err {
                    SpotifyError::Auth(_) => (400, false),
                    SpotifyError::Http(_) | SpotifyError::Api { .. } | SpotifyError::Decode(_) => {
                        (502, true)
                    }
                    _ => (500, false),
                };
                crate::failure::http_error(
                    &Failure::new(
                        ErrorCode::SpotifyAuthRequired,
                        safe_error(&err, self.cache.path()),
                    ),
                    status,
                    retryable,
                )
            }
        }
    }

    fn run_session<T>(
        &self,
        work: impl for<'a> AsyncFnOnce(&'a mut Session, &'a Cx) -> Result<T, SpotifyError>,
    ) -> Result<T, SpotifyError> {
        let rt = RuntimeBuilder::current_thread()
            .with_reactor(create_reactor().map_err(|e| SpotifyError::Http(e.to_string()))?)
            .blocking_threads(0, 4)
            .build()
            .map_err(|e| SpotifyError::Http(e.to_string()))?;
        rt.block_on(async {
            let cx = Cx::current()
                .ok_or_else(|| SpotifyError::Http("runtime context missing".into()))?;
            let mut session = Session::open(
                self.config
                    .clone()
                    .ok_or_else(|| SpotifyError::Config("not configured".into()))?,
                self.cache.clone(),
                Client::default_for_runtime(&cx),
            )?
            .with_endpoints(self.endpoints.clone());
            work(&mut session, &cx).await
        })
    }

    pub fn status(&self, cache: &SpotifyCache, sync: SyncDto) -> Result<StatusDto, Failure> {
        let state = self.state.lock().map_err(|_| internal())?;
        Ok(StatusDto {
            configured: self.config.is_some(),
            client_id: self.config.as_ref().map(|config| config.client_id.clone()),
            app_redirect_uri: self.app_redirect_uri.clone(),
            signed_in: state.signed_in,
            reauthorize: state.reauthorize,
            library: library_status(cache),
            sync,
        })
    }
}

fn sync_dto(state: &LibrarySyncDto) -> SyncDto {
    let (done, total) = state.progress.map_or((0, 0), |p| (p.done, p.total));
    SyncDto {
        running: state.running,
        done,
        total,
        error: state.error.as_ref().map(|detail| SyncErrorDto {
            detail: detail.clone(),
            retryable: true,
            retry_at: None,
        }),
    }
}

pub(crate) fn library_status(cache: &SpotifyCache) -> LibraryDto {
    LibraryDto {
        albums: cache.albums.iter().filter(|a| a.saved).count(),
        tracks: cache.liked_uris.len(),
        synced_at: cache.synced_at,
    }
}

pub(crate) fn not_configured() -> Failure {
    Failure::new(ErrorCode::SpotifyNotConfigured, "Spotify sign-in is not configured.")
        .with_hint("Set FSONOS_SPOTIFY_CLIENT_ID and register FSONOS_SPOTIFY_REDIRECT_URI in the Spotify dashboard.")
}
fn busy() -> Failure {
    Failure::new(
        ErrorCode::NotReady,
        "Spotify is busy; retry after sync or sign-in finishes.",
    )
    .with_hint("Wait for Spotify sign-in or sync to finish, then retry.")
}
fn internal() -> Failure {
    Failure::new(ErrorCode::Internal, "Spotify worker unavailable.")
}
fn safe_error(err: &SpotifyError, path: &Path) -> String {
    match err {
        SpotifyError::Auth(text) => {
            for code in [
                "invalid_grant",
                "invalid_client",
                "invalid_scope",
                "access_denied",
                "temporarily_unavailable",
                "server_error",
            ] {
                if text.contains(code) {
                    return format!("Spotify: {code}");
                }
            }
            "Spotify authorization failed. Start sign-in again.".into()
        }
        SpotifyError::Io(err) => format!(
            "Cannot read or write Spotify token cache {}: {}",
            path.display(),
            err.kind()
        ),
        SpotifyError::Api { status, .. } => format!("Spotify API returned HTTP {status}."),
        SpotifyError::Http(_) => "Spotify request failed. Check the connection and retry.".into(),
        SpotifyError::Decode(_) => "Spotify returned an unreadable response.".into(),
        SpotifyError::Store(_) | SpotifyError::Config(_) | SpotifyError::UnknownMood { .. } => {
            "Could not sync the Spotify library cache.".into()
        }
    }
}

impl crate::Surface {
    fn spotify_admit(
        &self,
        client: &fsonos_core::policy::Client,
        operation: &str,
        read: bool,
    ) -> Result<(), Failure> {
        self.guard(client).authorize(operation, read)
    }

    pub(crate) async fn spotify_exchange(
        &self,
        cx: &Cx,
        client: &fsonos_core::policy::Client,
        input: ExchangeRequest,
    ) -> Response {
        if let Err(err) = self.spotify_admit(client, "spotify_exchange", false) {
            return err.http_response();
        }
        let Some(spotify) = &self.spotify else {
            return not_configured().http_response();
        };
        let Some(store) = self.spotify_store() else {
            return internal().http_response();
        };
        spotify.exchange(cx, input, store).await
    }

    pub(crate) fn spotify_status(
        &self,
        client: &fsonos_core::policy::Client,
    ) -> Result<StatusDto, Failure> {
        self.spotify_admit(client, "spotify_status", true)?;
        let cache = self.with_store(|s| s.spotify_cache())?.unwrap_or_default();
        let sync = self
            .dj_engine()
            .and_then(crate::dj::DjEngine::library_sync)
            .map(|state| sync_dto(&state))
            .unwrap_or_default();
        match &self.spotify {
            Some(spotify) => spotify.status(&cache, sync),
            None => Ok(StatusDto {
                configured: false,
                client_id: None,
                app_redirect_uri: DEFAULT_APP_REDIRECT.into(),
                signed_in: false,
                reauthorize: false,
                library: library_status(&cache),
                sync,
            }),
        }
    }

    pub(crate) fn spotify_albums(
        &self,
        client: &fsonos_core::policy::Client,
        offset: usize,
        limit: usize,
        q: &str,
    ) -> Result<AlbumsDto, Failure> {
        self.spotify_admit(client, "list_spotify_albums", true)?;
        let cache = self.with_store(|s| s.spotify_cache())?.unwrap_or_default();
        let q = q.to_lowercase();
        let albums: Vec<_> = cache
            .albums
            .into_iter()
            .filter(|album| {
                album.saved
                    && format!("{} {}", album.title, album.artist)
                        .to_lowercase()
                        .contains(&q)
            })
            .collect();
        Ok(AlbumsDto {
            total: albums.len(),
            items: albums
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(AlbumDto::from)
                .collect(),
        })
    }

    pub(crate) fn spotify_search(
        &self,
        client: &fsonos_core::policy::Client,
        query: &str,
        limit: usize,
    ) -> Result<SearchDto, Failure> {
        self.spotify_admit(client, "search_spotify", true)?;
        self.spotify
            .as_ref()
            .ok_or_else(not_configured)?
            .search(query, limit)
    }

    pub(crate) fn spotify_album_tracks(
        &self,
        client: &fsonos_core::policy::Client,
        id: &str,
    ) -> Result<Vec<TrackDto>, Failure> {
        self.spotify_admit(client, "list_spotify_album_tracks", true)?;
        let uri = format!("spotify:album:{id}");
        let rows = self
            .with_store(|s| {
                let cache = s.spotify_cache()?;
                if !cache.albums.iter().any(|a| a.saved && a.uri == uri) {
                    return Ok(Vec::new());
                }
                let active: std::collections::HashSet<_> = cache.track_uris.into_iter().collect();
                Ok(s.library()?
                    .into_iter()
                    .filter(|row| active.contains(&row.track.source_uri))
                    .collect::<Vec<_>>())
            })?
            .unwrap_or_default();
        let mut tracks: Vec<_> = rows
            .iter()
            .filter(|row| row.album_uri.as_deref() == Some(uri.as_str()))
            .map(TrackDto::from)
            .collect();
        tracks.sort_by(|a, b| (a.disc, a.number, &a.uri).cmp(&(b.disc, b.number, &b.uri)));
        Ok(tracks)
    }

    pub(crate) fn spotify_tracks(
        &self,
        client: &fsonos_core::policy::Client,
        offset: usize,
        limit: usize,
        q: &str,
    ) -> Result<TracksDto, Failure> {
        self.spotify_admit(client, "list_spotify_tracks", true)?;
        let (cache, rows) = self
            .with_store(|s| Ok((s.spotify_cache()?, s.library()?)))?
            .unwrap_or_default();
        let q = q.to_lowercase();
        let liked: std::collections::HashSet<_> =
            cache.liked_uris.iter().map(String::as_str).collect();
        let tracks: Vec<_> = rows
            .iter()
            .filter(|row| {
                liked.contains(row.track.source_uri.as_str())
                    && format!(
                        "{} {} {}",
                        row.track.title,
                        row.track.artist.as_deref().unwrap_or_default(),
                        row.track.album.as_deref().unwrap_or_default()
                    )
                    .to_lowercase()
                    .contains(&q)
            })
            .collect();
        Ok(TracksDto {
            total: tracks.len(),
            items: tracks
                .into_iter()
                .skip(offset)
                .take(limit)
                .map(|row| {
                    let track = TrackDto::from(row);
                    LikedTrackDto {
                        id: track.id,
                        title: track.title,
                        artists: track.artists,
                        uri: track.uri,
                        duration_secs: track.duration_secs,
                        disc: track.disc,
                        number: track.number,
                        album: row.track.album.clone(),
                        art_url: cache
                            .albums
                            .iter()
                            .find(|a| Some(&a.uri) == row.album_uri.as_ref())
                            .and_then(|a| a.art_url.clone()),
                    }
                })
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_verifier_and_code_boundaries_are_validated_without_echoing_input() {
        for verifier in ["a".repeat(43), "b".repeat(128), "-._~".repeat(16)] {
            for code in ["x".into(), "x".repeat(512)] {
                let req = ExchangeRequest {
                    code,
                    code_verifier: verifier.clone(),
                    redirect_uri: DEFAULT_APP_REDIRECT.into(),
                };
                assert!(validate_exchange(&req, DEFAULT_APP_REDIRECT).is_ok());
            }
        }
        for verifier in [
            "a".repeat(42),
            "a".repeat(129),
            "é".repeat(43),
            format!("{}!", "a".repeat(43)),
        ] {
            let req = ExchangeRequest {
                code: "private-code".into(),
                code_verifier: verifier.clone(),
                redirect_uri: DEFAULT_APP_REDIRECT.into(),
            };
            let err = validate_exchange(&req, DEFAULT_APP_REDIRECT).unwrap_err();
            assert_eq!(err.code, ErrorCode::InvalidArgument);
            assert!(!err.detail.contains(&verifier));
            assert!(!err.detail.contains("private-code"));
        }
        for code in [String::new(), "x".repeat(513)] {
            let req = ExchangeRequest {
                code,
                code_verifier: "a".repeat(43),
                redirect_uri: DEFAULT_APP_REDIRECT.into(),
            };
            assert_eq!(
                validate_exchange(&req, DEFAULT_APP_REDIRECT)
                    .unwrap_err()
                    .code,
                ErrorCode::InvalidArgument
            );
        }
    }

    #[test]
    fn status_reports_configured_app_redirect_and_client_id() {
        let data_dir = fsonos_spotify::fake_spotify::scratch_dir("status-pure");
        for configured in [false, true] {
            let spotify = Spotify::new(
                configured.then(fsonos_spotify::fake_spotify::config),
                &data_dir,
                Endpoints::default(),
                "frankensonos://custom-callback".into(),
            )
            .unwrap();
            let status = serde_json::to_value(
                spotify
                    .status(&SpotifyCache::default(), SyncDto::default())
                    .unwrap(),
            )
            .unwrap();
            assert_eq!(status["app_redirect_uri"], "frankensonos://custom-callback");
            if configured {
                assert_eq!(status["client_id"], fsonos_spotify::fake_spotify::CLIENT_ID);
            } else {
                assert!(status.get("client_id").is_none());
            }
            assert_eq!(status["signed_in"], false);
        }
    }

    #[test]
    fn errors_never_repeat_upstream_tokens() {
        let err =
            SpotifyError::Auth("token endpoint 400: invalid_grant (access-1 refresh-1)".into());
        assert_eq!(
            safe_error(&err, Path::new("/tmp/cache")),
            "Spotify: invalid_grant"
        );
    }
}
