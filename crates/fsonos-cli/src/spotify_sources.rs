//! Catalog search and artist/show playback use the owner's existing Spotify
//! sign-in before any speaker command. Each read has its own runtime; one
//! admission slot bounds concurrency and retains Spotify's cooldown between
//! fresh sessions, so later sign-ins and saved app settings are picked up.

use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::{Mutex, TryLockError};
use std::time::{Duration, Instant};

use asupersync::Cx;
use asupersync::http::Client;
use asupersync::time::{timeout, wall_now};
use fsonos_api::source::{
    CatalogItem, CatalogKind, CatalogPage, SourceSelection, SourceSelectionKind, SpotifySources,
};
use fsonos_api::{ErrorCode, Failure, Surface};
use fsonos_spotify::SpotifyError;
use fsonos_spotify::artist_playback::{self, SelectionSource};
use fsonos_spotify::client::{Endpoints, SpotifyConfig, TokenCache};
use fsonos_spotify::session::Session;
use fsonos_spotify::{catalog_search, show_playback};

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
                "Spotify catalog access needs a configured Spotify app and sign-in",
            )
            .with_hint(
                "Run fsonos setup to save your Spotify app and sign in, then retry the catalog request.",
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
    fn artist_tracks(&self, artist_uri: &str) -> Result<SourceSelection, Failure> {
        self.resolve(artist_uri, false)
    }

    fn show_episodes(&self, show_uri: &str) -> Result<SourceSelection, Failure> {
        self.resolve(show_uri, true)
    }

    fn search_catalog(
        &self,
        query: &str,
        kind: CatalogKind,
        limit: u32,
    ) -> Result<CatalogPage, Failure> {
        let query = query.to_owned();
        self.request(move |session, cx| {
            Box::pin(async move {
                let page = catalog_search::search(session, cx, &query, kind.as_str(), limit).await?;
                Ok(CatalogPage {
                    items: page
                        .items
                        .into_iter()
                        .map(|item| CatalogItem {
                            source_uri: item.source_uri,
                            title: item.title,
                            subtitle: item.subtitle,
                        })
                        .collect(),
                    more: page.more,
                })
            })
        })
    }
}

type CatalogFuture<'a, T> = Pin<Box<dyn Future<Output = Result<T, SpotifyError>> + 'a>>;

impl Sources {
    fn resolve(&self, source_uri: &str, show: bool) -> Result<SourceSelection, Failure> {
        let uri = source_uri.to_owned();
        self.request(move |session, cx| {
            Box::pin(async move { select(session, cx, &uri, show).await })
        })
    }

    fn request<T, F>(&self, operation: F) -> Result<T, Failure>
    where
        T: Send + 'static,
        F: for<'a> FnOnce(&'a mut Session, &'a Cx) -> CatalogFuture<'a, T> + Send + 'static,
    {
        let mut state = match self.state.try_lock() {
            Ok(state) => state,
            Err(TryLockError::WouldBlock) => {
                return Err(Failure::new(
                    ErrorCode::NotReady,
                    "a Spotify catalog request is already being read",
                )
                .with_hint("Wait for that catalog request to finish before requesting another."));
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
        let worker = std::thread::Builder::new()
            .name("spotify-catalog-read".into())
            .stack_size(STACK)
            .spawn(move || read(config, cache, endpoints, operation))
            .map_err(|error| {
                Failure::new(
                    ErrorCode::Internal,
                    format!("cannot start Spotify catalog request: {error}"),
                )
            })?;
        let (result, retry_after) = worker.join().map_err(|_| {
            Failure::new(
                ErrorCode::Internal,
                "the Spotify catalog worker stopped unexpectedly",
            )
        })?;
        state.record(retry_after);
        if retry_after > 0 && matches!(&result, Err(SpotifyError::Api { status: 429, .. })) {
            return Err(cooldown(retry_after));
        }
        result.map_err(failure)
    }
}

fn read<T, F>(
    config: SpotifyConfig,
    cache: TokenCache,
    endpoints: Endpoints,
    operation: F,
) -> (Result<T, SpotifyError>, u64)
where
    F: for<'a> FnOnce(&'a mut Session, &'a Cx) -> CatalogFuture<'a, T>,
{
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
            operation(&mut session, &cx),
        )
        .await
        .unwrap_or_else(|_| {
            Err(SpotifyError::Http(
                "Spotify catalog lookup exceeded its 45-second deadline".into(),
            ))
        });
        (selected, session.retry_after_secs())
    })
}

async fn select(
    session: &mut Session,
    cx: &Cx,
    uri: &str,
    show: bool,
) -> Result<SourceSelection, SpotifyError> {
    if show {
        let selection = show_playback::resolve(session, cx, uri).await?;
        Ok(SourceSelection {
            tracks: selection
                .episodes
                .into_iter()
                .map(|episode| (episode.episode_uri, episode.title))
                .collect(),
            kind: SourceSelectionKind::ShowEpisodes,
        })
    } else {
        let selection = artist_playback::resolve(session, cx, uri).await?;
        Ok(SourceSelection {
            tracks: selection
                .tracks
                .into_iter()
                .map(|track| (track.source_uri, track.title))
                .collect(),
            kind: match selection.source {
                SelectionSource::TopTracks => SourceSelectionKind::TopTracks,
                SelectionSource::CatalogSample => SourceSelectionKind::CatalogSample,
            },
        })
    }
}

fn cooldown(seconds: u64) -> Failure {
    Failure::new(ErrorCode::NotReady, format!("Spotify catalog lookup is rate-limited; retry in {seconds} seconds"))
        .with_hint("Wait for Spotify's cooldown before trying this source again. Existing playback is unchanged.")
}

fn failure(error: SpotifyError) -> Failure {
    match error {
        SpotifyError::Auth(detail) => Failure::new(ErrorCode::SpotifyAuthRequired, detail)
            .with_hint("Sign in with fsonos setup, then retry the catalog request."),
        SpotifyError::Api { status: 401, .. } => Failure::new(
            ErrorCode::SpotifyAuthRequired,
            "Spotify did not accept the catalog lookup's sign-in",
        )
        .with_hint("Sign in with fsonos setup, then retry the catalog request."),
        SpotifyError::Config(detail) => Failure::invalid(detail),
        other => Failure::new(
            ErrorCode::NotReady,
            format!("Spotify catalog request: {other}"),
        )
        .with_hint("Try a specific track, album or episode, or retry when Spotify's catalog is available."),
    }
}

#[cfg(test)]
mod tests;
