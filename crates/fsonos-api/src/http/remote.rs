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

mod dashboard;
mod studio;
mod undo;

const REMOTE: &str = "remote";

const PAGE: &str = include_str!("../remote/index.html");
const QUEUE_SCRIPT: &str = include_str!("../remote/queue.js");
const PLAYBACK_SCRIPT: &str = include_str!("../remote/playback.js");
const PLAYBACK_STYLE: &str = include_str!("../remote/playback.css");
const SCRIPT: &str = include_str!("../remote/remote.js");
const LIBRARY_SCRIPT: &str = include_str!("../remote/library.js");
const LIBRARY_STYLE: &str = include_str!("../remote/library.css");
const GROUPS_SCRIPT: &str = include_str!("../remote/groups.js");
const GROUPS_STYLE: &str = include_str!("../remote/groups.css");
const TIMERS_SCRIPT: &str = include_str!("../remote/timers.js");
const TIMERS_STYLE: &str = include_str!("../remote/timers.css");
const SCHEDULES_SCRIPT: &str = include_str!("../remote/schedules.js");
const SCHEDULES_STYLE: &str = include_str!("../remote/schedules.css");
const SCENES_SCRIPT: &str = include_str!("../remote/scenes.js");
const SCENES_STYLE: &str = include_str!("../remote/scenes.css");
const MODE_SCRIPT: &str = include_str!("../remote/play-mode.js");
const MODE_STYLE: &str = include_str!("../remote/play-mode.css");
const ANNOUNCEMENTS_SCRIPT: &str = include_str!("../remote/announcements.js");
const ANNOUNCEMENTS_STYLE: &str = include_str!("../remote/announcements.css");
const PREFERENCES_SCRIPT: &str = include_str!("../remote/dj-preferences.js");
const PREFERENCES_STYLE: &str = include_str!("../remote/dj-preferences.css");
const SESSION_SCRIPT: &str = include_str!("../remote/dj-session.js");
const SESSION_STYLE: &str = include_str!("../remote/dj-session.css");
const STEERING_SCRIPT: &str = include_str!("../remote/dj-steering.js");
const STEERING_STYLE: &str = include_str!("../remote/dj-steering.css");
const HISTORY_SCRIPT: &str = include_str!("../remote/history.js");
const HISTORY_STYLE: &str = include_str!("../remote/history.css");
const SYNC_SCRIPT: &str = include_str!("../remote/library-sync.js");
const SYNC_STYLE: &str = include_str!("../remote/library-sync.css");
const CONNECT_SCRIPT: &str = include_str!("../remote/spotify-connect.js");
const CONNECT_STYLE: &str = include_str!("../remote/spotify-connect.css");
const EQ_SCRIPT: &str = include_str!("../remote/equalizer.js");
const EQ_STYLE: &str = include_str!("../remote/equalizer.css");
const SOUND_COPY_SCRIPT: &str = include_str!("../remote/sound-copy.js");
const SOUND_COPY_STYLE: &str = include_str!("../remote/sound-copy.css");
const FADES_SCRIPT: &str = include_str!("../remote/fades.js");
const FADES_STYLE: &str = include_str!("../remote/fades.css");
const SOURCE_PLAYER_SCRIPT: &str = include_str!("../remote/source-player.js");
const SOURCE_PLAYER_STYLE: &str = include_str!("../remote/source-player.css");
const GROUP_VOLUME_SCRIPT: &str = include_str!("../remote/group-volume.js");
const GROUP_VOLUME_STYLE: &str = include_str!("../remote/group-volume.css");
const STYLE: &str = include_str!("../remote/remote.css");

/// What the remote's documents may do: load only themselves, talk only to
/// their own origin, never be framed.
const CSP: &str = "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self'; \
                   connect-src 'self'; base-uri 'none'; form-action 'none'; \
                   frame-ancestors 'none'";

