//! The library refresh and cached browsing routes (see [`crate::surface::dj_sync`]).
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `GET /dj/sync` | | [`LibrarySyncDto`] |
//! | `POST /dj/sync` | `{}` | [`LibrarySyncDto`]: the refresh started (or already running) |
//! | `GET /library/browse` | | [`BrowsePage`]: cached tracks, albums, or playlists |

use fastapi::core::RouteEntry;
use fastapi::Request;
use serde_json::{Map, Value};

use super::{Ctx, DJ, Op, answer, body_or_default, query_param, whole_number};
use crate::failure::Failure;
use crate::surface::dj_sync::LibrarySyncDto;
use crate::surface::dj_sync::library::{BrowsePage, BrowseQuery};

pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/dj/sync",
                "dj_sync_status",
                DJ,
                "Where the DJ's library refresh from Spotify stands",
            ),
            |s, c, _| answer(s.dj_sync_status(c)),
        )
        .response_schema::<LibrarySyncDto>(200, "Running or not, and the last refresh"),
        cx.route(
            &Op::post(
                "/dj/sync",
                "dj_sync",
                DJ,
                "Refresh the DJ's library from Spotify now, in the background",
            ),
            |s, c, req| {
                answer(body_or_default(req, Map::<String, Value>::new()).and_then(|_| s.dj_sync(c)))
            },
        )
        .response_schema::<LibrarySyncDto>(200, "The refresh started (or already running)"),
        cx.route(
            &Op::get(
                "/library/browse",
                "browse_library",
                "library",
                "Browse cached tracks, albums, or playlists without a Spotify or speaker request",
            ),
            |s, c, req| answer(browse_query(req).and_then(|q| s.browse_library(c, &q))),
        )
        .query_schema::<BrowseQuery>(false)
        .response_schema::<BrowsePage>(200, "A bounded page of cached music with a change detector"),
    ]
}

fn browse_query(req: &Request) -> Result<BrowseQuery, Failure> {
    let raw = req.query().unwrap_or_default();
    if raw.len() > 16_384 {
        return Err(Failure::invalid("the library browse query is too long"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
        let (name, _) = pair.split_once('=').ok_or_else(|| {
            Failure::invalid("browse parameters must be name=value pairs")
        })?;
        if !["kind", "q", "album_uri", "offset", "limit"].contains(&name)
            || !seen.insert(name)
        {
            return Err(Failure::invalid("unknown or repeated library browse parameter"));
        }
    }
    let query = BrowseQuery {
        kind: query_param(req, "kind")?,
        offset: whole_number(req, "offset")?,
        limit: whole_number(req, "limit")?,
        q: query_param(req, "q")?,
        album_uri: query_param(req, "album_uri")?,
    };
    query.validate()?;
    Ok(query)
}
