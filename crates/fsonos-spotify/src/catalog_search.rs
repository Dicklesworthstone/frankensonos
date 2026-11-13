//! Bounded read-only Spotify catalog search, separate from saved-library reads.
//!
//! Search returns canonical sources for a later explicit playback choice. It
//! reads one page, preserves provider order, fetches no images and never writes
//! library rows, preferences, account state or speaker state.

use std::collections::{HashMap, HashSet};

use asupersync::Cx;
use serde::Deserialize;

use crate::SpotifyError;
use crate::client::decode;
use crate::session::Session;

/// Spotify's current per-kind search page maximum.
pub const MAX_RESULTS: u32 = 10;
/// Maximum trimmed UTF-8 query length, in bytes.
pub const MAX_QUERY_BYTES: usize = 256;
/// Display titles are accepted only within this UTF-8 byte bound.
pub const MAX_TITLE_BYTES: usize = 4096;
/// Creator bylines are shortened at a UTF-8 boundary to fit this byte bound.
pub const MAX_SUBTITLE_BYTES: usize = 512;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogItem {
    pub source_uri: String,
    pub title: String,
    pub subtitle: Option<String>,
}

/// A first page only. `more` describes the provider's remaining results, even
/// when some entries on this page were unavailable or duplicate and omitted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CatalogPage {
    pub items: Vec<CatalogItem>,
    pub more: bool,
}

#[derive(Debug, Deserialize)]
struct Page {
    items: Vec<Option<Item>>,
    offset: u32,
    limit: u32,
    total: u32,
    #[serde(default)]
    next: Option<String>,
}

// Identity is optional on the wire so an unavailable/local placeholder can be
// skipped. Every usable row must pass the canonical identity check below.
#[derive(Debug, Deserialize)]
struct Item {
    id: Option<String>,
    uri: Option<String>,
    #[serde(rename = "type")]
    kind: Option<String>,
    name: Option<String>,
    is_playable: Option<bool>,
    #[serde(default)]
    is_local: bool,
    restrictions: Option<serde_json::Map<String, serde_json::Value>>,
    album: Option<AlbumAvailability>,
    artists: Option<Vec<Creator>>,
    owner: Option<Owner>,
    publisher: Option<String>,
    show: Option<Creator>,
}

#[derive(Debug, Deserialize)]
struct AlbumAvailability {
    restrictions: Option<serde_json::Map<String, serde_json::Value>>,
}

#[derive(Debug, Deserialize)]
struct Creator {
    name: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Owner {
    display_name: Option<String>,
    id: Option<String>,
}

/// Search exactly one of `track`, `album`, `artist`, `playlist`, `show` or
/// `episode`. Queries are trimmed, nonempty, free of control characters and at
/// most [`MAX_QUERY_BYTES`] bytes; `limit` must be in `1..=10`.
///
/// The owner's existing user token supplies the account country. Reads use
/// [`Session::get_brief`]'s refresh, retry and cooldown policy. Returned paging
/// URLs are never followed, and no additional OAuth scopes are requested.
pub async fn search(
    session: &mut Session,
    cx: &Cx,
    query: &str,
    kind: &str,
    limit: u32,
) -> Result<CatalogPage, SpotifyError> {
    let query = validate_query(query, kind, limit)?;
    let url = session.endpoints().search(query, kind, limit);
    let body = session.get_brief(cx, &url).await?;
    let mut response: HashMap<String, Page> = decode(&body, "catalog search")?;
    let page = response.remove(&format!("{kind}s")).ok_or_else(|| {
        SpotifyError::Decode("catalog search omitted the requested result kind".into())
    })?;
    if page.offset != 0
        || page.limit != limit
        || page.items.len() > limit as usize
        || page.items.len() > page.total as usize
    {
        return Err(SpotifyError::Decode(
            "catalog search returned inconsistent page counters".into(),
        ));
    }
    let more = page.next.is_some() || page.total as usize > page.items.len();
    let mut seen = HashSet::new();
    let mut items = Vec::new();
    for item in page.items.iter().flatten() {
        if let Some(item) = catalog_item(item, kind)?
            && seen.insert(item.source_uri.clone())
        {
            items.push(item);
        }
    }
    Ok(CatalogPage { items, more })
}

fn validate_query<'a>(query: &'a str, kind: &str, limit: u32) -> Result<&'a str, SpotifyError> {
    if query.chars().any(char::is_control)
        || query.trim().is_empty()
        || query.trim().len() > MAX_QUERY_BYTES
    {
        return Err(SpotifyError::Config(
            "catalog search needs a nonempty query of at most 256 UTF-8 bytes without control characters"
                .into(),
        ));
    }
    if !matches!(
        kind,
        "track" | "album" | "artist" | "playlist" | "show" | "episode"
    ) {
        return Err(SpotifyError::Config(
            "catalog search needs one kind: track, album, artist, playlist, show or episode".into(),
        ));
    }
    if !(1..=MAX_RESULTS).contains(&limit) {
        return Err(SpotifyError::Config(
            "catalog search limit must be between 1 and 10".into(),
        ));
    }
    Ok(query.trim())
}

