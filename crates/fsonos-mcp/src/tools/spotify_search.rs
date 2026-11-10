//! Online Spotify catalog metadata; source selection stays an explicit action.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::source::CatalogKind;
use fsonos_api::surface::spotify_search::MAX_RESULTS;
use fsonos_api::{SpotifySearchDto, SpotifySearchQuery};

use super::{Backend, respond, with_backend_off_thread};

impl Backend {
    /// Use the selected daemon's Spotify session and validate its result; an
    /// error never falls through to another session or the saved-library cache.
    pub fn search_spotify(&self, request: &SpotifySearchQuery) -> McpResult<FinalCallToolResult> {
        respond(|| {
            self.authorize_reads(&["search_spotify"])?;
            let path = request.path()?;
            let page: SpotifySearchDto = self.read(&path, || {
                self.surface.search_spotify(
                    &self.client,
                    &request.query,
                    request.kind,
                    request.limit,
                )
            })?;
            page.validate_for(request)?;
            Ok((page.text(), page))
        })
    }
}

#[tool(
    description = "Search Spotify's online catalog using the owner's signed-in account, independently of the saved-library cache. query is 1-256 UTF-8 bytes of text without control characters. kind must be track, album, artist, playlist, show, or episode; limit is 1-10 (default 10). Results preserve Spotify's order and contain canonical source_uri, title and optional subtitle. more means more matches exist; refine the query to narrow them. This only reads metadata and does not discover speakers, change the saved library, filter by DJ taste, or start playback. Review the results, then explicitly select a source_uri with play or add_to_queue; artist and show sources resolve to bounded selections through the normal guarded playback path. Requires search_spotify / read permission.",
    annotations(read_only, idempotent)
)]
async fn search_spotify(
    ctx: &McpContext,
    query: String,
    kind: String,
    limit: Option<u32>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let kind: CatalogKind = kind
        .parse()
        .map_err(|failure| crate::tool_error(&failure))?;
    let request = SpotifySearchQuery {
        query,
        kind,
        limit: limit.unwrap_or(MAX_RESULTS),
    };
    with_backend_off_thread(ctx, move |backend| backend.search_spotify(&request)).await
}

#[cfg(test)]
mod tests;
