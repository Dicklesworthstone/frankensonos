//! Cached library browsing needs neither a running DJ nor speaker discovery.

use clap::Args;
use fsonos_api::surface::dj_sync::library::{BrowsePage, BrowseQuery};
use fsonos_api::surface::dj_sync::playlist::{PlaylistPage, PlaylistQuery};
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::policy::Client;

use crate::config::GlobalArgs;
use crate::remote::Daemon;
use crate::{daemon, emit};

#[derive(Debug, Args)]
pub struct BrowseArgs {
    /// Cached category; open album or playlist tracks with their URI flag.
    #[arg(default_value = "tracks", value_parser = ["tracks", "albums", "playlists"])]
    kind: String,
    /// Words to match in title, artist and album. Quote a multi-word filter.
    #[arg(long)]
    q: Option<String>,
    /// With tracks: this album's cached tracks, in disc/track order.
    #[arg(
        long,
        value_name = "SPOTIFY_ALBUM_URI",
        conflicts_with = "playlist_uri"
    )]
    album_uri: Option<String>,
    /// With tracks: this playlist's cached occurrences, in original order.
    /// Repeated tracks retain their separate positions; coverage may be partial.
    #[arg(
        long,
        value_name = "SPOTIFY_PLAYLIST_URI",
        conflicts_with = "album_uri"
    )]
    playlist_uri: Option<String>,
    /// Zero-based offset, using the previous page's next_offset.
    #[arg(long, default_value_t = 0)]
    offset: u32,
    /// Maximum entries to return; never implicitly fetch every page.
    #[arg(long, default_value_t = 50, value_parser = clap::value_parser!(u32).range(1..=50))]
    limit: u32,
    /// Reject a changed listing. Keep filters when continuing; start new
    /// filters at offset zero without the previous revision. Required for
    /// playlist contents when offset is greater than zero.
    #[arg(long, value_name = "REVISION")]
    expected_revision: Option<String>,
}

impl BrowseArgs {
    fn query(&self) -> BrowseQuery {
        BrowseQuery {
            kind: Some(self.kind.clone()),
            q: self.q.clone(),
            album_uri: self.album_uri.clone(),
            offset: Some(usize::try_from(self.offset).unwrap_or(usize::MAX)),
            limit: Some(usize::try_from(self.limit).unwrap_or(usize::MAX)),
        }
    }

    fn playlist_query(&self) -> Result<Option<PlaylistQuery>, Failure> {
        let Some(playlist_uri) = &self.playlist_uri else {
            return Ok(None);
        };
        if self.kind != "tracks" || self.album_uri.is_some() {
            return Err(Failure::new(
                ErrorCode::InvalidArgument,
                "--playlist-uri requires tracks and cannot be combined with --album-uri",
            ));
        }
        let query = PlaylistQuery {
            playlist_uri: playlist_uri.clone(),
            offset: Some(usize::try_from(self.offset).unwrap_or(usize::MAX)),
            limit: Some(usize::try_from(self.limit).unwrap_or(usize::MAX)),
            q: self.q.clone(),
            expected_revision: self.expected_revision.clone(),
        };
        query.validate()?;
        Ok(Some(query))
    }
}

fn read(global: &GlobalArgs, args: &BrowseArgs) -> Result<BrowsePage, Failure> {
    let query = args.query();
    // Fail invalid filters before even probing the daemon. Both modes use
    // exactly the same query validation, page checks and cached projection.
    let path = query.path()?;
    let page = if let Some(remote) = Daemon::find(global, global.daemon)? {
        // Once a daemon answered, an error must not silently switch caches.
        remote.get::<BrowsePage>(&path)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        let surface = daemon::with_action_log(surface, &dir, "cli");
        // Surface construction is lazy; unlike Direct::survey, this path
        // reads only the store and never calls its discovery closure.
        surface.browse_library(&Client::Cli, &query)?
    };
    page.validate_for(&query, args.expected_revision.as_deref())?;
    Ok(page)
}

fn read_playlist(global: &GlobalArgs, query: &PlaylistQuery) -> Result<PlaylistPage, Failure> {
    let path = query.path()?;
    let page = if let Some(remote) = Daemon::find(global, global.daemon)? {
        remote.get::<PlaylistPage>(&path)?
    } else {
        let dir = daemon::data_dir(global)?;
        let surface = daemon::surface(global, daemon::policy(&dir)?)?;
        let surface = daemon::with_action_log(surface, &dir, "cli");
        surface.playlist_contents(&Client::Cli, query)?
    };
    page.validate_for(query)?;
    Ok(page)
}

pub(super) fn run(global: &GlobalArgs, args: &BrowseArgs) -> anyhow::Result<()> {
    if let Some(query) = args.playlist_query()? {
        let page = read_playlist(global, &query)?;
        return emit(global.json, &page, |page| format!("{}\n", page.text()));
    }
    let page = read(global, args)?;
    emit(global.json, &page, |page| format!("{}\n", page.text()))
}

#[cfg(test)]
mod tests;
