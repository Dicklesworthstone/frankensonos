//! The studio organizes the native remote without a second command surface.

use fastapi::core::RouteEntry;

use super::{Ctx, Op, REMOTE, asset};

const SCRIPT: &str = include_str!("../../remote/studio.js");
const INSIGHTS_SCRIPT: &str = include_str!("../../remote/studio-insights.js");
const INSIGHTS_STYLE: &str = include_str!("../../remote/studio-insights.css");
const DIAGNOSTICS_SCRIPT: &str = include_str!("../../remote/studio-diagnostics.js");
const ROOM_SETS_SCRIPT: &str = include_str!("../../remote/room-sets.js");
const ROOM_SETS_PANEL: &str = include_str!("../../remote/room-sets-panel.js");
const ROOM_SETS_STYLE: &str = include_str!("../../remote/room-sets.css");
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
            |_, _, _| {
                asset(
                    "text/javascript; charset=utf-8",
                    &format!(
                        "{DIAGNOSTICS_SCRIPT}\n{ROOM_SETS_SCRIPT}\n{ROOM_SETS_PANEL}\n{SCRIPT}\n{INSIGHTS_SCRIPT}"
                    ),
                )
            },
        )
        .response_schema::<String>(200, "The studio script (text/javascript)"),
        cx.route(
            &Op::get(
                "/studio.css",
                "web_studio_style",
                REMOTE,
                "Listening studio responsive style",
            ),
            |_, _, _| {
                asset(
                    "text/css; charset=utf-8",
                    &format!("{STYLE}\n{INSIGHTS_STYLE}\n{ROOM_SETS_STYLE}"),
                )
            },
        )
        .response_schema::<String>(200, "The studio style sheet (text/css)"),
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn studio_assets_keep_the_remote_privacy_boundary() {
        for source in [
            SCRIPT,
            STYLE,
            INSIGHTS_SCRIPT,
            INSIGHTS_STYLE,
            DIAGNOSTICS_SCRIPT,
            ROOM_SETS_SCRIPT,
            ROOM_SETS_PANEL,
            ROOM_SETS_STYLE,
        ] {
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
