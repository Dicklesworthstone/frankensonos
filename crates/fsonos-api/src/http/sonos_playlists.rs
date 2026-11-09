//! Native saved Sonos playlists: browse and play with household-local metadata.

use fastapi::core::RouteEntry;
use fastapi::{JsonSchema, Request, fastapi_openapi};

use super::{Ctx, Op, answer, body, query_param, whole_number};
use crate::surface::sonos_playlists::{
    PlaySonosPlaylistRequest, SaveSonosPlaylistRequest, SavedSonosPlaylistDto,
    SonosPlaylistContentsDto, SonosPlaylistContentsRequest, SonosPlaylistDto,
    SonosPlaylistTrackDto, SonosPlaylistTrackRequest,
};
use crate::{Failure, OutcomeDto};

const TAG: &str = "sonos-playlists";

pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/sonos/playlists", "list_sonos_playlists", TAG,
                "The saved Sonos playlists of a room's household (list_favorites permission)",
            ),
            |s, c, req| answer(query(req).and_then(|q| s.sonos_playlists(c, &q.zone))),
        )
        .query_schema::<PlaylistQuery>(true)
        .response_schema::<Vec<SonosPlaylistDto>>(200, "The household's native saved playlists"),
        cx.route(
            &Op::get(
                "/sonos/playlists/contents", "sonos_playlist_contents", TAG,
                "Inspect ordered native saved playlist contents without changing the playing queue (list_favorites permission)",
            ),
            |s, c, req| answer(contents_query(req).and_then(|q| s.sonos_playlist_contents(c, &q))),
        )
        .query_schema::<ContentsQuery>(true)
        .response_schema::<SonosPlaylistContentsDto>(200, "A coherent native playlist page with its continuation revision"),
        cx.route(
            &Op::post(
                "/sonos/playlists/track", "sonos_playlist_track", TAG,
                "Play or queue one revision-bound native saved playlist occurrence (play_favorite or add_to_queue permission)",
            ),
            |s, c, req| answer(body::<SonosPlaylistTrackRequest>(req)
                .and_then(|r| s.sonos_playlist_track(c, &r))),
        )
        .request_schema::<SonosPlaylistTrackRequest>(true)
        .response_schema::<SonosPlaylistTrackDto>(200, "The selected native track's playback or confirmed queue insertion"),
        cx.route(
            &Op::post(
                "/sonos/playlists/play", "play_sonos_playlist", TAG,
                "Replace the queue with a saved Sonos playlist and play it (play_favorite permission)",
            ),
            |s, c, req| answer(body::<PlaySonosPlaylistRequest>(req)
                .and_then(|r| s.play_sonos_playlist(c, &r))),
        )
        .request_schema::<PlaySonosPlaylistRequest>(true)
        .response_schema::<OutcomeDto>(200, "The native playlist is playing"),
        cx.route(
            &Op::post(
                "/sonos/playlists/save", "save_sonos_playlist", TAG,
                "Save the current group queue as a new native playlist without changing playback (house-config permission)",
            ),
            |s, c, req| answer(body::<SaveSonosPlaylistRequest>(req)
                .and_then(|r| s.save_sonos_playlist(c, &r))),
        )
        .request_schema::<SaveSonosPlaylistRequest>(true)
        .response_schema::<SavedSonosPlaylistDto>(200, "A new playlist confirmed on the Sonos household"),
    ]
}

#[derive(JsonSchema)]
struct PlaylistQuery {
    /// Any room in the household; qualify as Room@S1 or Room@S2 if needed.
    zone: String,
}

fn query(req: &Request) -> Result<PlaylistQuery, Failure> {
    let zone = query_param(req, "zone")?
        .ok_or_else(|| Failure::invalid("name the room: GET /sonos/playlists?zone=<room>"))?;
    Ok(PlaylistQuery { zone })
}

#[derive(JsonSchema)]
struct ContentsQuery {
    zone: String,
    /// An SQ: id, catalog number or unambiguous title; continuations require the exact SQ: id.
    playlist: String,
    /// Number of entries to skip (default 0).
    offset: Option<u32>,
    /// Maximum entries, 1-100 (default 100).
    limit: Option<u32>,
    /// Previous page's opaque revision; required when offset is nonzero.
    expected_revision: Option<String>,
}

fn contents_query(req: &Request) -> Result<SonosPlaylistContentsRequest, Failure> {
    let raw = req.query().unwrap_or_default();
    if raw.len() > 32_768 {
        return Err(Failure::invalid(
            "the saved playlist contents query is too long",
        ));
    }
    let mut seen = std::collections::BTreeSet::new();
    for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
        let (name, _) = pair
            .split_once('=')
            .ok_or_else(|| Failure::invalid("playlist parameters must be name=value pairs"))?;
        if !["zone", "playlist", "offset", "limit", "expected_revision"].contains(&name)
            || !seen.insert(name)
        {
            return Err(Failure::invalid(
                "unknown or repeated saved playlist contents parameter",
            ));
        }
    }
    let number = |name: &str| -> Result<Option<u32>, Failure> {
        whole_number(req, name)?
            .map(|n| u32::try_from(n).map_err(|_| Failure::invalid(format!("{name} is too large"))))
            .transpose()
    };
    let query = ContentsQuery {
        zone: query_param(req, "zone")?
            .ok_or_else(|| Failure::invalid("name the room with zone"))?,
        playlist: query_param(req, "playlist")?
            .ok_or_else(|| Failure::invalid("select the saved playlist with playlist"))?,
        offset: number("offset")?,
        limit: number("limit")?,
        expected_revision: query_param(req, "expected_revision")?,
    };
    let request = SonosPlaylistContentsRequest {
        zone: query.zone,
        playlist: query.playlist,
        offset: query.offset.unwrap_or(0),
        limit: query.limit.unwrap_or(100),
        expected_revision: query.expected_revision,
    };
    request.validate()?;
    Ok(request)
}
