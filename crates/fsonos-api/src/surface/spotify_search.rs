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
/// Largest explicit Spotify search offset.
pub const MAX_OFFSET: u32 = 1000;
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

    /// Request a later page with the exact same query, kind and limit.
    pub fn path_at(&self, offset: u32) -> Result<String, Failure> {
        validate_offset(offset)?;
        let path = self.path()?;
        Ok(if offset == 0 {
            path
        } else {
            format!("{path}&offset={offset}")
        })
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

/// Search results with an explicit continuation. Existing first-page fields
/// retain their wire shape, so older consumers can still read these results.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SpotifySearchPageDto {
    pub query: String,
    pub kind: CatalogKind,
    pub items: Vec<CatalogItem>,
    pub more: bool,
    /// Raw provider position, before unavailable rows or duplicates are removed.
    #[serde(default)]
    pub offset: u32,
    /// Continue this query/kind/limit at this exact offset; never derive it
    /// from the number of visible items. None also covers the provider bound.
    #[serde(default)]
    pub next_offset: Option<u32>,
}

impl SpotifySearchPageDto {
    #[must_use]
    pub fn into_results(self) -> SpotifySearchDto {
        SpotifySearchDto {
            query: self.query,
            kind: self.kind,
            items: self.items,
            more: self.more,
        }
    }

    /// A later request must not accept a stale first-page daemon response.
    pub fn validate_for(&self, request: &SpotifySearchQuery, offset: u32) -> Result<(), Failure> {
        validate_offset(offset)?;
        self.clone().into_results().validate_for(request)?;
        if self.offset != offset
            || self.next_offset.is_some_and(|next| {
                !self.more
                    || next <= offset
                    || next > MAX_OFFSET
                    || next > offset + request.limit
                    || usize::try_from(next - offset)
                        .is_ok_and(|received| received < self.items.len())
            })
        {
            return Err(Failure::new(
                ErrorCode::NotReady,
                "Spotify returned an inconsistent search continuation",
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn text(&self) -> String {
        let mut result = self.clone().into_results();
        result.more = false;
        let mut text = result.text();
        let _ = write!(text, "\nSearch offset: {}.", self.offset);
        if let Some(next) = self.next_offset {
            let _ = write!(
                text,
                "\nMore results: repeat the same query, kind and limit with --offset {next} (MCP/HTTP offset={next})."
            );
        } else if self.more {
            text.push_str("\nNo further continuation is available; refine the query.");
        }
        text
    }
}

pub fn validate_offset(offset: u32) -> Result<(), Failure> {
    if offset > MAX_OFFSET {
        Err(Failure::invalid("Spotify search offset must be 0 to 1000"))
    } else {
        Ok(())
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
        self.search_spotify_page(
            client,
            &SpotifySearchQuery {
                query: query.to_owned(),
                kind,
                limit,
            },
            0,
        )
        .map(SpotifySearchPageDto::into_results)
    }

    /// Read exactly one explicit page using the selected session and policy.
    pub fn search_spotify_page(
        &self,
        client: &Client,
        request: &SpotifySearchQuery,
        offset: u32,
    ) -> Result<SpotifySearchPageDto, Failure> {
        self.guard(client).authorize("search_spotify", true)?;
        request.validate()?;
        validate_offset(offset)?;
        let sources = self.spotify_sources.as_ref().ok_or_else(|| {
            Failure::new(
                ErrorCode::SpotifyAuthRequired,
                "catalog search needs the owner's Spotify session",
            )
            .with_hint("Sign in with fsonos setup, then search Spotify again.")
        })?;
        let found = sources.search_catalog_page(
            request.query.trim(),
            request.kind,
            request.limit,
            offset,
        )?;
        self.guard(client).authorize("search_spotify", true)?;
        let response = SpotifySearchPageDto {
            query: request.query.trim().to_owned(),
            kind: request.kind,
            items: found.page.items,
            more: found.page.more,
            offset: found.offset,
            next_offset: found.next_offset,
        };
        response.validate_for(request, offset)?;
        Ok(response)
    }
}
