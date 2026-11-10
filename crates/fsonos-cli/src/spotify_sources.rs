//! Explicit artist playback resolves through the owner's existing Spotify
//! sign-in before any speaker command. Each read has its own runtime; one
//! admission slot bounds concurrency and retains Spotify's cooldown between
//! fresh sessions, so later sign-ins and saved app settings are picked up.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, TryLockError};
use std::time::{Duration, Instant};

use asupersync::Cx;
use asupersync::http::Client;
use asupersync::time::{timeout, wall_now};
use fsonos_api::source::{ArtistSelection, ArtistSelectionKind, SpotifySources};
use fsonos_api::{ErrorCode, Failure, Surface};
use fsonos_spotify::SpotifyError;
use fsonos_spotify::artist_playback::{self, SelectionSource};
use fsonos_spotify::client::{Endpoints, SpotifyConfig, TokenCache};
use fsonos_spotify::session::Session;

use crate::config::{SPOTIFY_REDIRECT_URI, ServeArgs};

const STACK: usize = 8 * 1024 * 1024;
/// Includes all catalog pages, retries and any reactive token refresh.
const LOOKUP_LIMIT: Duration = Duration::from_secs(45);

/// Install even before setup: the next request reads newly saved app settings
/// and credentials. Serve's explicit options retain their normal precedence.
pub fn install(surface: Surface, data_dir: &Path, serve: Option<&ServeArgs>) -> Surface {
    surface.with_spotify_sources(Box::new(Sources {
        data_dir: data_dir.to_path_buf(),
        client_id: serve.and_then(|args| args.spotify_client_id.clone()),
        redirect_uri: serve.map(|args| args.spotify_redirect_uri.clone()),
        endpoints: Endpoints::for_session(),
        state: Mutex::new(State::default()),
    }))
}

struct Sources {
    data_dir: PathBuf,
    client_id: Option<String>,
    redirect_uri: Option<String>,
    endpoints: Endpoints,
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    /// A duration from a monotonic observation avoids deadline overflow when
    /// Spotify asks for an exceptionally long delay.
    cooldown: Option<(Instant, Duration)>,
}

impl State {
    fn remaining(&self) -> u64 {
        let left = self.cooldown.map_or(Duration::ZERO, |(at, delay)| {
            delay.saturating_sub(at.elapsed())
        });
        left.as_secs()
            .saturating_add(u64::from(left.subsec_nanos() != 0))
    }

    fn record(&mut self, retry_after: u64) {
        self.cooldown =
            (retry_after > 0).then(|| (Instant::now(), Duration::from_secs(retry_after)));
    }
}

impl Sources {
    fn configuration(&self) -> Result<SpotifyConfig, Failure> {
        let environment = std::env::var("FSONOS_SPOTIFY_CLIENT_ID").ok();
        let client_id = crate::spotify_app::client_id(
            self.client_id.as_deref().or(environment.as_deref()),
            &self.data_dir,
        )
        .ok_or_else(|| {
            Failure::new(
                ErrorCode::SpotifyAuthRequired,
                "artist playback needs a configured Spotify app and sign-in",
            )
            .with_hint(
                "Run fsonos setup to save your Spotify app and sign in, then retry the artist.",
            )
        })?;
        let redirect_uri = self
            .redirect_uri
            .clone()
            .or_else(|| std::env::var("FSONOS_SPOTIFY_REDIRECT_URI").ok())
            .unwrap_or_else(|| SPOTIFY_REDIRECT_URI.into());
        let config = SpotifyConfig {
            client_id,
            redirect_uri,
        };
        config.validate().map_err(failure)?;
        Ok(config)
    }
}

