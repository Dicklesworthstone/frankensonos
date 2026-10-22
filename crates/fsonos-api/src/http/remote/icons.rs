//! The remote's icon, the studio's mark: for the browser tab (`/icon.svg`)
//! and a phone's home screen (`/apple-touch-icon.png`, 180 px square, since
//! iOS takes no SVG there).

use fastapi::core::RouteEntry;
use fastapi::{Response, ResponseBody};

use super::{Ctx, Op, REMOTE, hardened};

const SVG: &str = include_str!("../../remote/icon.svg");
const PNG: &[u8] = include_bytes!("../../remote/apple-touch-icon.png");

pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get("/icon.svg", "web_icon", REMOTE, "The remote's icon"),
            |_, _, _| image("image/svg+xml", SVG.as_bytes()),
        )
        .response_schema::<String>(200, "The icon (image/svg+xml)"),
        cx.route(
            &Op::get(
                "/apple-touch-icon.png",
                "web_touch_icon",
                REMOTE,
                "The remote's icon for a phone's home screen",
            ),
            |_, _, _| image("image/png", PNG),
        )
        .response_schema::<String>(200, "The icon, 180 px square (image/png)"),
    ]
}

/// An icon, with the headers every remote response carries; browsers check
/// for a newer one on each load, as for the other assets.
fn image(content_type: &str, bytes: &[u8]) -> Response {
    hardened(Response::ok())
        .header("content-type", content_type)
        .header("cache-control", "no-cache")
        .body(ResponseBody::Bytes(bytes.to_vec()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_icons_are_the_mark_and_nothing_else() {
        assert!(SVG.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\""));
        // Nothing that runs or loads: no script, link, event handler or image.
        let lower = SVG.to_ascii_lowercase();
        for forbidden in ["<script", "href", " on", "<image", "<foreignobject"] {
            assert!(!lower.contains(forbidden), "icon.svg carries {forbidden}");
        }
        assert!(PNG.starts_with(b"\x89PNG\r\n\x1a\n"));
        // IHDR: 180 x 180.
        assert_eq!(&PNG[16..24], &[0, 0, 0, 180, 0, 0, 0, 180]);
    }
}
