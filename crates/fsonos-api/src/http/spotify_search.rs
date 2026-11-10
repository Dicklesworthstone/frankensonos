//! Online Spotify metadata search, separate from the saved library.

use fastapi::core::RouteEntry;
use fastapi::{JsonSchema, Request, fastapi_openapi};

use super::{Ctx, Op, answer, query_param, whole_number};
use crate::source::CatalogKind;
use crate::surface::spotify_search::MAX_RESULTS;
use crate::{Failure, SpotifySearchDto, SpotifySearchQuery};

pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/spotify/search",
                "search_spotify",
                "spotify",
                "Search Spotify's online catalog without changing playback or the saved library",
            ),
            |surface, client, req| {
                answer(query(req).and_then(|q| {
                    surface.search_spotify(client, &q.query, q.kind, q.limit)
                }))
            },
        )
        .query_schema::<SearchQuery>(true)
        .response_schema::<SpotifySearchDto>(
            200,
            "A bounded page of canonical sources in Spotify's order; playing or queueing is a separate request",
        ),
    ]
}

#[derive(JsonSchema)]
struct SearchQuery {
    /// Search words, 1-256 UTF-8 bytes after trimming; no control characters.
    q: String,
    /// The one content kind to search: track, album, artist, playlist, show, episode.
    kind: CatalogKind,
    /// Maximum results, 1-10 (default 10).
    limit: Option<u32>,
}

fn query(req: &Request) -> Result<SpotifySearchQuery, Failure> {
    let raw = req.query().unwrap_or_default();
    if raw.len() > 4096 {
        return Err(Failure::invalid("the Spotify search query is too long"));
    }
    let mut seen = std::collections::HashSet::new();
    for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
        let (name, _) = pair
            .split_once('=')
            .ok_or_else(|| Failure::invalid("search parameters must be name=value pairs"))?;
        if !["q", "kind", "limit"].contains(&name) || !seen.insert(name) {
            return Err(Failure::invalid(
                "unknown or repeated Spotify search parameter",
            ));
        }
    }
    let query = SearchQuery {
        q: query_param(req, "q")?
            .ok_or_else(|| Failure::invalid("say what to search for with q"))?,
        kind: query_param(req, "kind")?
            .ok_or_else(|| Failure::invalid("choose a Spotify search kind"))?
            .parse()?,
        limit: whole_number(req, "limit")?
            .map(|limit| {
                u32::try_from(limit)
                    .map_err(|_| Failure::invalid("Spotify search limit is too large"))
            })
            .transpose()?,
    };
    let request = SpotifySearchQuery {
        query: query.q,
        kind: query.kind,
        limit: query.limit.unwrap_or(MAX_RESULTS),
    };
    request.validate()?;
    Ok(request)
}
