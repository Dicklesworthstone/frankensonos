//! The HTTP routes, over a shared [`Surface`].
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `GET /health` | | [`crate::HealthDto`] |
//! | `GET /zones` | | `[ZoneDto]` |
//! | `GET /zones/{room}` | | [`crate::ZoneDto`] (`room` is percent-decoded) |
//! | `GET /zones/{room}/state` | | [`crate::ZoneStateDto`] |
//! | `GET /favorites?zone=<room>` | | `[FavoriteDto]` of the room's household |
//! | `POST /play/favorite` | [`crate::PlayFavoriteRequest`] | [`crate::OutcomeDto`] |
//! | `GET /doctor` | | the doctor report (`schema`, `exit_code`, `counts`, `checks`) |
//! | `GET /actions?client=&since=&limit=` | | `[ActionDto]`, newest first |
//! | `POST /undo` | [`crate::UndoRequest`] | [`crate::UndoDto`] |
//! | `POST /play` | [`crate::PlayRequest`] | [`OutcomeDto`] |
//! | `POST /pause`, `/resume`, `/next`, `/previous`, `/ungroup` | [`crate::ZoneRequest`] | [`crate::OutcomeDto`] |
//! | `POST /volume` | [`crate::VolumeRequest`] | [`crate::OutcomeDto`] |
//! | `POST /mute` | [`crate::MuteRequest`] | [`crate::OutcomeDto`] |
//! | `POST /group` | [`crate::GroupRequest`] | [`crate::OutcomeDto`] |
//! | `POST /dj/start`, `/dj/skip`, `/dj/stop` | [`crate::ZoneRequest`] | [`crate::OutcomeDto`] |
//!
//! Every route first passes the listener's [`WebPolicy`] (a present Origin
//! must be the daemon's own; POSTs must be JSON); the listener itself admits
//! only its own Host names. Failures answer with the code's status and an
//! [`crate::ApiError`] body
//! (`docs/ERRORS.md`). Every call runs as the listener's [`Client`] under the
//! house policy. Speaker I/O is synchronous inside the handler.

use fastapi::{App, PathParams, Request, RequestContext, Response};
use fsonos_core::HouseholdState;
use fsonos_core::policy::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::future::{Ready, ready};
use std::sync::Arc;

use crate::HealthDto;
use crate::failure::Failure;
use crate::log::{ActionDto, ActionsQuery, UndoDto, UndoRequest};
use crate::plan::{
    self, Command, DjAction, TransportAction, plan_group, plan_mute, plan_play, plan_ungroup,
    plan_volume,
};
use crate::request::{PlayFavoriteRequest, ZoneRequest};
use crate::surface::Surface;
use crate::web::WebPolicy;

