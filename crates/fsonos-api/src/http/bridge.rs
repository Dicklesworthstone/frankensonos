//! The legacy Sonos bridge's HTTP API, served by `fsonos serve
//! --bridge-compat <loopback address>` (vg3s.9).
//!
//! The owner's agents called a small Python bridge that applied no policy:
//! no caps, no quiet hours, no log, no undo. This listener answers its routes
//! with its JSON shapes, so a caller moves over by changing nothing, and every
//! call runs as the `bridge-compat` client through the one choke point the
//! other surfaces use: a write is held to that client's caps, logged, and
//! undoable.
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `GET /health` | | `{ok, devices}` |
//! | `GET /zones` | | `{zones: [{name, ip, model, is_coordinator, coordinator, group_members, transport_state}]}` |
//! | `GET /status?zone=` | | `{zone, transport_state, title, artist, album, position, duration, volume, muted}` |
//! | `GET /favorites?zone=` | | `{zone, favorites: [{title, uri, meta}]}` (the first 30) |
//! | `GET /mediainfo?zone=` | | `{zone, current_uri, current_uri_metadata}` |
//! | `POST /rediscover` | | `{ok, devices}` |
//! | `POST /play` | `{zone, uri, title?}` | `{ok, played_on}` |
//! | `POST /pause`, `/resume`, `/next`, `/previous` | `{zone}` | `{ok, zone}` |
//! | `POST /volume` | `{zone, level}` | `{ok, zone, volume}`: the level set, after the caps |
//! | `POST /mute` | `{zone, muted}` | `{ok, zone, muted}` |
//!
//! An unknown zone answers 404 `{error: "no such zone", zone, known}`; any
//! other failure answers its code's status with `{error, code, hint}`. A
//! request from a web page (one carrying `Origin` or `Sec-Fetch-Site`) is
//! refused: the bridge had no defense against a page posting to it.

use fastapi::core::{BoxFuture, RouteEntry};
use fastapi::{App, AppBuilder, Method, Request, Response, ResponseBody, Route, StatusCode};
use fsonos_core::policy::Client;
use serde::Deserialize;
use serde_json::{Value, json};
use std::future::ready;
use std::sync::Arc;

use super::query_param;
use crate::failure::{ErrorCode, Failure};
use crate::plan::{TransportAction, plan_mute_set, plan_play, plan_transport_set, plan_volume_set};
use crate::request::{MuteRequest, PlayRequest, VolumeRequest, ZoneRequest};
use crate::surface::Surface;

/// The bridge's API over `surface`, answering every caller as
/// [`Client::BridgeCompat`].
#[must_use]
pub fn app(surface: &Arc<Surface>) -> App {
    let route = |method, path: &'static str, work: Work| entry(surface, method, path, work);
    let read = |of: Read| -> Work {
        Box::new(move |s: &Surface, c: &Client, req: &mut Request| zone_read(s, c, req, of))
    };
    let skip = |tool: &'static str, action: TransportAction| -> Work {
        Box::new(move |s: &Surface, c: &Client, req: &mut Request| {
            transport(s, c, req, tool, action)
        })
    };
    [
        route(Method::Get, "/health", Box::new(health)),
        route(Method::Get, "/zones", Box::new(zones)),
        route(Method::Get, "/status", read(Surface::bridge_status)),
        route(Method::Get, "/favorites", read(Surface::bridge_favorites)),
        route(Method::Get, "/mediainfo", read(Surface::bridge_media_info)),
        route(Method::Post, "/rediscover", Box::new(rediscover)),
        route(Method::Post, "/play", Box::new(play)),
        route(
            Method::Post,
            "/pause",
            skip("pause", TransportAction::Pause),
        ),
        route(
            Method::Post,
            "/resume",
            skip("resume", TransportAction::Resume),
        ),
        route(Method::Post, "/next", skip("next", TransportAction::Next)),
        route(
            Method::Post,
            "/previous",
            skip("previous", TransportAction::Previous),
        ),
        route(Method::Post, "/volume", Box::new(volume)),
        route(Method::Post, "/mute", Box::new(mute)),
    ]
    .into_iter()
    .fold(App::builder(), AppBuilder::route_entry)
    .build()
}

/// A route's work: the surface, the caller, the request.
type Work = Box<dyn Fn(&Surface, &Client, &mut Request) -> Response + Send + Sync>;

/// A `?zone=` read on the surface.
type Read = fn(&Surface, &Client, &str) -> Result<Value, Failure>;

