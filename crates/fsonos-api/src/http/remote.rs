//! The web remote: `GET /` is a page (with `/remote.js` and `/remote.css`)
//! that controls the speakers from any browser that reaches the listener,
//! the loopback one or the tailnet one, which is also how it is reached
//! behind Tailscale Serve. It is a client of the public API like any other:
//! no token, no secret, nothing in the page that the API does not answer
//! anyway. `GET /art?zone=<room>` is its album art (see
//! [`crate::surface::art`]).
//!
//! The page is static: each asset is embedded at build time and sent with
//! a Content-Security-Policy that lets it load only itself and talk only to
//! its own origin, and never be framed. Its control calls are ordinary
//! same-origin JSON POSTs, which pass the listener's Origin check only from
//! the daemon's own origins ([`crate::web`]).

use fastapi::core::RouteEntry;
use fastapi::{JsonSchema, Request, Response, ResponseBody, StatusCode, fastapi_openapi};

use super::{Ctx, Op, query_param};
use crate::failure::{ErrorCode, Failure};

const REMOTE: &str = "remote";

const PAGE: &str = include_str!("../remote/index.html");
const SCRIPT: &str = include_str!("../remote/remote.js");
const STYLE: &str = include_str!("../remote/remote.css");

/// What the remote's documents may do: load only themselves, talk only to
/// their own origin, never be framed.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
                   connect-src 'self'; base-uri 'none'; form-action 'none'; \
                   frame-ancestors 'none'";

/// Every remote route.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/",
                "web_remote",
                REMOTE,
                "The web remote: a page to control the speakers from a browser",
            ),
            |_, _, _| asset("text/html; charset=utf-8", PAGE),
        )
        .response_schema::<String>(200, "The page (text/html)"),
        cx.route(
            &Op::get(
                "/remote.js",
                "web_remote_script",
                REMOTE,
                "The web remote's script",
            ),
            |_, _, _| asset("text/javascript; charset=utf-8", SCRIPT),
        )
        .response_schema::<String>(200, "The script (text/javascript)"),
        cx.route(
            &Op::get(
                "/remote.css",
                "web_remote_style",
                REMOTE,
                "The web remote's style",
            ),
            |_, _, _| asset("text/css; charset=utf-8", STYLE),
        )
        .response_schema::<String>(200, "The style sheet (text/css)"),
        cx.route(
            &Op::get(
                "/art",
                "album_art",
                REMOTE,
                "The album art of what a room's group plays, from its own player (204 when none)",
            ),
            |s, c, req| match same_origin(req)
                .and_then(|()| art_query(req))
                .and_then(|q| Ok((s.album_art(c, &q.zone)?, q.v.is_some())))
            {
                // A URL tagged for its track may be kept a while; an
                // untagged one is asked again each time.
                Ok((Some(art), tagged)) => hardened(Response::ok())
                    .header("content-type", art.content_type.into_bytes())
                    .header(
                        "cache-control",
                        if tagged {
                            "private, max-age=300"
                        } else {
                            "private, no-cache"
                        },
                    )
                    .body(ResponseBody::Bytes(art.bytes)),
                Ok((None, _)) => hardened(Response::with_status(StatusCode::NO_CONTENT))
                    .header("cache-control", "no-store"),
                Err(failure) => failure.http_response(),
            },
        )
        .query_schema::<ArtQuery>(false)
        .response_schema::<String>(200, "The image, as the player serves it (image/*)"),
    ]
}

/// The headers every remote response carries: the [`CSP`], no sniffing,
/// no referrer, and no use by other origins' pages.
fn hardened(response: Response) -> Response {
    response
        .header("content-security-policy", CSP)
        .header("x-content-type-options", "nosniff")
        .header("referrer-policy", "no-referrer")
        .header("cross-origin-resource-policy", "same-origin")
        .header("x-frame-options", "DENY")
}

/// One embedded asset; browsers check for a newer one on each load.
fn asset(content_type: &str, text: &'static str) -> Response {
    hardened(Response::ok())
        .header("content-type", content_type)
        .header("cache-control", "no-cache")
        .body(ResponseBody::Bytes(text.as_bytes().to_vec()))
}

/// `GET /art?zone=<room>&v=<tag>`.
#[derive(Debug, JsonSchema)]
struct ArtQuery {
    /// The room whose group's art to show.
    zone: String,
    /// A tag for the track (at most 64 letters, digits, `-` or `_`), so a
    /// page asks again when it changes; a tagged answer may be cached.
    v: Option<String>,
}

