//! The studio organizes the native remote without a second command surface.

use fastapi::core::RouteEntry;

use super::{Ctx, Op, REMOTE, asset};

const SCRIPT: &str = include_str!("../../remote/studio.js");
const STYLE: &str = include_str!("../../remote/studio.css");

pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/studio.js",
                "web_studio_script",
                REMOTE,
                "Listening studio navigation and native control presentation",
            ),
            |_, _, _| asset("text/javascript; charset=utf-8", SCRIPT),
        )
        .response_schema::<String>(200, "The studio script (text/javascript)"),
        cx.route(
            &Op::get(
                "/studio.css",
                "web_studio_style",
                REMOTE,
                "Listening studio responsive style",
            ),
            |_, _, _| asset("text/css; charset=utf-8", STYLE),
        )
        .response_schema::<String>(200, "The studio style sheet (text/css)"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn studio_assets_keep_the_remote_privacy_boundary() {
        for source in [SCRIPT, STYLE] {
            let lower = source.to_ascii_lowercase();
            for forbidden in [
                "authorization",
                "password",
                "cookie",
                "http://",
                "https://",
                "innerhtml",
                "eval(",
            ] {
                assert!(!lower.contains(forbidden), "studio includes {forbidden}");
            }
        }
    }
}
