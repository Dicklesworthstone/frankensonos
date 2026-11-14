//! Reviewed household favorites authoring, separate from playback endpoints.

use fastapi::core::RouteEntry;
use fastapi::{JsonSchema, Request, fastapi_openapi};

use super::{Ctx, Op, answer, body, query_param};
use crate::Failure;
use crate::surface::favorite_management::{
    InspectFavoriteRequest, ManagedFavoriteDto, ObservedFavoriteDto, RemoveFavoriteRequest,
    RenameFavoriteRequest, SaveFavoriteRequest,
};

pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get("/favorites/current", "current_favorite", "favorites", "Preview the current native track or station for saving (list_favorites and get_zone_state permissions)"),
            |s, c, req| answer(current_query(req).and_then(|zone| s.current_favorite(c, &zone))),
        )
        .query_schema::<CurrentQuery>(true)
        .response_schema::<ObservedFavoriteDto>(200, "The actual source and its observed revision; no speaker writes"),
        cx.route(
            &Op::get("/favorites/inspect", "inspect_favorite", "favorites", "Read an exact native favorite and its revision before a catalog change"),
            |s, c, req| answer(inspect_query(req).and_then(|request| s.inspect_favorite(c, &request))),
        )
        .query_schema::<InspectFavoriteRequest>(true)
        .response_schema::<ObservedFavoriteDto>(200, "An exact household-local favorite with its revision"),
        cx.route(
            &Op::post("/favorites/save", "save_favorite", "favorites", "Save the reviewed current native track or station to My Sonos (house-config permission)"),
            |s, c, req| answer(body::<SaveFavoriteRequest>(req).and_then(|request| s.save_favorite(c, &request))),
        )
        .request_schema::<SaveFavoriteRequest>(true)
        .response_schema::<ManagedFavoriteDto>(200, "The created favorite confirmed in its household"),
        cx.route(
            &Op::post("/favorites/rename", "rename_favorite", "favorites", "Rename one reviewed My Sonos favorite (house-config permission)"),
            |s, c, req| answer(body::<RenameFavoriteRequest>(req).and_then(|request| s.rename_favorite(c, &request))),
        )
        .request_schema::<RenameFavoriteRequest>(true)
        .response_schema::<ManagedFavoriteDto>(200, "The same native favorite confirmed with its new name"),
        cx.route(
            &Op::post("/favorites/remove", "remove_favorite", "favorites", "Remove one reviewed My Sonos favorite without changing playback (house-config permission)"),
            |s, c, req| answer(body::<RemoveFavoriteRequest>(req).and_then(|request| s.remove_favorite(c, &request))),
        )
        .request_schema::<RemoveFavoriteRequest>(true)
        .response_schema::<ManagedFavoriteDto>(200, "The exact favorite confirmed absent from its household"),
    ]
}

#[derive(JsonSchema)]
struct CurrentQuery {
    zone: String,
}

fn validate_query(req: &Request, allowed: &[&str]) -> Result<(), Failure> {
    let raw = req.query().unwrap_or_default();
    if raw.len() > 32_768 {
        return Err(Failure::invalid("the favorites query is too long"));
    }
    let mut seen = std::collections::BTreeSet::new();
    for pair in raw.split('&').filter(|pair| !pair.is_empty()) {
        let (name, _) = pair
            .split_once('=')
            .ok_or_else(|| Failure::invalid("favorite parameters must be name=value pairs"))?;
        if !allowed.contains(&name) || !seen.insert(name) {
            return Err(Failure::invalid("unknown or repeated favorite parameter"));
        }
    }
    Ok(())
}

fn current_query(req: &Request) -> Result<String, Failure> {
    validate_query(req, &["zone"])?;
    let query = CurrentQuery {
        zone: query_param(req, "zone")?.ok_or_else(|| Failure::invalid("name a room with zone"))?,
    };
    Ok(query.zone)
}

fn inspect_query(req: &Request) -> Result<InspectFavoriteRequest, Failure> {
    validate_query(req, &["zone", "favorite"])?;
    let request = InspectFavoriteRequest {
        zone: query_param(req, "zone")?.ok_or_else(|| Failure::invalid("name a room with zone"))?,
        favorite: query_param(req, "favorite")?
            .ok_or_else(|| Failure::invalid("name an exact favorite with its FV:2/<n> id"))?,
    };
    request.validate()?;
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastapi::Method;

    #[test]
    fn favorites_query_refuses_ambiguous_or_ignored_parameters() {
        for path in [
            "/favorites/current?zone=Den&zone=Kitchen",
            "/favorites/current?zone=Den&favorite=FV%3A2%2F1",
            "/favorites/current?zone=Den&force=true",
        ] {
            let (path, query) = path.split_once('?').unwrap();
            let mut request = Request::new(Method::Get, path);
            request.set_query(Some(query.into()));
            assert!(current_query(&request).is_err());
        }
        let mut request = Request::new(Method::Get, "/favorites/inspect");
        request.set_query(Some("zone=Den%40S2&favorite=FV%3A2%2F7".into()));
        let parsed = inspect_query(&request).unwrap();
        assert_eq!(parsed.zone, "Den@S2");
        assert_eq!(parsed.favorite, "FV:2/7");
    }
}
