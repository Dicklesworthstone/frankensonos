//! The scene routes: `GET /scenes`, `GET|PUT|DELETE /scenes/{name}` and
//! `POST /scenes/{name}/apply` (see [`crate::surface::scenes`]).
//!
//! `PUT` saves the house as it is now when it has no body, or the scene a
//! JSON body writes out (any other body is UNSUPPORTED_MEDIA_TYPE); `DELETE`
//! takes none. Browsers send an Origin with both, which the listener checks,
//! and never send either cross-origin without a preflight.

use fastapi::core::RouteEntry;
use fastapi::{Method, PathParams, Request};

use super::{Ctx, Op, answer, body_or_default, percent_decode};
use crate::failure::Failure;
use crate::surface::scenes::{
    SceneApplyDto, SceneApplyRequest, SceneDefinition, SceneDeletedDto, SceneDto,
};

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
                "Save a scene (replacing one of that name): the house as it is now, or the one the body writes out",
            ),
            |s, c, req| {
                // No body (or `{}`): the house as it is now.
                let saved = body_or_default::<Option<SceneDefinition>>(req, None).and_then(|def| {
                    let name = path_name(req)?;
                    match def.filter(|d| *d != SceneDefinition::default()) {
                        Some(def) => s.save_scene_from(c, &name, &def),
                        None => s.save_scene(c, &name),
                    }
                });
                answer(saved)
            },
        )
        .request_schema::<SceneDefinition>(false)
        .response_schema::<SceneDto>(200, "The scene as saved"),
        cx.route(
            &Op::post(
                "/scenes/{name}/apply",
                "apply_scene",
                SCENES,
                "Put the house in a saved scene: only the steps it needs, undoable",
            ),
            |s, c, req| {
                // An empty body or `{}` applies at once.
                let applied = body_or_default(req, SceneApplyRequest::default()).and_then(|r| {
                    let name = path_name(req)?;
                    s.apply_scene_with(c, &name, &r)
                });
                answer(applied)
            },
        )
        .request_schema::<SceneApplyRequest>(false)
        .response_schema::<SceneApplyDto>(200, "What was done"),
        cx.route(
            &Op::delete("/scenes/{name}", "delete_scene", SCENES, "Forget a scene"),
            |s, c, req| answer(path_name(req).and_then(|name| s.delete_scene(c, &name))),
        )
        .response_schema::<SceneDeletedDto>(200, "The scene is gone"),
    ]
}