/// The API application over `surface`, answering every caller as `client`,
/// with the listener's browser-safety rules (`web`, see [`crate::web`]).
#[must_use]
pub fn app(surface: &Arc<Surface>, client: &Client, web: &WebPolicy) -> App {
    let web = Arc::new(web.clone());
    let read = |handle: Handler| admitted(&web, false, handle);
    let write = |handle: Handler| admitted(&web, true, handle);
    let ctl = |tool: &'static str| (Arc::clone(surface), client.clone(), tool);
    let mut app = App::builder()
        .get("/health", read(Box::new(|_| health())))
        .get("/zones", {
            let (s, c, _) = ctl("list_zones");
            read(Box::new(move |_| answer(s.zones(&c))))
        })
        .get("/zones/{room}", {
            let (s, c, _) = ctl("get_zone");
            read(Box::new(move |req| {
                answer(path_room(req).and_then(|room| s.zone(&c, &room)))
            }))
        })
        .get("/zones/{room}/state", {
            let (s, c, _) = ctl("get_zone_state");
            read(Box::new(move |req| {
                answer(path_room(req).and_then(|room| s.zone_state(&c, &room)))
            }))
        })
        .get("/favorites", {
            let (s, c, _) = ctl("list_favorites");
            read(Box::new(move |req| {
                answer(query_zone(req).and_then(|zone| s.favorites(&c, &zone)))
            }))
        })
        .get("/doctor", {
            let (s, c, _) = ctl("doctor");
            read(Box::new(move |_| answer(s.doctor(&c).map(|r| r.to_json()))))
        })
        .get("/actions", {
            let (s, c, _) = ctl("recent_actions");
            read(Box::new(move |req| {
                let listed = actions_query(req).and_then(|q| s.recent_actions(&c, &q.filter()));
                answer(listed.map(|a| a.iter().map(ActionDto::from).collect::<Vec<_>>()))
            }))
        })
        .post("/play/favorite", {
            let (s, c, _) = ctl("play_favorite");
            write(Box::new(move |req| {
                answer(body::<PlayFavoriteRequest>(req).and_then(|b| s.play_favorite(&c, &b)))
            }))
        })
        .post("/undo", {
            let (s, c, _) = ctl("undo");
            write(Box::new(move |req| {
                let undone = body_or_default::<UndoRequest>(req, UndoRequest { own_only: true })
                    .and_then(|r| s.undo(&c, r.own_only));
                answer(undone.map(UndoDto::from))
            }))
        })
        .post("/play", write(control(ctl("play"), plan_play)))
        .post("/volume", write(control(ctl("set_volume"), plan_volume)))
        .post("/mute", write(control(ctl("mute"), plan_mute)))
        .post("/group", write(control(ctl("group"), plan_group)))
        .post("/ungroup", write(control(ctl("ungroup"), plan_ungroup)));
    for (path, tool, action) in [
        ("/pause", "pause", TransportAction::Pause),
        ("/resume", "resume", TransportAction::Resume),
        ("/next", "next", TransportAction::Next),
        ("/previous", "previous", TransportAction::Previous),
    ] {
        let handle = control(ctl(tool), move |h, r: &ZoneRequest| {
            plan::plan_transport(h, r, action)
        });
        app = app.post(path, write(handle));
    }
    for (path, tool, action) in [
        ("/dj/start", "dj_start", DjAction::Start),
        ("/dj/skip", "dj_skip", DjAction::Skip),
        ("/dj/stop", "dj_stop", DjAction::Stop),
    ] {
        let handle = control(ctl(tool), move |h, r: &ZoneRequest| {
            plan::plan_dj(h, r, action)
        });
        app = app.post(path, write(handle));
    }
    app.build()
}

/// A route's work, after the browser-safety checks passed.
type Handler = Box<dyn Fn(&mut Request) -> Response + Send + Sync>;

/// `handle` behind `web`'s checks; `write` routes must also be JSON.
fn admitted(
    web: &Arc<WebPolicy>,
    write: bool,
    handle: Handler,
) -> impl Fn(&RequestContext, &mut Request) -> Ready<Response> + Send + Sync + 'static {
    let web = Arc::clone(web);
    move |_: &RequestContext, req: &mut Request| {
        ready(match web.admit(req, write) {
            Ok(()) => handle(req),
            Err(refused) => refused.http_response(),
        })
    }
}

