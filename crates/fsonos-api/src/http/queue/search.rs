//! Bounded metadata search over one observed queue revision.

use fastapi::core::RouteEntry;
use fastapi::{JsonSchema, Request, fastapi_openapi};

use super::super::{Ctx, Op, ZONES, answer, path_room, query_param, whole_number};
use crate::failure::Failure;
use crate::surface::queue::{QueueSearchDto, QueueSearchRequest};

#[derive(JsonSchema)]
struct SearchQuery {
    /// Words from the queued title, artist or album; all words must match.
    q: String,
    /// Entries already examined (default 0). Continuations need expected_revision.
    offset: Option<u32>,
    /// Maximum matches in this section (default 50, at most 200).
    limit: Option<u32>,
    /// Opaque revision from the previous search; a changed queue is rejected.
    expected_revision: Option<String>,
}

fn request(req: &Request) -> Result<QueueSearchRequest, Failure> {
    let raw = req.query().unwrap_or_default();
    if raw.len() > 16_384 {
        return Err(Failure::invalid("the queue search query is too long"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
        let (name, _) = pair
            .split_once('=')
            .ok_or_else(|| Failure::invalid("search parameters must be name=value pairs"))?;
        if !["q", "offset", "limit", "expected_revision"].contains(&name) || !seen.insert(name) {
            return Err(Failure::invalid(
                "unknown or repeated queue search parameter",
            ));
        }
    }
    let number = |name: &str| -> Result<Option<u32>, Failure> {
        whole_number(req, name)?
            .map(|n| u32::try_from(n).map_err(|_| Failure::invalid(format!("{name} is too large"))))
            .transpose()
    };
    let query = SearchQuery {
        q: query_param(req, "q")?.ok_or_else(|| {
            Failure::invalid("say what to find: GET /zones/{room}/queue/search?q=<words>")
        })?,
        offset: number("offset")?,
        limit: number("limit")?,
        expected_revision: query_param(req, "expected_revision")?,
    };
    Ok(QueueSearchRequest {
        zone: path_room(req)?,
        query: query.q,
        offset: query.offset.unwrap_or(0),
        limit: query.limit.unwrap_or(50),
        expected_revision: query.expected_revision,
    })
}

pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/zones/{room}/queue/search",
                "search_queue",
                ZONES,
                "Find queued music by title, artist and album, retaining exact positions and revision (get_queue permission)",
            ),
            |s, c, req| answer(request(req).and_then(|query| s.queue_search(c, &query))),
        )
        .query_schema::<SearchQuery>(true)
        .response_schema::<QueueSearchDto>(200, "Matches and the exact queue section examined"),
    ]
}