fn catalog_item(item: &Item, kind: &str) -> Result<Option<CatalogItem>, SpotifyError> {
    if item.is_local
        || item.is_playable == Some(false)
        || item.restrictions.is_some()
        || (kind == "track"
            && item
                .album
                .as_ref()
                .is_some_and(|album| album.restrictions.is_some()))
    {
        return Ok(None);
    }
    // Spotify documents an empty album name as a catalog takedown.
    if kind == "album" && item.name.as_ref().is_some_and(String::is_empty) {
        return Ok(None);
    }
    let id = item.id.as_deref().unwrap_or_default();
    let uri = item.uri.as_deref().unwrap_or_default();
    if id.len() != 22
        || !id.bytes().all(|byte| byte.is_ascii_alphanumeric())
        || item.kind.as_deref() != Some(kind)
        || uri != format!("spotify:{kind}:{id}")
        || (kind == "episode" && item.is_playable != Some(true))
    {
        return Err(SpotifyError::Decode(
            "catalog search returned a malformed or inconsistent playable item".into(),
        ));
    }
    let title = item.name.as_deref().unwrap_or_default();
    check_display(title)?;
    let title = title.trim();
    if title.is_empty() {
        return Err(SpotifyError::Decode(
            "catalog search returned a playable item without a title".into(),
        ));
    }
    Ok(Some(CatalogItem {
        source_uri: uri.into(),
        title: title.into(),
        subtitle: subtitle(item, kind)?,
    }))
}

fn check_display(value: &str) -> Result<(), SpotifyError> {
    if value.len() > MAX_TITLE_BYTES || value.chars().any(char::is_control) {
        Err(SpotifyError::Decode(
            "catalog search returned invalid display metadata".into(),
        ))
    } else {
        Ok(())
    }
}

fn subtitle(item: &Item, kind: &str) -> Result<Option<String>, SpotifyError> {
    let mut names = Vec::new();
    match kind {
        "track" | "album" => {
            if let Some(artists) = &item.artists {
                for artist in artists.iter().take(3) {
                    if let Some(name) = artist.name.as_deref() {
                        names.push(name);
                    }
                }
            }
        }
        "playlist" => {
            if let Some(owner) = &item.owner
                && let Some(name) = owner
                    .display_name
                    .as_deref()
                    .filter(|name| !name.trim().is_empty())
                    .or(owner.id.as_deref())
            {
                names.push(name);
            }
        }
        "show" => names.extend(item.publisher.as_deref()),
        "episode" => names.extend(item.show.as_ref().and_then(|show| show.name.as_deref())),
        _ => {}
    }
    for name in &names {
        check_display(name)?;
    }
    let mut text = names
        .into_iter()
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .collect::<Vec<_>>()
        .join(", ");
    if text.is_empty() {
        return Ok(None);
    }
    if text.len() > MAX_SUBTITLE_BYTES {
        let mut end = MAX_SUBTITLE_BYTES - '…'.len_utf8();
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    Ok(Some(text))
}

#[cfg(test)]
mod tests;
