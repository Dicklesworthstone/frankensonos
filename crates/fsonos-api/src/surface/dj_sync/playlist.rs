//! Browsing ordered occurrences from a cached playlist observation. Reads use
//! only the local store; positions identify the source playlist, not a queue.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use fsonos_core::store::{CachedPlaylist, PlaylistTrack};
use fsonos_types::text::normalize;
use serde::{Deserialize, Serialize};

use super::super::Surface;
use crate::failure::{ErrorCode, Failure};

pub use super::library::MAX_PAGE;

mod client;

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlaylistQuery {
    /// Canonical spotify:playlist URI from the cached playlist listing.
    pub playlist_uri: String,
    /// Zero-based offset in the filtered cached occurrences; default zero.
    pub offset: Option<usize>,
    /// One to fifty occurrences; default fifty.
    pub limit: Option<usize>,
    /// All words must match a title, artist or album; at most 512 characters.
    pub q: Option<String>,
    /// Local observation revision; required when offset is greater than zero.
    pub expected_revision: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlaylistItem {
    /// One-based original playlist position, including skipped source entries.
    pub position: u32,
    pub source_uri: String,
    pub title: String,
    pub subtitle: String,
    pub album: Option<String>,
    pub album_uri: Option<String>,
    pub disc_number: Option<u32>,
    pub track_number: Option<u32>,
    pub duration_secs: Option<u32>,
    pub explicit: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlaylistPage {
    pub playlist_uri: String,
    pub name: String,
    /// The requested filter, with surrounding whitespace removed.
    pub q: String,
    /// complete, partial, or not_cached. Complete means pagination ended;
    /// the observation is neither atomic nor a claim about Spotify now.
    pub coverage: String,
    pub fetched_at: Option<i64>,
    /// Source entries advertised by Spotify, including unplayable entries.
    pub source_total: Option<u32>,
    /// Raw source entries examined before filtering unplayable music.
    pub examined: u32,
    /// Playable occurrences in this observation before the query filter.
    pub cached_total: usize,
    /// Cached occurrences matching the filter.
    pub total: usize,
    pub offset: usize,
    pub limit: usize,
    pub next_offset: Option<usize>,
    pub revision: Option<String>,
    pub items: Vec<PlaylistItem>,
}

fn source(uri: &str, kind: &str) -> bool {
    uri.strip_prefix(&format!("spotify:{kind}:"))
        .is_some_and(|id| {
            !id.is_empty() && id.len() <= 128 && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
        })
}

impl PlaylistQuery {
    pub fn validate(&self) -> Result<(), Failure> {
        if !source(&self.playlist_uri, "playlist") {
            return Err(Failure::invalid(
                "playlist_uri must be a canonical spotify:playlist:<id> URI",
            ));
        }
        if self
            .limit
            .is_some_and(|limit| limit == 0 || limit > MAX_PAGE)
            || self
                .offset
                .is_some_and(|offset| u32::try_from(offset).is_err())
        {
            return Err(Failure::invalid(
                "limit must be 1 to 50 and offset must fit a 32-bit index",
            ));
        }
        if self
            .q
            .as_ref()
            .is_some_and(|query| query.chars().count() > 512)
        {
            return Err(Failure::invalid(
                "the playlist filter must have at most 512 characters",
            ));
        }
        if self.expected_revision.as_ref().is_some_and(|revision| {
            revision.trim().is_empty()
                || revision.len() > 256
                || revision.chars().any(char::is_control)
        }) {
            return Err(Failure::invalid(
                "expected_revision must be a nonblank cached revision of at most 256 bytes",
            ));
        }
        if self.offset.unwrap_or_default() > 0 && self.expected_revision.is_none() {
            return Err(Failure::invalid(
                "continuing a cached playlist requires expected_revision",
            )
            .with_hint("Read offset 0 first, then keep its revision and filter when paging."));
        }
        Ok(())
    }
}

fn changed() -> Failure {
    Failure::invalid("the cached playlist changed while paging").with_hint(
        "Restart at offset 0 without expected_revision, keeping the playlist and filter. Do not combine old and new pages.",
    )
}

fn item(track: PlaylistTrack) -> PlaylistItem {
    PlaylistItem {
        position: track.position,
        source_uri: track.source_uri,
        title: track.title,
        subtitle: track.artist.unwrap_or_default(),
        album: track.album,
        album_uri: track.album_uri.filter(|uri| source(uri, "album")),
        disc_number: track.disc_number,
        track_number: track.track_number,
        duration_secs: track.duration_secs,
        explicit: track.explicit,
    }
}

fn matches(item: &PlaylistItem, filter: &str) -> bool {
    let fields = normalize(&format!(
        "{} {} {}",
        item.title,
        item.subtitle,
        item.album.as_deref().unwrap_or("")
    ));
    filter.split_whitespace().all(|term| fields.contains(term))
}

fn page(
    query: &PlaylistQuery,
    name: String,
    contents: Option<CachedPlaylist>,
) -> Result<PlaylistPage, Failure> {
    query.validate()?;
    let revision = contents.as_ref().map(|contents| contents.revision.clone());
    if query
        .expected_revision
        .as_ref()
        .is_some_and(|expected| Some(expected) != revision.as_ref())
    {
        return Err(changed());
    }
    let mut page = PlaylistPage {
        playlist_uri: query.playlist_uri.clone(),
        name,
        q: query.q.as_deref().unwrap_or("").trim().to_owned(),
        coverage: "not_cached".into(),
        fetched_at: None,
        source_total: None,
        examined: 0,
        cached_total: 0,
        total: 0,
        offset: query.offset.unwrap_or_default(),
        limit: query.limit.unwrap_or(MAX_PAGE),
        next_offset: None,
        revision,
        items: Vec::new(),
    };
    if let Some(contents) = contents {
        if contents.playlist_uri != query.playlist_uri {
            return Err(Failure::new(
                ErrorCode::Internal,
                "cached playlist identity differs",
            ));
        }
        page.coverage = if contents.complete {
            "complete"
        } else {
            "partial"
        }
        .into();
        page.fetched_at = Some(contents.fetched_at);
        page.source_total = contents.source_total;
        page.examined = contents.examined;
        page.cached_total = contents.tracks.len();
        let filter = normalize(&page.q);
        let items = contents
            .tracks
            .into_iter()
            .map(item)
            .filter(|item| matches(item, &filter))
            .collect::<Vec<_>>();
        page.total = items.len();
        page.items = items
            .into_iter()
            .skip(page.offset)
            .take(page.limit)
            .collect();
        let end = page.offset.saturating_add(page.items.len());
        page.next_offset = (end < page.total).then_some(end);
    }
    page.validate_for(query)?;
    Ok(page)
}

impl Surface {
    /// Read one cached playlist using the existing browse_library permission.
    /// No discovery, player calls, Spotify calls or action-log writes occur.
    pub fn playlist_contents(
        &self,
        client: &Client,
        query: &PlaylistQuery,
    ) -> Result<PlaylistPage, Failure> {
        self.guard(client).authorize("browse_library", true)?;
        query.validate()?;
        let log = self.log.as_ref().ok_or_else(|| {
            Failure::new(
                ErrorCode::NotReady,
                "playlist browsing needs the daemon's local store",
            )
            .with_hint("Connect to fsonos serve and refresh its library first.")
        })?;
        let (listed, contents) = {
            let store = log.store.lock().map_err(|_| {
                Failure::new(ErrorCode::Internal, "the library store is unavailable")
            })?;
            let error = |error: fsonos_core::store::StoreError| {
                Failure::new(ErrorCode::Internal, error.to_string())
            };
            (
                store
                    .playlists()
                    .map_err(error)?
                    .into_iter()
                    .find(|(uri, _)| *uri == query.playlist_uri),
                store
                    .playlist_contents(&query.playlist_uri)
                    .map_err(error)?,
            )
        };
        if listed.is_none() && contents.is_none() {
            return Err(
                Failure::new(ErrorCode::NoMatch, "the playlist is not in the local cache")
                    .with_hint(
                        "Choose a cached playlist from browse_library or refresh the library.",
                    ),
            );
        }
        let name = listed
            .map(|(_, name)| name)
            .filter(|name| !name.trim().is_empty())
            .unwrap_or_else(|| query.playlist_uri.clone());
        page(query, name, contents)
    }
}

#[cfg(test)]
mod tests;
