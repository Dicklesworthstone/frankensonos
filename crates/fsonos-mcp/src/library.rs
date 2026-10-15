//! Browse cached music before selecting an exact source to play or queue.

use crate::worker;
use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::dj_sync::library::BrowseQuery;

#[tool(
    description = "Browse cached tracks (default), albums, or playlists without contacting Spotify or speakers. kind is tracks, albums, or playlists. q filters title, artist and album words; album_uri with tracks opens a spotify:album URI in cached disc/track order. offset is zero-based; limit is 1-50 (default 50). To continue, pass next_offset and the previous revision as expected_revision, retaining the same filters. A changed revision requires a fresh first page, not combining listings. Album cached_tracks is not the complete catalog length; playlist membership is not available here. Cache entries may no longer be saved on Spotify and explicit tracks remain visible. Use an item's exact source_uri with play or add_to_queue, never its display position. A new category or filter starts at offset zero without expected_revision. Library refresh is the separate dj_sync action.",
    annotations(read_only, idempotent)
)]
async fn browse_library(
    ctx: &McpContext,
    kind: Option<String>,
    q: Option<String>,
    album_uri: Option<String>,
    offset: Option<u32>,
    limit: Option<u32>,
    expected_revision: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    ctx.checkpoint()?;
    let query = BrowseQuery {
        kind,
        q,
        album_uri,
        offset: offset.map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
        limit: limit.map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
    };
    worker::run(move |backend| backend.browse_library(&query, expected_revision.as_deref())).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cached_browse_registers_on_the_dual_era_server() {
        fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
            "library-browse-test",
            "1.0",
            fastmcp::ProtocolPolicy::Auto,
        )
        .unwrap()
        .tool(BrowseLibrary)
        .try_build()
        .expect("cached browsing has a valid tool schema");
    }
}