impl SpotifySources for Sources {
    fn artist_tracks(&self, artist_uri: &str) -> Result<ArtistSelection, Failure> {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => {
                return Err(Failure::new(
                    ErrorCode::NotReady,
                    "a Spotify artist selection is already being read",
                )
                .with_hint("Wait for that selection to finish before requesting another artist."));
            }
            Err(TryLockError::Poisoned(error)) => error.into_inner(),
        };
        let remaining = state.remaining();
        if remaining > 0 {
            return Err(cooldown(remaining));
        }
        let config = self.configuration()?;
        let cache = TokenCache::in_data_dir(&self.data_dir);
        let endpoints = self.endpoints.clone();
        let uri = artist_uri.to_owned();
        let worker = std::thread::Builder::new()
            .name("spotify-artist-selection".into())
            .stack_size(STACK)
            .spawn(move || read(config, cache, endpoints, &uri))
            .map_err(|error| {
                Failure::new(
                    ErrorCode::Internal,
                    format!("cannot start Spotify artist selection: {error}"),
                )
            })?;
        let (result, retry_after) = worker.join().map_err(|_| {
            Failure::new(
                ErrorCode::Internal,
                "the Spotify artist selection worker stopped unexpectedly",
            )
        })?;
        state.record(retry_after);
        if retry_after > 0 && matches!(&result, Err(SpotifyError::Api { status: 429, .. })) {
            return Err(cooldown(retry_after));
        }
        result.map_err(failure)
    }
}

fn read(
    config: SpotifyConfig,
    cache: TokenCache,
    endpoints: Endpoints,
    uri: &str,
) -> (Result<ArtistSelection, SpotifyError>, u64) {
    let runtime = match crate::runtime() {
        Ok(runtime) => runtime,
        Err(error) => return (Err(SpotifyError::Http(format!("{error:#}"))), 0),
    };
    runtime.block_on(async {
        let Some(cx) = Cx::current() else {
            return (Err(SpotifyError::Http("no async context".into())), 0);
        };
        let mut session = match Session::open(config, cache, Client::default_for_runtime(&cx)) {
            Ok(session) => session.with_endpoints(endpoints),
            Err(error) => return (Err(error), 0),
        };
        let now = cx.timer_driver().map_or_else(wall_now, |timer| timer.now());
        let selected = timeout(
            now,
            LOOKUP_LIMIT,
            Box::pin(artist_playback::resolve(&mut session, &cx, uri)),
        )
        .await
        .unwrap_or_else(|_| {
            Err(SpotifyError::Http(
                "artist catalog lookup exceeded its 45-second deadline".into(),
            ))
        });
        let result = selected.map(|selection| ArtistSelection {
            tracks: selection
                .tracks
                .into_iter()
                .map(|track| (track.source_uri, track.title))
                .collect(),
            kind: match selection.source {
                SelectionSource::TopTracks => ArtistSelectionKind::TopTracks,
                SelectionSource::CatalogSample => ArtistSelectionKind::CatalogSample,
            },
        });
        (result, session.retry_after_secs())
    })
}

fn cooldown(seconds: u64) -> Failure {
    Failure::new(ErrorCode::NotReady, format!("Spotify artist lookup is rate-limited; retry in {seconds} seconds"))
        .with_hint("Wait for Spotify's cooldown before trying this artist again. Existing playback is unchanged.")
}

fn failure(error: SpotifyError) -> Failure {
    match error {
        SpotifyError::Auth(detail) => Failure::new(ErrorCode::SpotifyAuthRequired, detail)
            .with_hint("Sign in with fsonos setup, then retry the artist."),
        SpotifyError::Api { status: 401, .. } => Failure::new(
            ErrorCode::SpotifyAuthRequired,
            "Spotify did not accept the artist lookup's sign-in",
        )
        .with_hint("Sign in with fsonos setup, then retry the artist."),
        SpotifyError::Config(detail) => Failure::invalid(detail),
        other => Failure::new(
            ErrorCode::NotReady,
            format!("Spotify artist selection: {other}"),
        )
        .with_hint("Try a specific track or album, or retry when Spotify's catalog is available."),
    }
}

#[cfg(test)]
mod tests;
