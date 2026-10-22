//! Browse cached music before selecting an exact source to play or queue.

use crate::worker;
use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::dj_sync::library::BrowseQuery;
use fsonos_api::surface::dj_sync::playlist::PlaylistQuery;

#[tool(
    description = "Browse cached tracks (default), albums, or playlists without contacting Spotify or speakers. kind is tracks, albums, or playlists. q filters title, artist and album words; album_uri with tracks opens a spotify:album URI in cached disc/track order. offset is zero-based; limit is 1-50 (default 50). To continue, pass next_offset and the previous revision as expected_revision, retaining the same filters. A changed revision requires a fresh first page, not combining listings. Album cached_tracks is not the complete catalog length; use playlist_contents with a playlist's exact source_uri to inspect its cached tracks in original order. Cache entries may no longer be saved on Spotify and explicit tracks remain visible. Use an item's exact source_uri with play or add_to_queue, never its display position. A new category or filter starts at offset zero without expected_revision. Library refresh is the separate dj_sync action.",
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
    worker::run(ctx, move |backend| {
        backend.browse_library(&query, expected_revision.as_deref())
    })
    .await
}

#[tool(
    description = "Read one cached Spotify playlist's tracks in original playlist order without contacting Spotify or speakers. playlist_uri is an exact spotify:playlist URI from browse_library. Repeated track URIs remain separate occurrences at their original one-based positions; these are playlist positions, not Sonos queue positions. coverage is complete, partial, or not_cached: only observed playable tracks are returned, and an uncached playlist is not an empty playlist. q filters title, artist and album words. offset is zero-based over matching cached occurrences; limit is 1-50 (default 50). Continue with next_offset and the same playlist_uri, q, and revision as expected_revision; expected_revision is required after offset zero. A changed revision requires a fresh first page. Use an item's exact source_uri with play or add_to_queue. Explicit tracks remain visible. Uses browse_library policy permission; dj_sync refreshes the cache separately.",
    annotations(read_only, idempotent)
)]
async fn playlist_contents(
    ctx: &McpContext,
    playlist_uri: String,
    offset: Option<u32>,
    limit: Option<u32>,
    q: Option<String>,
    expected_revision: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    ctx.checkpoint()?;
    let query = PlaylistQuery {
        playlist_uri,
        offset: offset.map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
        limit: limit.map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
        q,
        expected_revision,
    };
    worker::run(ctx, move |backend| backend.playlist_contents(&query)).await
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
        .tool(PlaylistContents)
        .try_build()
        .expect("cached browsing has a valid tool schema");
    }
}
