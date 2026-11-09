//! `POST /announce`: speak, chime or upload a WAV, then put the music back (see
//! [`crate::surface::announce`]).

use fastapi::core::RouteEntry;
use fastapi::{JsonSchema, PathParams, Request, fastapi_openapi};
use serde::Deserialize;

use super::{CONTROL, Ctx, Op, answer, body, body_or_default, percent_decode};
use crate::Failure;
use crate::surface::announce::{AnnounceDto, AnnounceRequest, AnnouncementJobDto, TOOL};

#[derive(Default, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct CancelAnnouncementRequest {}

fn id(req: &Request) -> Result<String, Failure> {
    let raw = req
        .get_extension::<PathParams>()
        .and_then(|params| params.get("id"))
        .unwrap_or_default();
    percent_decode(raw)
        .ok_or_else(|| Failure::invalid("announcement id is not valid percent-encoded UTF-8"))
}

/// The announcement route.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route_shared(
            &Op::post(
                "/announcements",
                "announce_start",
                CONTROL,
                "Start a bounded announcement job; inspect or cancel it by its returned id",
            ),
            |surface, client, req| {
                answer(
                    body::<AnnounceRequest>(req)
                        .and_then(|request| surface.start_announcement(client, &request)),
                )
            },
        )
        .request_schema::<AnnounceRequest>(true)
        .response_schema::<AnnouncementJobDto>(
            200,
            "Accepted job; playback and restoration continue in the daemon",
        ),
        cx.route(
            &Op::get(
                "/announcements",
                "list_announcements",
                CONTROL,
                "Recent and active announcements visible to this caller; the owner may inspect all",
            ),
            |surface, client, _| answer(surface.list_announcements(client)),
        )
        .response_schema::<Vec<AnnouncementJobDto>>(
            200,
            "At most 64 retained jobs, newest first; process-local ids",
        ),
        cx.route(
            &Op::get(
                "/announcements/{id}",
                "get_announcement",
                CONTROL,
                "Read announcement progress, cancellation state and the final restoration report",
            ),
            |surface, client, req| {
                answer(id(req).and_then(|id| surface.get_announcement(client, &id)))
            },
        )
        .response_schema::<AnnouncementJobDto>(
            200,
            "Current job observation; no playback commands",
        ),
        cx.route(
            &Op::post(
                "/announcements/{id}/cancel",
                "cancel_announcement",
                CONTROL,
                "Cancel a waiting or active announcement; ownership-checked restoration continues",
            ),
            |surface, client, req| {
                answer(
                    body_or_default::<CancelAnnouncementRequest>(
                        req,
                        CancelAnnouncementRequest::default(),
                    )
                    .and_then(|_| id(req))
                    .and_then(|id| surface.cancel_announcement(client, &id)),
                )
            },
        )
        .request_schema::<CancelAnnouncementRequest>(false)
        .response_schema::<AnnouncementJobDto>(
            200,
            "Cancellation requested, or an already terminal receipt; never replays Stop",
        ),
        cx.route(
            &Op::post(
                "/announce",
                TOOL,
                CONTROL,
                "Speak, chime or play an uploaded WAV in rooms, then put the music back",
            ),
            |surface, client, req| {
                answer(body::<AnnounceRequest>(req).and_then(|r| surface.announce(client, &r)))
            },
        )
        .request_schema::<AnnounceRequest>(true)
        .response_schema::<AnnounceDto>(200, "What each household heard, and what was put back"),
    ]
}