/// `path` doing `work` as [`Client::BridgeCompat`], on a thread of its own
/// (see [`crate::off_thread`]), after the browser check.
fn entry(surface: &Arc<Surface>, method: Method, path: &'static str, work: Work) -> RouteEntry {
    let (surface, work) = (Arc::clone(surface), Arc::new(work));
    RouteEntry::from_route(Route::new(method, path), move |_, req| {
        if let Some(refused) = from_a_browser(req) {
            return Box::pin(ready(refused)) as BoxFuture<'_, Response>;
        }
        let stand_in = Request::new(req.method(), req.path().to_owned());
        let mut owned = std::mem::replace(req, stand_in);
        let (surface, work) = (Arc::clone(&surface), Arc::clone(&work));
        match crate::off_thread::off_thread(move || {
            work(&surface, &Client::BridgeCompat, &mut owned)
        }) {
            Ok(working) => Box::pin(async move {
                working.await.unwrap_or_else(|| {
                    failed(&Failure::new(
                        ErrorCode::Internal,
                        "the call stopped before it answered",
                    ))
                })
            }) as BoxFuture<'_, Response>,
            Err(busy) => Box::pin(ready(failed(&busy))) as BoxFuture<'_, Response>,
        }
    })
}

/// A web page's request: refused, so no page can drive the speakers
/// through this listener (it has no CORS or token check to rely on).
fn from_a_browser(req: &Request) -> Option<Response> {
    let headers = req.headers();
    (headers.get("origin").is_some() || headers.get("sec-fetch-site").is_some()).then(|| {
        reply(
            403,
            &json!({ "error": "a web page may not call the bridge-compatible listener" }),
        )
    })
}

fn reply(status: u16, body: &Value) -> Response {
    Response::with_status(StatusCode::from_u16(status))
        .header("content-type", b"application/json".to_vec())
        .body(ResponseBody::Bytes(body.to_string().into_bytes()))
}

/// `failure` as the bridge answered one: `{error}` with the status, plus the
/// stable code and hint every other surface gives.
fn failed(failure: &Failure) -> Response {
    reply(
        failure.status(),
        &json!({
            "error": format!("{}: {}", failure.code.as_str(), failure.detail),
            "code": failure.code.as_str(),
            "hint": failure.hint,
        }),
    )
}

/// An unknown `zone` as the bridge answered it (404, with the rooms there
/// are), else `failure`.
fn refused(surface: &Surface, zone: &str, failure: &Failure) -> Response {
    if failure.code == ErrorCode::UnknownRoom {
        return reply(
            404,
            &json!({ "error": "no such zone", "zone": zone, "known": surface.bridge_known() }),
        );
    }
    failed(failure)
}

fn answer(result: Result<Value, Failure>) -> Response {
    match result {
        Ok(body) => reply(200, &body),
        Err(failure) => failed(&failure),
    }
}

fn health(surface: &Surface, _: &Client, _: &mut Request) -> Response {
    reply(
        200,
        &json!({ "ok": true, "devices": surface.bridge_devices() }),
    )
}

fn zones(surface: &Surface, client: &Client, _: &mut Request) -> Response {
    answer(surface.bridge_zones(client))
}

fn rediscover(surface: &Surface, client: &Client, _: &mut Request) -> Response {
    answer(
        surface
            .bridge_rediscover(client)
            .map(|devices| json!({ "ok": true, "devices": devices })),
    )
}

/// A `GET ?zone=` read.
fn zone_read(surface: &Surface, client: &Client, req: &mut Request, read: Read) -> Response {
    let zone = match query_param(req, "zone") {
        Ok(zone) => zone.unwrap_or_default(),
        Err(failure) => return failed(&failure),
    };
    match read(surface, client, &zone) {
        Ok(body) => reply(200, &body),
        Err(failure) => refused(surface, &zone, &failure),
    }
}

/// A write's body: every field the bridge read, each optional as it was.
#[derive(Debug, Default, Deserialize)]
struct Body {
    #[serde(default)]
    zone: String,
    #[serde(default)]
    uri: String,
    title: Option<String>,
    /// A number, or a string of one: the bridge took either.
    level: Option<Value>,
    muted: Option<bool>,
}

impl Body {
    /// The volume asked for, 0–100: 20 when none, as the bridge's default.
    fn level(&self) -> Result<i64, Failure> {
        let level = match &self.level {
            None => Some(20),
            Some(Value::Number(n)) => n.as_i64(),
            Some(Value::String(s)) => s.trim().parse().ok(),
            Some(_) => None,
        };
        level
            .map(|level| level.clamp(0, 100))
            .ok_or_else(|| Failure::invalid("level is not a whole number"))
    }
}

/// The JSON body (an empty one is `{}`, as the bridge read it).
fn body(req: &mut Request) -> Result<Body, Failure> {
    let bytes = req.take_body().into_bytes();
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(Body::default());
    }
    serde_json::from_slice(&bytes)
        .map_err(|e| Failure::invalid(format!("the request body is not valid JSON: {e}")))
}

