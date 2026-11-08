//! Native saved Sonos playlists: browse and play with household-local metadata.

use fastapi::core::RouteEntry;
use fastapi::{JsonSchema, Request, fastapi_openapi};

use super::{Ctx, Op, answer, body, query_param};
use crate::surface::sonos_playlists::{
    PlaySonosPlaylistRequest, SaveSonosPlaylistRequest, SavedSonosPlaylistDto, SonosPlaylistDto,
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
