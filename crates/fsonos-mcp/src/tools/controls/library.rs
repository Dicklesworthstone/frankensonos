//! One cached listing authority for agent browsing, search and playback.

use crate::tools::{Backend, respond, segment};
use fastmcp::FinalCallToolResult;
use fastmcp::prelude::McpResult;
use fsonos_api::surface::dj_sync::library::{BrowsePage, BrowseQuery, MAX_PAGE};
use fsonos_api::{ErrorCode, Failure};
use std::collections::HashSet;
use std::fmt::Write as _;

impl Backend {
    /// Use the daemon's cache when present, never silently switch caches after
    /// an error. Standalone mode uses the same Surface projection and policy.
    pub fn browse_library(
        &self,
        query: &BrowseQuery,
        expected_revision: Option<&str>,
    ) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let path = browse_path(query);
            let page = self.read(&path, || self.surface.browse_library(&self.client, query))?;
            check_page(&page, query, expected_revision)?;
            Ok((page_text(&page), page))
        })
    }
}

fn browse_path(query: &BrowseQuery) -> String {
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
    path
}

fn check_page(
    page: &BrowsePage,
    query: &BrowseQuery,
    expected_revision: Option<&str>,
) -> Result<(), Failure> {
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
    let prefix = format!("spotify:{item_kind}:");
    let mut seen = HashSet::new();
    for item in &page.items {
        if item.kind != item_kind
            || !seen.insert(&item.source_uri)
            || !item.source_uri.strip_prefix(&prefix).is_some_and(|id| {
                !id.is_empty() && id.len() <= 128 && id.bytes().all(|b| b.is_ascii_alphanumeric())
            })
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
        let _ = writeln!(text, "  source_uri: {}", item.source_uri);
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

#[cfg(test)]
mod tests;