/// Do `work` for the body's zone and answer with what it did and the room
/// that coordinates the zone.
fn write(
    surface: &Surface,
    client: &Client,
    req: &mut Request,
    work: impl FnOnce(&Body) -> Result<Value, Failure>,
) -> Response {
    let body = match body(req) {
        Ok(body) => body,
        Err(failure) => return failed(&failure),
    };
    let done = surface
        .bridge_coordinator(client, &body.zone)
        .and_then(|zone| {
            work(&body).map(|mut done| {
                done["ok"] = json!(true);
                if done.get("zone").is_some() {
                    done["zone"] = json!(zone);
                }
                if done.get("played_on").is_some() {
                    done["played_on"] = json!(zone);
                }
                done
            })
        });
    match done {
        Ok(done) => reply(200, &done),
        Err(failure) => refused(surface, &body.zone, &failure),
    }
}

/// `POST /play`: a Spotify track, album or playlist (the bridge's legacy
/// `spotify:user:<user>:playlist:<id>` names the playlist), or a URI the
/// speakers play as given.
fn play(surface: &Surface, client: &Client, req: &mut Request) -> Response {
    write(surface, client, req, |b| {
        let req = PlayRequest {
            zone: b.zone.clone(),
            source_uri: playlist_of_user(&b.uri).unwrap_or_else(|| b.uri.clone()),
            title: b.title.clone(),
        };
        surface.control(client, "play", |h| plan_play(h, &req))?;
        Ok(json!({ "played_on": "" }))
    })
}

/// `spotify:user:<user>:playlist:<id>` as `spotify:playlist:<id>`.
fn playlist_of_user(uri: &str) -> Option<String> {
    let rest = uri.trim().strip_prefix("spotify:user:")?;
    let (_, id) = rest.split_once(":playlist:")?;
    (!id.is_empty() && !id.contains(':')).then(|| format!("spotify:playlist:{id}"))
}

fn transport(
    surface: &Surface,
    client: &Client,
    req: &mut Request,
    tool: &'static str,
    action: TransportAction,
) -> Response {
    write(surface, client, req, |b| {
        let req = ZoneRequest {
            zone: b.zone.clone(),
        };
        surface.control_set(client, tool, |h| plan_transport_set(h, &req, action))?;
        Ok(json!({ "zone": "" }))
    })
}

/// `POST /volume`: the bridge's default level is 20; the answer is the level
/// the caps let through.
fn volume(surface: &Surface, client: &Client, req: &mut Request) -> Response {
    write(surface, client, req, |b| {
        let level = b.level()?;
        let req = VolumeRequest {
            zone: b.zone.clone(),
            volume: Some(level),
            delta: None,
            group: false,
        };
        let done = surface.control_set(client, "set_volume", |h| plan_volume_set(h, &req))?;
        Ok(json!({ "zone": "", "volume": done.volume.map_or(level, i64::from) }))
    })
}

/// `POST /mute`: mutes unless `muted` is false, as the bridge did.
fn mute(surface: &Surface, client: &Client, req: &mut Request) -> Response {
    write(surface, client, req, |b| {
        let mute = b.muted.unwrap_or(true);
        let req = MuteRequest {
            zone: b.zone.clone(),
            mute,
        };
        surface.control_set(client, "mute", |h| plan_mute_set(h, &req))?;
        Ok(json!({ "zone": "", "muted": mute }))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_level_is_a_number_or_a_string_of_one_and_defaults_to_twenty() {
        let level = |v: Value| {
            serde_json::from_value::<Body>(json!({ "zone": "Den", "level": v }))
                .unwrap()
                .level()
        };
        assert_eq!(Body::default().level().unwrap(), 20);
        assert_eq!(level(json!(45)).unwrap(), 45);
        assert_eq!(level(json!(" 30 ")).unwrap(), 30);
        assert_eq!(level(json!(250)).unwrap(), 100);
        assert_eq!(level(json!(-3)).unwrap(), 0);
        assert!(level(json!("loud")).is_err());
        assert!(level(json!(true)).is_err());
    }

    #[test]
    fn a_legacy_user_playlist_names_the_playlist() {
        assert_eq!(
            playlist_of_user("spotify:user:someone:playlist:37i9dQZF1DX0XUsuxWHRQd").as_deref(),
            Some("spotify:playlist:37i9dQZF1DX0XUsuxWHRQd")
        );
        assert_eq!(playlist_of_user("spotify:playlist:abc"), None);
        assert_eq!(playlist_of_user("spotify:user:someone:playlist:"), None);
        assert_eq!(playlist_of_user("spotify:track:abc"), None);
    }
}
