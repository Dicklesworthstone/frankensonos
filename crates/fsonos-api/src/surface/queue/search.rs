//! Bounded metadata search over one observed coordinator queue. The scan uses
//! ContentDirectory directly, without polling playback for every page. A final
//! topology and queue read rejects a changing group instead of combining it.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::control;
use fsonos_core::policy::Client;
use fsonos_proto::content::BrowsePage;
use fsonos_proto::didl::DidlObject;
use fsonos_types::text::normalize;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;
use std::time::{Duration, Instant};

use super::{MAX_PAGE, QueueItemDto, Surface, TOOL, snapshot};
use crate::failure::{ErrorCode, Failure};
use crate::request::zone_name;

/// Most ContentDirectory reads in one search, including its final recheck.
/// At most 99 data pages (9,900 entries) are examined before continuation.
pub const SEARCH_PAGE_READS: usize = 100;
/// Most matching occurrences retained by one search or continuation.
pub const MAX_SEARCH_RESULTS: u32 = 200;
// Checked between data-page reads; an in-flight read and final validation
// retain their transport deadlines. Slow sections return a continuation.
const SEARCH_SCAN_BUDGET: Duration = Duration::from_secs(15);
const MAX_QUERY_BYTES: usize = 1024;
const MAX_QUERY_TERMS: usize = 32;
const MAX_FIELD_BYTES: usize = 16 * 1024;

const fn default_limit() -> u32 {
    50
}

/// Search the target group's existing queue, starting after the offset entries.
/// A continuation must carry the revision returned by the preceding search.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueSearchRequest {
    pub zone: String,
    /// Every normalized term must occur in the title, creator or album.
    pub query: String,
    #[serde(default)]
    pub offset: u32,
    #[serde(default = "default_limit")]
    pub limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<String>,
}

/// One bounded section of a queue search. Repeated tracks remain separate
/// occurrences with their original positions and exact renderer identities.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueueSearchDto {
    pub zone: String,
    pub query: String,
    /// Total entries in the observed queue, not the number of search matches.
    pub total: u32,
    pub revision: String,
    /// Zero-based beginning of the section examined by this request.
    pub offset: u32,
    /// Number of entries examined in this section, including nonmatches.
    pub examined: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_offset: Option<u32>,
    /// Whether this section reached the end of the queue.
    pub complete: bool,
    pub items: Vec<QueueItemDto>,
}

impl QueueSearchRequest {
    /// Validate limits and continuation identity before any player reads.
    pub fn validate(&self) -> Result<(), Failure> {
        zone_name("zone", &self.zone)?;
        if self.query.len() > MAX_QUERY_BYTES || self.query.chars().any(char::is_control) {
            return Err(Failure::invalid(format!(
                "query must be at most {MAX_QUERY_BYTES} bytes without control characters"
            )));
        }
        let normalized = normalize(&self.query);
        let terms = normalized.split_whitespace().count();
        if terms == 0 || terms > MAX_QUERY_TERMS {
            return Err(Failure::invalid(format!(
                "query must contain 1 to {MAX_QUERY_TERMS} words of a title, artist or album"
            )));
        }
        if !(1..=MAX_SEARCH_RESULTS).contains(&self.limit) {
            return Err(Failure::invalid(format!(
                "limit must be 1 to {MAX_SEARCH_RESULTS}"
            )));
        }
        if self.offset == u32::MAX {
            return Err(Failure::invalid("invalid queue search offset"));
        }
        if self.expected_revision.as_deref().is_some_and(|revision| {
            revision.trim().is_empty()
                || revision.len() > 1024
                || revision.chars().any(char::is_control)
        }) {
            return Err(Failure::invalid(
                "expected_revision must be nonempty and at most 1024 bytes without control characters",
            ));
        }
        if self.offset > 0 && self.expected_revision.is_none() {
            return Err(Failure::invalid(
                "a queue search continuation requires expected_revision from the previous result",
            ));
        }
        Ok(())
    }

    /// Encode a read-only daemon request without letting names become URL syntax.
    pub fn path(&self) -> Result<String, Failure> {
        self.validate()?;
        let mut path = format!(
            "/zones/{}/queue/search?q={}&offset={}&limit={}",
            segment(self.zone.trim()),
            segment(self.query.trim()),
            self.offset,
            self.limit,
        );
        if let Some(revision) = &self.expected_revision {
            let _ = write!(path, "&expected_revision={}", segment(revision));
        }
        Ok(path)
    }
}

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

fn one_line(value: &str) -> String {
    value.chars().flat_map(char::escape_debug).collect()
}

