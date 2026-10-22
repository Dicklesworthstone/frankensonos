//! Typed query construction and response checking for remote consumers.

use std::fmt::Write as _;

use super::{MAX_PAGE, PlaylistPage, PlaylistQuery, changed, matches, source};
use crate::failure::{ErrorCode, Failure};
use fsonos_types::text::normalize;

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

impl PlaylistQuery {
    /// Validate and encode values without treating filter text as URL syntax.
    pub fn path(&self) -> Result<String, Failure> {
        self.validate()?;
        let mut path = format!(
            "/library/playlist?playlist_uri={}&offset={}&limit={}",
            segment(&self.playlist_uri),
            self.offset.unwrap_or_default(),
            self.limit.unwrap_or(MAX_PAGE),
        );
        for (name, value) in [
            ("q", &self.q),
            ("expected_revision", &self.expected_revision),
        ] {
            if let Some(value) = value {
                let _ = write!(path, "&{name}={}", segment(value));
            }
        }
        Ok(path)
    }
}

fn inconsistent() -> Failure {
    Failure::new(
        ErrorCode::Internal,
        "the cached playlist page is inconsistent",
    )
    .with_hint("Read the first page again; no music was started or queued.")
}

impl PlaylistPage {
    /// Verify identity, filter, coverage, occurrence positions and continuation
    /// before exposing a remote response's source selections.
    pub fn validate_for(&self, query: &PlaylistQuery) -> Result<(), Failure> {
        query.validate()?;
        let offset = query.offset.unwrap_or_default();
        let limit = query.limit.unwrap_or(MAX_PAGE);
        let filter = normalize(query.q.as_deref().unwrap_or(""));
        let cached_total = u32::try_from(self.cached_total).map_err(|_| inconsistent())?;
        if self.playlist_uri != query.playlist_uri
            || self.q != query.q.as_deref().unwrap_or("").trim()
            || self.offset != offset
            || self.limit != limit
            || self.total > self.cached_total
            || cached_total > self.examined
            || (filter.is_empty() && self.total != self.cached_total)
            || self.items.len() != self.total.saturating_sub(offset).min(limit)
        {
            return Err(inconsistent());
        }
        let end = offset
            .checked_add(self.items.len())
            .ok_or_else(inconsistent)?;
        if self.next_offset != (end < self.total).then_some(end) {
            return Err(inconsistent());
        }
        self.validate_coverage()?;
        let mut previous = 0;
        for (index, item) in self.items.iter().enumerate() {
            let position = usize::try_from(item.position).map_err(|_| inconsistent())?;
            if item.position <= previous
                || item.position > self.examined
                || position <= offset + index
                || !source(&item.source_uri, "track")
                || item
                    .album_uri
                    .as_deref()
                    .is_some_and(|uri| !source(uri, "album"))
                || !matches(item, &filter)
                || std::iter::once(item.title.as_str())
                    .chain(std::iter::once(item.subtitle.as_str()))
                    .chain(item.album.as_deref())
                    .any(|field| field.len() > 16 * 1024)
            {
                return Err(inconsistent());
            }
            previous = item.position;
        }
        if query
            .expected_revision
            .as_ref()
            .is_some_and(|expected| Some(expected) != self.revision.as_ref())
        {
            return Err(changed());
        }
        Ok(())
    }

    fn validate_coverage(&self) -> Result<(), Failure> {
        match self.coverage.as_str() {
            "not_cached" => {
                if self.fetched_at.is_some()
                    || self.source_total.is_some()
                    || self.examined != 0
                    || self.cached_total != 0
                    || self.total != 0
                    || self.revision.is_some()
                    || !self.items.is_empty()
                    || self.next_offset.is_some()
                {
                    return Err(inconsistent());
                }
            }
            "complete" | "partial" => {
                if self.fetched_at.is_none()
                    || self.revision.as_deref().is_none_or(|revision| {
                        revision.trim().is_empty()
                            || revision.len() > 256
                            || revision.chars().any(char::is_control)
                    })
                    || self.source_total.is_some_and(|total| {
                        total < self.examined
                            || (self.coverage == "complete" && total != self.examined)
                    })
                {
                    return Err(inconsistent());
                }
            }
            _ => return Err(inconsistent()),
        }
        Ok(())
    }

    /// Plain text retains exact track identities and continuation information.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = format!(
            "Cached playlist: {}\nplaylist_uri: {}\nCoverage: {}. {} shown, {} match, {} cached; offset {}.\n",
            one_line(&self.name),
            one_line(&self.playlist_uri),
            one_line(&self.coverage),
            self.items.len(),
            self.total,
            self.cached_total,
            self.offset,
        );
        if self.coverage == "not_cached" {
            text.push_str("No track contents have been cached for this playlist. Refresh the library to try reading them.\n");
            return text;
        }
        let total = self
            .source_total
            .map_or_else(|| "unknown".into(), |total| total.to_string());
        let _ = writeln!(
            text,
            "Source entries examined: {} of {total}; unavailable entries may leave position gaps.",
            self.examined
        );
        if let Some(at) = self.fetched_at {
            let when = chrono::DateTime::from_timestamp(at, 0).map_or_else(
                || at.to_string(),
                |date| date.format("%Y-%m-%d %H:%M UTC").to_string(),
            );
            let _ = writeln!(
                text,
                "Observed: {when}. Spotify may have changed since this read."
            );
        }
        for item in &self.items {
            let _ = writeln!(
                text,
                "#{} {} — {}",
                item.position,
                one_line(&item.title),
                one_line(&item.subtitle)
            );
            let _ = writeln!(text, "  source_uri: {}", one_line(&item.source_uri));
            if let Some(album) = &item.album {
                let _ = writeln!(text, "  Album: {}", one_line(album));
            }
            if item.explicit {
                text.push_str("  Explicit content\n");
            }
        }
        if let Some(revision) = &self.revision {
            let _ = writeln!(text, "Revision: {}", one_line(revision));
        }
        if let Some(next) = self.next_offset {
            let _ = writeln!(
                text,
                "Next: offset={next}; pass this revision as expected_revision and keep the same playlist and filter."
            );
        } else if self.coverage == "partial" {
            text.push_str("End of the cached portion; more source entries were not read.\n");
        } else {
            text.push_str("End of the cached observation. Complete means pagination ended, not a live Spotify snapshot.\n");
        }
        text.push_str("Play or queue an exact source_uri; these positions belong to the playlist, not a Sonos queue.");
        text
    }
}

fn one_line(value: &str) -> String {
    value.chars().flat_map(char::escape_debug).collect()
}
