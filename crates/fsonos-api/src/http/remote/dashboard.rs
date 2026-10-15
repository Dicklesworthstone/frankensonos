//! The Listening Room dashboard is a separate, embedded client of the same API.
//! It inherits the remote's browser security headers and listener guards.

use fastapi::core::RouteEntry;
use fastapi::Response;

use super::{Ctx, Op, REMOTE, asset};

const PAGE: &str = include_str!("../../remote/dashboard.html");
const STYLE: &str = include_str!("../../remote/dashboard.css");
const CORE: &str = include_str!("../../remote/dashboard-core.js");
const VISUALIZER: &str = include_str!("../../remote/dashboard-visualizer.js");
const AUDIO: &str = include_str!("../../remote/dashboard-audio.js");
const SCRIPT: &str = include_str!("../../remote/dashboard.js");

fn document(content_type: &str, text: &str) -> Response {
    asset(content_type, text).header(
        "permissions-policy",
        "camera=(), geolocation=(), microphone=(self)",
    )
}

pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/dashboard",
                "web_dashboard",
                REMOTE,
                "Listening Room: live household dashboard and local visualization",
            ),
            |_, _, _| document("text/html; charset=utf-8", PAGE),
        )
        .response_schema::<String>(200, "The listening dashboard (text/html)"),
        cx.route(
            &Op::get(
                "/dashboard.css",
                "web_dashboard_style",
                REMOTE,
                "The listening dashboard's style sheet",
            ),
            |_, _, _| document("text/css; charset=utf-8", STYLE),
        )
        .response_schema::<String>(200, "The dashboard style sheet (text/css)"),
        cx.route(
            &Op::get(
                "/dashboard.js",
                "web_dashboard_script",
                REMOTE,
                "The listening dashboard's self-contained script",
            ),
            |_, _, _| {
                document(
                    "text/javascript; charset=utf-8",
                    &format!("{CORE}\n{VISUALIZER}\n{AUDIO}\n{SCRIPT}"),
                )
            },
        )
        .response_schema::<String>(200, "The dashboard script (text/javascript)"),
    ]
}
