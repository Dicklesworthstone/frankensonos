//! The scene routes: `GET /scenes`, `GET|PUT|DELETE /scenes/{name}` and
//! `POST /scenes/{name}/apply` (see [`crate::surface::scenes`]).
//!
//! `PUT` saves the house as it is now, so it takes no body; neither does
//! `DELETE`. Browsers send an Origin with both, which the listener checks,
//! and never send either cross-origin without a preflight.

use fastapi::core::RouteEntry;
use fastapi::{Method, PathParams, Request};
use serde_json::{Map, Value};

use super::{Ctx, Op, answer, body_or_default, percent_decode};
use crate::failure::Failure;
use crate::surface::scenes::{SceneApplyDto, SceneDeletedDto, SceneDto};

const SCENES: &str = "scenes";

impl Op {
    const fn put(
        path: &'static str,
        id: &'static str,
        tag: &'static str,
        summary: &'static str,
    ) -> Self {
        Self {
            method: Method::Put,
            ..Self::get(path, id, tag, summary)
        }
    }

    const fn delete(
        path: &'static str,
        id: &'static str,
        tag: &'static str,
        summary: &'static str,
    ) -> Self {
        Self {
            method: Method::Delete,
            ..Self::get(path, id, tag, summary)
        }
    }
}

/// The `{name}` in the path, percent-decoded.
fn path_name(req: &Request) -> Result<String, Failure> {
    let raw = req
        .get_extension::<PathParams>()
        .and_then(|p| p.get("name"))
        .unwrap_or_default();
    percent_decode(raw).ok_or_else(|| {
        Failure::invalid(format!("scene {raw:?} is not valid percent-encoded UTF-8"))
    })
}

/// Every scene route.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get("/scenes", "list_scenes", SCENES, "Every saved scene"),
            |s, c, _| answer(s.scenes(c)),
        )
        .response_schema::<Vec<SceneDto>>(200, "The scenes, by name"),
        cx.route(
            &Op::get(
                "/scenes/{name}",
                "get_scene",
                SCENES,
                "A saved scene: its groups, what each plays, and each room's volume",
            ),
            |s, c, req| answer(path_name(req).and_then(|name| s.scene(c, &name))),
        )
        .response_schema::<SceneDto>(200, "The scene"),
        cx.route(
            &Op::put(
                "/scenes/{name}",
                "save_scene",
                SCENES,
                "Save the house as it is now as a scene (replacing one of that name)",
            ),
            |s, c, req| answer(path_name(req).and_then(|name| s.save_scene(c, &name))),
        )
        .response_schema::<SceneDto>(200, "The scene as saved"),
        cx.route(
            &Op::post(
                "/scenes/{name}/apply",
                "apply_scene",
                SCENES,
                "Put the house in a saved scene: only the steps it needs, undoable",
            ),
            |s, c, req| {
                // No fields yet: an empty body or `{}`.
                let applied = body_or_default::<Map<String, Value>>(req, Map::new())
                    .and_then(|_| path_name(req))
                    .and_then(|name| s.apply_scene(c, &name));
                answer(applied)
            },
        )
        .response_schema::<SceneApplyDto>(200, "What was done"),
        cx.route(
            &Op::delete("/scenes/{name}", "delete_scene", SCENES, "Forget a scene"),
            |s, c, req| answer(path_name(req).and_then(|name| s.delete_scene(c, &name))),
        )
        .response_schema::<SceneDeletedDto>(200, "The scene is gone"),
    ]
}
