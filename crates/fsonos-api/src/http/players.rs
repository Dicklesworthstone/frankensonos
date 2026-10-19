//! Player listings and read-only smart-target resolution.

use fastapi::core::RouteEntry;
use fastapi::{JsonSchema, Request, fastapi_openapi};

use super::{Ctx, DAEMON, Op, answer, query_param};
use crate::failure::Failure;
use crate::surface::players::targets::ResolvedRoomsDto;
use crate::surface::players::{PlayerDto, TOOL};

#[derive(Debug, JsonSchema)]
struct TargetQuery {
    /// A room, player id, alias, here, or everywhere; optionally qualified.
    target: String,
}

fn target_query(request: &Request) -> Result<TargetQuery, Failure> {
    Ok(TargetQuery {
        target: query_param(request, "target")?.ok_or_else(|| {
            Failure::invalid("name the target: GET /rooms/resolve?target=<room or alias>")
        })?,
    })
}

/// Inventory reads; resolving a target never sends a playback command.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/players",
                TOOL,
                DAEMON,
                "Every player, by household and room",
            ),
            |surface, client, _| answer(surface.players(client)),
        )
        .response_schema::<Vec<PlayerDto>>(200, "The players"),
        cx.route(
            &Op::get(
                "/rooms/resolve",
                "resolve_rooms",
                DAEMON,
                "Inspect a smart target's addressable rooms and full transport groups; requires resolve_rooms, list_rooms and list_zones permission",
            ),
            |surface, client, request| {
                answer(
                    target_query(request)
                        .and_then(|query| surface.resolve_rooms(client, &query.target)),
                )
            },
        )
        .query_schema::<TargetQuery>(true)
        .response_schema::<ResolvedRoomsDto>(
            200,
            "Resolved room identities and affected groups; an observation, not a reservation",
        ),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastapi::Method;

    #[test]
    fn target_query_decodes_names_and_requires_an_explicit_target() {
        let mut request = Request::new(Method::Get, "/rooms/resolve");
        assert!(target_query(&request).is_err());
        request.set_query(Some("target=Ada%E2%80%99s+Studio%40S1".into()));
        assert_eq!(target_query(&request).unwrap().target, "Ada’s Studio@S1");
    }
}