/// Every remote route.
pub(super) fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    let mut routes = vec![
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
            |_, _, _| {
                asset(
                    "text/javascript; charset=utf-8",
                    &format!(
                        "{QUEUE_SCRIPT}\n{SCRIPT}\n{LIBRARY_SCRIPT}\n{GROUPS_SCRIPT}\n{TIMERS_SCRIPT}\n{SCHEDULES_SCRIPT}\n{SCENES_SCRIPT}\n{MODE_SCRIPT}\n{ANNOUNCEMENTS_SCRIPT}\n{PREFERENCES_SCRIPT}\n{SESSION_SCRIPT}\n{STEERING_SCRIPT}\n{HISTORY_SCRIPT}\n{SYNC_SCRIPT}\n{CONNECT_SCRIPT}\n{PLAYBACK_SCRIPT}\n{EQ_SCRIPT}\n{SOUND_COPY_SCRIPT}\n{SOURCE_PLAYER_SCRIPT}\n{FADES_SCRIPT}\n{GROUP_VOLUME_SCRIPT}"
                    ),
                )
            },
        )
        .response_schema::<String>(200, "The script (text/javascript)"),
        cx.route(
            &Op::get(
                "/remote.css",
                "web_remote_style",
                REMOTE,
                "The web remote's style",
            ),
            |_, _, _| {
                asset(
                    "text/css; charset=utf-8",
                    &format!(
                        "{STYLE}\n{LIBRARY_STYLE}\n{GROUPS_STYLE}\n{TIMERS_STYLE}\n{SCHEDULES_STYLE}\n{SCENES_STYLE}\n{MODE_STYLE}\n{ANNOUNCEMENTS_STYLE}\n{PREFERENCES_STYLE}\n{SESSION_STYLE}\n{STEERING_STYLE}\n{HISTORY_STYLE}\n{SYNC_STYLE}\n{CONNECT_STYLE}\n{PLAYBACK_STYLE}\n{EQ_STYLE}\n{SOUND_COPY_STYLE}\n{SOURCE_PLAYER_STYLE}\n{FADES_STYLE}\n{GROUP_VOLUME_STYLE}"
                    ),
                )
            },
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
    ];
    routes.extend(undo::routes(cx));
    routes.extend(dashboard::routes(cx));
    routes.extend(studio::routes(cx));
    routes
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
fn asset(content_type: &str, text: &str) -> Response {
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
            ("queue.js", QUEUE_SCRIPT),
            ("playback.js", PLAYBACK_SCRIPT),
            ("playback.css", PLAYBACK_STYLE),
            ("library.js", LIBRARY_SCRIPT),
            ("library.css", LIBRARY_STYLE),
            ("remote.css", STYLE),
            ("groups.js", GROUPS_SCRIPT),
            ("groups.css", GROUPS_STYLE),
            ("timers.js", TIMERS_SCRIPT),
            ("timers.css", TIMERS_STYLE),
            ("schedules.js", SCHEDULES_SCRIPT),
            ("schedules.css", SCHEDULES_STYLE),
            ("scenes.js", SCENES_SCRIPT),
            ("scenes.css", SCENES_STYLE),
            ("play-mode.js", MODE_SCRIPT),
            ("play-mode.css", MODE_STYLE),
            ("announcements.js", ANNOUNCEMENTS_SCRIPT),
            ("announcements.css", ANNOUNCEMENTS_STYLE),
            ("dj-preferences.js", PREFERENCES_SCRIPT),
            ("dj-preferences.css", PREFERENCES_STYLE),
            ("dj-session.js", SESSION_SCRIPT),
            ("dj-session.css", SESSION_STYLE),
            ("dj-steering.js", STEERING_SCRIPT),
            ("dj-steering.css", STEERING_STYLE),
            ("history.js", HISTORY_SCRIPT),
            ("history.css", HISTORY_STYLE),
            ("library-sync.js", SYNC_SCRIPT),
            ("library-sync.css", SYNC_STYLE),
            ("spotify-connect.js", CONNECT_SCRIPT),
            ("spotify-connect.css", CONNECT_STYLE),
            ("equalizer.js", EQ_SCRIPT),
            ("equalizer.css", EQ_STYLE),
            ("sound-copy.js", SOUND_COPY_SCRIPT),
            ("sound-copy.css", SOUND_COPY_STYLE),
            ("source-player.js", SOURCE_PLAYER_SCRIPT),
            ("source-player.css", SOURCE_PLAYER_STYLE),
            ("fades.js", FADES_SCRIPT),
            ("fades.css", FADES_STYLE),
            ("group-volume.js", GROUP_VOLUME_SCRIPT),
            ("group-volume.css", GROUP_VOLUME_STYLE),
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
