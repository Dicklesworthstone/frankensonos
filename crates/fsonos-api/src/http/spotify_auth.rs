//! The Spotify sign-in routes (see [`crate::surface::spotify_auth`]).
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `GET /auth/spotify` | | [`SignInDto`] |
//! | `POST /auth/spotify/begin` | `{}` | [`SignInStartDto`]: the consent page to open |
//! | `GET /auth/spotify/callback?code=&state=` | | a short HTML page for the browser Spotify sends back |
//! | `POST /auth/spotify/complete` | [`SignInCompleteRequest`] | [`SignInDto`] |

use fastapi::core::RouteEntry;
use fastapi::{Response, ResponseBody, StatusCode};

use super::{Ctx, DAEMON, Op, answer, body};
use crate::failure::Failure;
use crate::surface::spotify_auth::{SignInCompleteRequest, SignInDto, SignInStartDto};

/// Every sign-in route.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/auth/spotify",
                "spotify_sign_in_status",
                DAEMON,
                "Whether the daemon is signed in to the owner's Spotify",
            ),
            |s, c, _| answer(s.spotify_sign_in_status(c)),
        )
        .response_schema::<SignInDto>(200, "The sign-in"),
        cx.route(
            &Op::post(
                "/auth/spotify/begin",
                "spotify_sign_in_begin",
                DAEMON,
                "Start the Spotify sign-in: the consent page to open",
            ),
            |s, c, _| answer(s.spotify_sign_in_begin(c)),
        )
        .response_schema::<SignInStartDto>(200, "The page to open"),
        cx.route(
            &Op::post(
                "/auth/spotify/complete",
                "spotify_sign_in_complete",
                DAEMON,
                "Finish the sign-in with the address the browser landed on",
            ),
            |s, c, req| {
                answer(
                    body::<SignInCompleteRequest>(req)
                        .and_then(|b| s.spotify_sign_in_complete(c, &b.redirect)),
                )
            },
        )
        .request_schema::<SignInCompleteRequest>(true)
        .response_schema::<SignInDto>(200, "Signed in"),
        cx.route(
            &Op::get(
                "/auth/spotify/callback",
                "spotify_sign_in_callback",
                DAEMON,
                "Where Spotify sends the browser back (the registered redirect URI)",
            ),
            |s, c, req| {
                let query = req.query().unwrap_or_default();
                page(&s.spotify_sign_in_complete(c, &format!("?{query}")))
            },
        ),
    ]
}

/// The page the browser lands on: signed in, or why not. It carries no
/// script, and no referrer leaves it (the address holds the one-time code).
fn page(result: &Result<SignInDto, Failure>) -> Response {
    let (status, message) = match result {
        Ok(_) => (
            StatusCode::OK,
            "Signed in to Spotify. You can close this tab and go back to the terminal.".to_owned(),
        ),
        Err(f) => (
            StatusCode::from_u16(f.status()),
            format!("The Spotify sign-in did not finish: {}", f.detail),
        ),
    };
    let body = format!(
        "<!doctype html><meta charset=\"utf-8\"><title>FrankenSonos</title><p>{}</p>",
        escape(&message)
    );
    Response::with_status(status)
        .header("content-type", b"text/html; charset=utf-8".to_vec())
        .header("cache-control", b"no-store".to_vec())
        .header("content-security-policy", b"default-src 'none'".to_vec())
        .header("x-content-type-options", b"nosniff".to_vec())
        .header("referrer-policy", b"no-referrer".to_vec())
        .body(ResponseBody::Bytes(body.into_bytes()))
}

fn escape(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_page_escapes_what_it_shows() {
        assert_eq!(
            escape("<a href=\"x\">&</a>"),
            "&lt;a href=&quot;x&quot;&gt;&amp;&lt;/a&gt;"
        );
    }
}