fn health() -> Response {
    let body = HealthDto {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    Response::json(&body).expect("HealthDto serializes")
}

/// A control route: parse the JSON body as `B`, plan it, carry it out.
fn control<B, P>((surface, client, tool): (Arc<Surface>, Client, &'static str), plan: P) -> Handler
where
    B: DeserializeOwned + 'static,
    P: Fn(&[HouseholdState], &B) -> Result<Command, Failure> + Send + Sync + 'static,
{
    Box::new(move |req: &mut Request| {
        answer(body::<B>(req).and_then(|body| surface.control(&client, tool, |h| plan(h, &body))))
    })
}

fn body<B: DeserializeOwned>(req: &mut Request) -> Result<B, Failure> {
    let bytes = req.take_body().into_bytes();
    serde_json::from_slice(&bytes).map_err(|e| {
        Failure::invalid(format!("the request body is not valid for this route: {e}"))
            .with_hint("Send a JSON object with the fields docs/ERRORS.md and the API docs name.")
    })
}

fn path_room(req: &Request) -> Result<String, Failure> {
    let raw = req
        .get_extension::<PathParams>()
        .and_then(|p| p.get("room"))
        .unwrap_or_default();
    percent_decode(raw)
        .ok_or_else(|| Failure::invalid(format!("room {raw:?} is not valid percent-encoded UTF-8")))
}

/// A query parameter, percent-decoded (`+` is a space); `Ok(None)` when it
/// is absent.
fn query_param(req: &Request, name: &str) -> Result<Option<String>, Failure> {
    let prefix = format!("{name}=");
    let Some(raw) = req
        .query()
        .unwrap_or_default()
        .split('&')
        .find_map(|pair| pair.strip_prefix(prefix.as_str()))
    else {
        return Ok(None);
    };
    percent_decode(&raw.replace('+', " "))
        .map(Some)
        .ok_or_else(|| {
            Failure::invalid(format!("{name} {raw:?} is not valid percent-encoded UTF-8"))
        })
}

/// The `zone` query parameter, which `GET /favorites` requires.
fn query_zone(req: &Request) -> Result<String, Failure> {
    query_param(req, "zone")?.ok_or_else(|| {
        Failure::invalid("name the room: GET /favorites?zone=<room>")
            .with_hint("Add ?zone=<room>; any room of the household will do.")
    })
}

/// `GET /actions`' optional `client`, `since` and `limit`.
fn actions_query(req: &Request) -> Result<ActionsQuery, Failure> {
    let number = |name: &str| -> Result<Option<i64>, Failure> {
        query_param(req, name)?
            .map(|v| {
                v.parse::<i64>().map_err(|_| {
                    Failure::invalid(format!("{name} must be a whole number, got {v:?}"))
                })
            })
            .transpose()
    };
    Ok(ActionsQuery {
        client: query_param(req, "client")?,
        since: number("since")?,
        limit: number("limit")?.map(|n| usize::try_from(n.max(0)).unwrap_or(0)),
    })
}

/// A JSON body, or `default` when the body is empty.
fn body_or_default<B: DeserializeOwned>(req: &mut Request, default: B) -> Result<B, Failure> {
    let bytes = req.take_body().into_bytes();
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(default);
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| Failure::invalid(format!("the request body is not valid for this route: {e}")))
}

/// Decode `%XX` escapes (and nothing else) into UTF-8 text.
fn percent_decode(raw: &str) -> Option<String> {
    let bytes = raw.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' {
            let hex = raw.get(i + 1..i + 3)?;
            out.push(u8::from_str_radix(hex, 16).ok()?);
            i += 3;
        } else {
            out.push(bytes[i]);
            i += 1;
        }
    }
    String::from_utf8(out).ok()
}

fn answer<T: Serialize>(result: Result<T, Failure>) -> Response {
    match result {
        Ok(value) => Response::json(&value).unwrap_or_else(|e| {
            Failure::new(crate::ErrorCode::Internal, e.to_string()).http_response()
        }),
        Err(failure) => failure.http_response(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn percent_decoding_handles_room_names() {
        assert_eq!(percent_decode("Kitchen").unwrap(), "Kitchen");
        assert_eq!(percent_decode("Living%20Room").unwrap(), "Living Room");
        assert_eq!(
            percent_decode("Ada%E2%80%99s%20Studio").unwrap(),
            "Ada\u{2019}s Studio"
        );
        assert_eq!(percent_decode("Den%40S1").unwrap(), "Den@S1");
        assert_eq!(percent_decode("bad%2"), None);
        assert_eq!(percent_decode("bad%zz"), None);
        assert_eq!(percent_decode("%FF"), None);
    }
}
