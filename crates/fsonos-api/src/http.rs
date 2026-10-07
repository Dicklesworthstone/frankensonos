//! The HTTP routes, over a shared [`Surface`].
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `GET /health` | | [`crate::HealthDto`] |
//! | `GET /zones` | | `[ZoneDto]` |
//! | `GET /zones/{room}` | | [`crate::ZoneDto`] (`room` is percent-decoded) |
//! | `POST /play` | [`crate::PlayRequest`] | [`OutcomeDto`] |
//! | `POST /pause`, `/resume`, `/next`, `/previous`, `/ungroup` | [`crate::ZoneRequest`] | [`crate::OutcomeDto`] |
//! | `POST /volume` | [`crate::VolumeRequest`] | [`crate::OutcomeDto`] |
//! | `POST /mute` | [`crate::MuteRequest`] | [`crate::OutcomeDto`] |
//! | `POST /group` | [`crate::GroupRequest`] | [`crate::OutcomeDto`] |
//! | `POST /dj/start`, `/dj/skip`, `/dj/stop` | [`crate::ZoneRequest`] | [`crate::OutcomeDto`] |
//!
//! Failures answer with the code's status and an [`crate::ApiError`] body
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
use crate::plan::{
    self, Command, DjAction, TransportAction, plan_group, plan_mute, plan_play, plan_ungroup,
    plan_volume,
};
use crate::request::ZoneRequest;
use crate::surface::Surface;

/// The API application over `surface`, answering every caller as `client`.
#[must_use]
pub fn app(surface: &Arc<Surface>, client: &Client) -> App {
    let ctl = |tool: &'static str| (Arc::clone(surface), client.clone(), tool);
    let mut app = App::builder()
        .get("/health", health)
        .get("/zones", {
            let (s, c, _) = ctl("list_zones");
            move |_: &RequestContext, _: &mut Request| ready(answer(s.zones(&c)))
        })
        .get("/zones/{room}", {
            let (s, c, _) = ctl("get_zone");
            move |_: &RequestContext, req: &mut Request| {
                ready(answer(path_room(req).and_then(|room| s.zone(&c, &room))))
            }
        })
        .post("/play", control(ctl("play"), plan_play))
        .post("/volume", control(ctl("set_volume"), plan_volume))
        .post("/mute", control(ctl("mute"), plan_mute))
        .post("/group", control(ctl("group"), plan_group))
        .post("/ungroup", control(ctl("ungroup"), plan_ungroup));
    for (path, tool, action) in [
        ("/pause", "pause", TransportAction::Pause),
        ("/resume", "resume", TransportAction::Resume),
        ("/next", "next", TransportAction::Next),
        ("/previous", "previous", TransportAction::Previous),
    ] {
        app = app.post(
            path,
            control(ctl(tool), move |h, r: &ZoneRequest| {
                plan::plan_transport(h, r, action)
            }),
        );
    }
    for (path, tool, action) in [
        ("/dj/start", "dj_start", DjAction::Start),
        ("/dj/skip", "dj_skip", DjAction::Skip),
        ("/dj/stop", "dj_stop", DjAction::Stop),
    ] {
        app = app.post(
            path,
            control(ctl(tool), move |h, r: &ZoneRequest| {
                plan::plan_dj(h, r, action)
            }),
        );
    }
    app.build()
}

fn health(_: &RequestContext, _: &mut Request) -> Ready<Response> {
    let body = HealthDto {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    ready(Response::json(&body).expect("HealthDto serializes"))
}

/// A POST route: parse the JSON body as `B`, plan it, carry it out.
fn control<B, P>(
    (surface, client, tool): (Arc<Surface>, Client, &'static str),
    plan: P,
) -> impl Fn(&RequestContext, &mut Request) -> Ready<Response> + Send + Sync + 'static
where
    B: DeserializeOwned,
    P: Fn(&[HouseholdState], &B) -> Result<Command, Failure> + Send + Sync + 'static,
{
    move |_: &RequestContext, req: &mut Request| {
        let outcome =
            body::<B>(req).and_then(|body| surface.control(&client, tool, |h| plan(h, &body)));
        ready(answer(outcome))
    }
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
