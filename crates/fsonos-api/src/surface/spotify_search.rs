//! Explicit online catalog search, independent of the saved library and LAN.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fmt::Write as _;

use super::Surface;
use crate::source::{CatalogItem, CatalogKind, normalize_source_uri};
use crate::{ErrorCode, Failure};

/// Spotify's bounded search page size, shared by every caller.
pub const MAX_RESULTS: u32 = 10;
/// Maximum UTF-8 bytes of query text after trimming outer spaces.
pub const MAX_QUERY_BYTES: usize = 256;
const MAX_LABEL_BYTES: usize = 4096;

/// The same validation and URL encoding for local, CLI and MCP requests.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotifySearchQuery {
    pub query: String,
    pub kind: CatalogKind,
    pub limit: u32,
}

impl SpotifySearchQuery {
    pub fn validate(&self) -> Result<(), Failure> {
        if self.query.chars().any(char::is_control)
            || self.query.trim().is_empty()
            || self.query.trim().len() > MAX_QUERY_BYTES
        {
            return Err(Failure::invalid(
                "Spotify search needs 1 to 256 UTF-8 bytes of text without control characters",
            ));
        }
        if !(1..=MAX_RESULTS).contains(&self.limit) {
            return Err(Failure::invalid("Spotify search limit must be 1 to 10"));
        }
        Ok(())
    }

    /// Query text is encoded as one value; it cannot introduce URL parameters.
    pub fn path(&self) -> Result<String, Failure> {
        self.validate()?;
        let mut encoded = String::new();
        for byte in self.query.trim().bytes() {
            if byte.is_ascii_alphanumeric() || b"-._~".contains(&byte) {
                encoded.push(char::from(byte));
            } else {
                let _ = write!(encoded, "%{byte:02X}");
            }
        }
        Ok(format!(
            "/spotify/search?q={encoded}&kind={}&limit={}",
            self.kind.as_str(),
            self.limit
        ))
    }
}

/// Search metadata only. Playing or queueing a result is a separate request
/// through the usual source review, house policy and renderer resolution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SpotifySearchDto {
    pub query: String,
    pub kind: CatalogKind,
    pub items: Vec<CatalogItem>,
    pub more: bool,
}

impl SpotifySearchDto {
    /// Check both provider and forwarded daemon replies before offering sources.
    pub fn validate_for(&self, request: &SpotifySearchQuery) -> Result<(), Failure> {
        request.validate()?;
        let invalid = || {
            Failure::new(
                ErrorCode::NotReady,
                "Spotify returned an invalid catalog search result",
            )
        };
        if self.query != request.query.trim()
            || self.kind != request.kind
            || !u32::try_from(self.items.len()).is_ok_and(|count| count <= request.limit)
        {
            return Err(invalid());
        }
        let prefix = format!("spotify:{}:", request.kind.as_str());
        let mut seen = HashSet::new();
        for item in &self.items {
            if !item.source_uri.starts_with(&prefix)
                || normalize_source_uri(&item.source_uri).as_ref() != Ok(&item.source_uri)
                || !seen.insert(&item.source_uri)
                || item.title.trim().is_empty()
                || !valid_label(&item.title)
                || item
                    .subtitle
                    .as_deref()
                    .is_some_and(|text| !valid_label(text))
            {
                return Err(invalid());
            }
        }
        Ok(())
    }

    /// Sources remain explicit so an agent can ask to play or queue its choice.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = if self.items.is_empty() {
            format!(
                "No Spotify {} results for {:?}.",
                self.kind.as_str(),
                self.query
            )
        } else {
            format!(
                "Spotify {} results for {:?}:",
                self.kind.as_str(),
                self.query
            )
        };
        for (index, item) in self.items.iter().enumerate() {
            let _ = write!(text, "\n{}. {}", index + 1, item.title);
            if let Some(subtitle) = item.subtitle.as_deref().filter(|text| !text.is_empty()) {
                let _ = write!(text, " ({subtitle})");
            }
            let _ = write!(text, " — {}", item.source_uri);
        }
        if self.more {
            text.push_str("\nMore results exist; refine the query to narrow the selection.");
        }
        text
    }
}

fn valid_label(text: &str) -> bool {
    text.len() <= MAX_LABEL_BYTES && !text.chars().any(char::is_control)
}

impl Surface {
    /// Query Spotify with the owner's session without discovery, cache reads,
    /// queue writes, playback changes or DJ taste filtering.
    pub fn search_spotify(
        &self,
        client: &Client,
        query: &str,
        kind: CatalogKind,
        limit: u32,
    ) -> Result<SpotifySearchDto, Failure> {
        self.guard(client).authorize("search_spotify", true)?;
        let request = SpotifySearchQuery {
            query: query.to_owned(),
            kind,
            limit,
        };
        request.validate()?;
        let sources = self.spotify_sources.as_ref().ok_or_else(|| {
            Failure::new(
                ErrorCode::SpotifyAuthRequired,
                "catalog search needs the owner's Spotify session",
            )
            .with_hint("Sign in with fsonos setup, then search Spotify again.")
        })?;
        let page = sources.search_catalog(query.trim(), kind, limit)?;
        self.guard(client).authorize("search_spotify", true)?;
        let response = SpotifySearchDto {
            query: query.trim().to_owned(),
            kind,
            items: page.items,
            more: page.more,
        };
        response.validate_for(&request)?;
        Ok(response)
    }
}
