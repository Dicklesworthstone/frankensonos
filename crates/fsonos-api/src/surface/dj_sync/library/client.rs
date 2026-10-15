//! Typed client support shared by the CLI and MCP cached library browser.

use super::{BrowsePage, BrowseQuery, MAX_PAGE, source};
use crate::failure::{ErrorCode, Failure};
use std::collections::HashSet;
use std::fmt::Write as _;

impl BrowseQuery {
    /// Validate and encode this query for the daemon; filter text is never a URL.
    pub fn path(&self) -> Result<String, Failure> {
        browse_path(self)
    }
}

impl BrowsePage {
    /// Reject inconsistent or stale pages before exposing source selections.
    /// A revision is an observation, not a reservation or a playback credential.
    pub fn validate_for(
        &self,
        query: &BrowseQuery,
        expected_revision: Option<&str>,
    ) -> Result<(), Failure> {
        check_page(self, query, expected_revision)
    }

    /// Cached coverage, exact source identities, and continuation guidance.
    #[must_use]
    pub fn text(&self) -> String {
        page_text(self)
    }
}

/// Percent-encode UTF-8 query values, including delimiters and percent signs.
fn segment(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
            encoded.push(char::from(byte));
        } else {
            let _ = write!(encoded, "%{byte:02X}");
        }
    }
    encoded
}

fn browse_path(query: &BrowseQuery) -> Result<String, Failure> {
    query.validate()?;
    let mut path = format!(
        "/library/browse?kind={}&offset={}&limit={}",
        segment(query.kind.as_deref().unwrap_or("tracks")),
        query.offset.unwrap_or_default(),
        query.limit.unwrap_or(MAX_PAGE),
    );
    for (name, value) in [("q", &query.q), ("album_uri", &query.album_uri)] {
        if let Some(value) = value {
            let _ = write!(path, "&{name}={}", segment(value));
        }
    }
    Ok(path)
}

fn check_page(
    page: &BrowsePage,
    query: &BrowseQuery,
    expected_revision: Option<&str>,
) -> Result<(), Failure> {
    query.validate()?;
    let inconsistent = || {
        Failure::new(ErrorCode::Internal, "the cached library page is inconsistent")
            .with_hint("Read the first page again; no music was started or queued.")
    };
    let kind = query.kind.as_deref().unwrap_or("tracks");
    let item_kind = match kind {
        "tracks" => "track",
        "albums" => "album",
        "playlists" => "playlist",
        _ => return Err(inconsistent()),
    };
    let offset = query.offset.unwrap_or_default();
    let limit = query.limit.unwrap_or(MAX_PAGE);
    if limit == 0
        || limit > MAX_PAGE
        || page.kind != kind
        || page.coverage != "cached"
        || page.offset != offset
        || page.limit != limit
        || page.album_uri != query.album_uri
        || page.revision.is_empty()
        || page.revision.len() > 256
        || page.items.len() != page.total.saturating_sub(offset).min(limit)
    {
        return Err(inconsistent());
    }
    let end = offset
        .checked_add(page.items.len())
        .ok_or_else(inconsistent)?;
    if page.next_offset != (end < page.total).then_some(end) {
        return Err(inconsistent());
    }
    let mut seen = HashSet::new();
    for item in &page.items {
        if item.kind != item_kind
            || !seen.insert(&item.source_uri)
            || !source(&item.source_uri, item_kind)
            || query
                .album_uri
                .as_ref()
                .is_some_and(|uri| item.album_uri.as_ref() != Some(uri))
        {
            return Err(inconsistent());
        }
    }
    if expected_revision.is_some_and(|expected| expected != page.revision) {
        return Err(Failure::invalid("the cached library changed while paging").with_hint(
            "Start at offset 0 without expected_revision, keeping your filters. Do not combine the old and new pages.",
        ));
    }
    Ok(())
}

// Preserve exact metadata in structured content while keeping each text row on
// one line. In particular, cached metadata cannot emit terminal control codes.
fn one_line(value: &str) -> String {
    value.chars().flat_map(char::escape_debug).collect()
}

fn page_text(page: &BrowsePage) -> String {
    let mut text = format!(
        "Cached {}: {} shown, {} total; offset {}.\nCache contents may differ from your current Spotify saves.\n",
        page.kind,
        page.items.len(),
        page.total,
        page.offset,
    );
    for item in &page.items {
        let _ = writeln!(
            text,
            "{} — {}",
            one_line(&item.title),
            one_line(&item.subtitle)
        );
        let _ = writeln!(text, "  source_uri: {}", one_line(&item.source_uri));
        if let Some(count) = item.cached_tracks {
            let _ = writeln!(text, "  {count} cached track(s); the album may contain more.");
        }
        if item.kind == "track" {
            if let Some(album) = &item.album {
                let _ = writeln!(text, "  Album: {}", one_line(album));
            }
            if let (Some(disc), Some(track)) = (item.disc_number, item.track_number) {
                let _ = writeln!(text, "  Disc {disc}, track {track}");
            }
        }
        if item.explicit {
            text.push_str("  Explicit content\n");
        }
    }
    let _ = writeln!(text, "Revision: {}", one_line(&page.revision));
    if let Some(next) = page.next_offset {
        let _ = writeln!(text, "Next: offset={next}; pass this revision as expected_revision and keep the same filters.");
    } else {
        text.push_str("End of this cached listing.\n");
    }
    text.push_str("Play or queue an exact source_uri. Album tracks: browse tracks with that album_uri.");
    text
}