/// The art query, refusing any other parameter: the art is always what the
/// room's own player reports, never a URL a caller names.
fn art_query(req: &Request) -> Result<ArtQuery, Failure> {
    let query = req.query().unwrap_or_default();
    for pair in query.split('&').filter(|p| !p.is_empty()) {
        let name = pair.split_once('=').map_or(pair, |(n, _)| n);
        if name != "zone" && name != "v" {
            return Err(Failure::invalid(format!(
                "GET /art takes only zone (and v), not {name:?}"
            ))
            .with_hint(
                "Name the room: GET /art?zone=<room>. The art is always what that room's own \
                 player reports.",
            ));
        }
    }
    let v = query_param(req, "v")?;
    if let Some(v) = &v
        && (v.len() > 64
            || !v
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'))
    {
        return Err(Failure::invalid(
            "v must be at most 64 letters, digits, - or _",
        ));
    }
    let zone = query_param(req, "zone")?
        .ok_or_else(|| Failure::invalid("name the room: GET /art?zone=<room>"))?;
    Ok(ArtQuery { zone, v })
}

/// A browser loading the art into a page from another origin (an `<img>`
/// elsewhere) is refused: what plays is the house's business.
/// `Sec-Fetch-Site` is sent by browsers only; other clients are unaffected.
fn same_origin(req: &Request) -> Result<(), Failure> {
    let site = req
        .headers()
        .get("sec-fetch-site")
        .map(|v| String::from_utf8_lossy(v).trim().to_ascii_lowercase());
    match site.as_deref() {
        None | Some("same-origin" | "none") => Ok(()),
        Some(other) => Err(Failure::new(
            ErrorCode::UntrustedOrigin,
            format!("album art is only for the daemon's own pages (Sec-Fetch-Site: {other})"),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fastapi::Method;

    fn get(uri: &str, headers: &[(&str, &str)]) -> Request {
        let (path, query) = uri.split_once('?').unwrap_or((uri, ""));
        let mut req = Request::new(Method::Get, path);
        if !query.is_empty() {
            req.set_query(Some(query.to_string()));
        }
        for (name, value) in headers {
            req.headers_mut().insert(*name, value.as_bytes().to_vec());
        }
        req
    }

    #[test]
    fn art_takes_a_room_never_a_url() {
        let q = art_query(&get("/art?zone=Living+Room&v=abc-1", &[])).unwrap();
        assert_eq!(
            (q.zone.as_str(), q.v.as_deref()),
            ("Living Room", Some("abc-1"))
        );
        for refused in [
            "/art?zone=Kitchen&url=http%3A%2F%2F127.0.0.1%3A1400%2Fx",
            "/art?zone=Kitchen&uri=x",
            "/art?url=http://192.0.2.10:1400/getaa",
            "/art?zone=Kitchen&v=a/b",
            "/art?zone=Kitchen&v=http://evil.example",
            "/art",
        ] {
            let failure = art_query(&get(refused, &[])).unwrap_err();
            assert_eq!(failure.code, ErrorCode::InvalidArgument, "{refused}");
        }
    }

    #[test]
    fn other_sites_pages_get_no_art() {
        for ok in [None, Some("same-origin"), Some("none")] {
            let headers: Vec<(&str, &str)> =
                ok.map(|v| ("sec-fetch-site", v)).into_iter().collect();
            assert_eq!(same_origin(&get("/art", &headers)), Ok(()), "{ok:?}");
        }
        for site in ["cross-site", "same-site"] {
            let failure = same_origin(&get("/art", &[("sec-fetch-site", site)])).unwrap_err();
            assert_eq!(
                (failure.code, failure.status()),
                (ErrorCode::UntrustedOrigin, 403)
            );
        }
    }

    /// No credential, token or address is ever embedded in what the page
    /// sends a browser.
    #[test]
    fn the_page_carries_no_secrets() {
        for (name, text) in [
            ("index.html", PAGE),
            ("remote.js", SCRIPT),
            ("remote.css", STYLE),
        ] {
            let lower = text.to_ascii_lowercase();
            for secret in [
                "token",
                "authorization",
                "x-fsonos",
                "tailscale-user",
                "password",
                "cookie",
                "ts.net",
                "http://",
                "https://",
            ] {
                assert!(!lower.contains(secret), "{name} mentions {secret:?}");
            }
            assert!(
                !text
                    .split(|c: char| !(c.is_ascii_digit() || c == '.'))
                    .any(|w| w.split('.').count() == 4 && w.split('.').all(|o| !o.is_empty())),
                "{name} embeds an IPv4 address"
            );
        }
    }
}