impl QueueSearchDto {
    /// Occurrence identities and the exact search coverage, including how to
    /// continue a bounded section that has not reached the end of the queue.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = format!(
            "{}'s queue: {} match(es) for {:?}; {} entries total.\n",
            one_line(&self.zone),
            self.items.len(),
            self.query,
            self.total,
        );
        if self.examined > 0 {
            let _ = writeln!(
                text,
                "Examined positions {} through {} ({} entries).",
                self.offset.saturating_add(1),
                self.offset.saturating_add(self.examined),
                self.examined,
            );
        } else {
            let _ = writeln!(text, "No entries at offset {}.", self.offset);
        }
        for item in &self.items {
            let _ = writeln!(text, "{:>5}. {}", item.position, one_line(&item.title));
            for (label, value) in [("Artist", &item.creator), ("Album", &item.album)] {
                if let Some(value) = value {
                    let _ = writeln!(text, "       {label}: {}", one_line(value));
                }
            }
            let uri = item
                .uri
                .as_deref()
                .filter(|uri| !uri.trim().is_empty() && *uri != "NOT_IMPLEMENTED");
            if let Some(uri) = uri {
                let _ = writeln!(text, "       renderer_uri: {}", one_line(uri));
            } else {
                text.push_str(
                    "       No usable renderer identity; this occurrence cannot be selected.\n",
                );
            }
        }
        let _ = writeln!(text, "Revision: {}", one_line(&self.revision));
        if let Some(offset) = self.next_offset {
            let _ = writeln!(
                text,
                "More of the queue remains. Continue with offset={offset}, this expected_revision and the same query."
            );
        } else if self.complete {
            text.push_str("End of queue reached.\n");
        }
        text
    }
}

fn changed() -> Failure {
    Failure::new(
        ErrorCode::QueueChanged,
        "the queue or its group changed during the search",
    )
    .with_hint("Start a new search at offset 0 without expected_revision; do not combine the old and new results.")
}

fn bounded_metadata(page: &BrowsePage) -> Result<(), Failure> {
    for item in &page.objects {
        if [
            Some(item.title.as_str()),
            item.creator.as_deref(),
            item.album.as_deref(),
            item.res.as_ref().map(|res| res.uri.as_str()),
        ]
        .into_iter()
        .flatten()
        .any(|field| field.len() > MAX_FIELD_BYTES)
        {
            return Err(Failure::new(
                ErrorCode::UpnpFault,
                "a queue metadata field exceeds the 16384-byte search limit",
            ));
        }
    }
    Ok(())
}

fn matches(item: &DidlObject, terms: &[&str]) -> bool {
    let fields = [
        normalize(&item.title),
        normalize(item.creator.as_deref().unwrap_or_default()),
        normalize(item.album.as_deref().unwrap_or_default()),
    ];
    terms
        .iter()
        .all(|term| fields.iter().any(|field| field.contains(*term)))
}

impl Surface {
    /// Search existing queue metadata. Every read is authorized as get_queue;
    /// this never changes playback, queue contents, levels, or the action log.
    pub fn queue_search(
        &self,
        client: &Client,
        req: &QueueSearchRequest,
    ) -> Result<QueueSearchDto, Failure> {
        self.guard(client).authorize(TOOL, true)?;
        req.validate()?;
        self.search_queue_section(client, req, SEARCH_SCAN_BUDGET)
            .inspect_err(|failure| self.notice(failure))
    }

    fn search_queue_section(
        &self,
        client: &Client,
        req: &QueueSearchRequest,
        budget: Duration,
    ) -> Result<QueueSearchDto, Failure> {
        let started = Instant::now();
        let limit = usize::try_from(req.limit).unwrap_or(usize::MAX);
        let (households, group) = self.queue_households(client, &req.zone)?;
        let coordinator = control::locate(&households, &group.coordinator)?;
        let count = MAX_PAGE.min(u32::MAX - req.offset);
        let first = snapshot::page(&*self.transport, coordinator.ip, req.offset, count)?;
        bounded_metadata(&first)?;
        let revision = snapshot::revision(&coordinator.id, &first);
        if req
            .expected_revision
            .as_ref()
            .is_some_and(|expected| *expected != revision)
        {
            return Err(changed());
        }
        let normalized = normalize(&req.query);
        let terms: Vec<_> = normalized.split_whitespace().collect();
        let total = first.total_matches;
        let mut cursor = req.offset;
        let mut items = Vec::new();
        let mut current = first.clone();
        for page_index in 0..SEARCH_PAGE_READS - 1 {
            if snapshot::revision(&coordinator.id, &current) != revision {
                return Err(changed());
            }
            bounded_metadata(&current)?;
            for item in current.objects {
                cursor += 1;
                if matches(&item, &terms) {
                    items.push(QueueItemDto {
                        position: cursor,
                        title: item.title,
                        creator: item.creator,
                        album: item.album,
                        uri: item.res.map(|res| res.uri),
                    });
                    if items.len() == limit {
                        break;
                    }
                }
            }
            if cursor >= total
                || items.len() == limit
                || page_index + 1 == SEARCH_PAGE_READS - 1
                || started.elapsed() >= budget
            {
                break;
            }
            current = snapshot::page(
                &*self.transport,
                coordinator.ip,
                cursor,
                MAX_PAGE.min(total - cursor),
            )?;
        }
        // Recheck live topology, not the surface's cached household snapshot.
        // This also catches a target room leaving an unchanged coordinator queue.
        self.queue_group_unchanged(coordinator.ip, &group)
            .map_err(|failure| {
                if failure.code == ErrorCode::NotCoordinator {
                    changed()
                } else {
                    failure
                }
            })?;
        let after = snapshot::page(&*self.transport, coordinator.ip, req.offset, count)?;
        if after != first {
            return Err(changed());
        }
        Ok(QueueSearchDto {
            zone: coordinator.room_name.clone(),
            query: req.query.trim().to_string(),
            total,
            revision,
            offset: req.offset,
            examined: cursor - req.offset,
            next_offset: (cursor < total).then_some(cursor),
            complete: cursor >= total,
            items,
        })
    }
}

#[cfg(test)]
mod tests;
