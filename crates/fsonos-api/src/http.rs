//! The HTTP routes, over a shared [`Surface`].
//!
//! | Route | Body | Answer |
//! |---|---|---|
//! | `GET /health` | | [`crate::HealthDto`] |
//! | `GET /openapi.json` | | the OpenAPI document of every other route |
//! | `GET /events?since=<id>` | | server-sent events as the house changes ([`crate::events`]); resumes after `Last-Event-ID` or `since` |
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
//! (`docs/ERRORS.md`). Each operation id in the OpenAPI document is the
//! name of the MCP tool that does the same. Every call runs as its caller ([`Identity`]) under the
//! house policy. Speaker I/O is synchronous inside the handler.

use fastapi::core::{BoxFuture, RouteEntry};
use fastapi::fastapi_openapi;
use fastapi::{
    App, AppBuilder, JsonSchema, Method, OpenApiConfig, PathParams, Request, Response,
    ResponseBody, Route,
};
use fsonos_core::policy::Client;
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::collections::BTreeMap;
use std::future::ready;
use std::sync::Arc;

use crate::failure::{ErrorCode, Failure};
use crate::identity::Identity;
use crate::log::{ActionDto, ActionsQuery, UndoDto, UndoRequest};
use crate::plan::{
    self, Command, DjAction, Rooms, TransportAction, plan_group, plan_mute, plan_play,
    plan_ungroup, plan_volume,
};
use crate::reads::{FavoriteDto, HitDto, PlayDto, RoomDto, ZoneStateDto};
use crate::request::{
    GroupRequest, MuteRequest, PlayFavoriteRequest, PlayRequest, SearchRequest, VolumeRequest,
    ZoneRequest,
};
use crate::surface::Surface;
use crate::web::WebPolicy;
use crate::{ApiError, HealthDto, OutcomeDto, ZoneDto};

/// The API application over `surface`, answering every caller as `client`,
/// with the listener's browser-safety rules (`web`, see [`crate::web`]).
#[must_use]
pub fn app(surface: &Arc<Surface>, identity: &Identity, web: &WebPolicy) -> App {
    let web = Arc::new(web.clone());
    let entries = routes(&Ctx {
        surface,
        identity,
        web: &web,
    });
    let spec = openapi_document(&entries);
    let openapi = Op::get(
        "/openapi.json",
        "openapi",
        DAEMON,
        "This API's OpenAPI document",
    )
    .entry(&web, Box::new(move |_| json_text(&spec)));
    entries
        .into_iter()
        .chain([openapi])
        .fold(App::builder(), AppBuilder::route_entry)
        .build()
}

/// The OpenAPI document describing `entries`.
fn openapi_document(entries: &[RouteEntry]) -> String {
    let config = OpenApiConfig::new()
        .title("FrankenSonos")
        .version(env!("CARGO_PKG_VERSION"))
        .description(
            "Control the Sonos players of your own households from anywhere on your \
             tailnet. Failures carry a stable `code` (docs/ERRORS.md); every operation id \
             matches the MCP tool that does the same.",
        );
    let documented = entries
        .iter()
        .cloned()
        .fold(App::builder().openapi(config), AppBuilder::route_entry)
        .build();
    documented.openapi_spec().unwrap_or("{}").to_string()
}

const DAEMON: &str = "daemon";
const ZONES: &str = "zones";
const FAVORITES: &str = "favorites";
const CONTROL: &str = "control";
const DJ: &str = "dj";
const LOG: &str = "log";

/// Every route but `/openapi.json`.
fn routes(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    let mut routes = reads(cx);
    routes.extend(house(cx));
    routes.extend(controls(cx));
    routes
}

/// The daemon and the zones.
fn reads(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/events",
                "events",
                DAEMON,
                "The house's changes as server-sent events (text/event-stream)",
            ),
            |s, c, req| match s
                .events(c)
                .and_then(|bus| Ok((events_query(req, &bus)?, bus)))
            {
                Ok((q, bus)) => crate::events::response(bus, q.since.unwrap_or_default()),
                Err(failure) => failure.http_response(),
            },
        )
        .query_schema::<EventsQuery>(false),
        cx.route(
            &Op::get(
                "/health",
                "health",
                DAEMON,
                "Whether the daemon is up, and its version",
            ),
            |_, _, _| health(),
        )
        .response_schema::<HealthDto>(200, "The daemon answers"),
        cx.route(
            &Op::get(
                "/doctor",
                "doctor",
                DAEMON,
                "Check this setup; each problem names its fix",
            ),
            |s, c, _| answer(s.doctor(c).map(|r| r.to_json())),
        )
        .response_schema::<serde_json::Value>(
            200,
            "The doctor report: `schema`, `exit_code`, `counts`, `checks`",
        ),
        cx.route(
            &Op::get(
                "/zones",
                "list_zones",
                ZONES,
                "Every zone (playing group), both households",
            ),
            |s, c, _| answer(s.zones(c)),
        )
        .response_schema::<Vec<ZoneDto>>(200, "The zones"),
        cx.route(
            &Op::get(
                "/rooms",
                "list_rooms",
                ZONES,
                "Every room, its household and zone, and the aliases that name it",
            ),
            |s, c, _| answer(s.rooms(c)),
        )
        .response_schema::<Vec<RoomDto>>(200, "The rooms"),
        cx.route(
            &Op::get(
                "/zones/{room}",
                "get_zone",
                ZONES,
                "The zone a room plays in",
            ),
            |s, c, req| answer(path_room(req).and_then(|room| s.zone(c, &room))),
        )
        .response_schema::<ZoneDto>(200, "The room's zone"),
        cx.route(
            &Op::get(
                "/zones/{room}/state",
                "get_zone_state",
                ZONES,
                "What a room's zone plays, and how loud the room is",
            ),
            |s, c, req| answer(path_room(req).and_then(|room| s.zone_state(c, &room))),
        )
        .response_schema::<ZoneStateDto>(200, "The zone's live state"),
    ]
}

/// Favorites, the action log and undo.
fn house(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    vec![
        cx.route(
            &Op::get(
                "/library/search",
                "search_library",
                FAVORITES,
                "Search the owner's library (and a room's favorites) for music",
            ),
            |s, c, req| answer(search_query(req).and_then(|q| s.search_library(c, &q))),
        )
        .query_schema::<SearchQuery>(true)
        .response_schema::<Vec<HitDto>>(200, "The best matches, best first"),
        cx.route(
            &Op::get(
                "/history",
                "recent_plays",
                LOG,
                "What played recently, newest first",
            ),
            |s, c, req| {
                answer(
                    history_query(req)
                        .and_then(|q| s.recent_plays(c, q.zone.as_deref(), q.limit.unwrap_or(20))),
                )
            },
        )
        .query_schema::<HistoryQuery>(false)
        .response_schema::<Vec<PlayDto>>(200, "The recorded plays"),
        cx.route(
            &Op::get(
                "/favorites",
                "list_favorites",
                FAVORITES,
                "The Sonos favorites of a room's household",
            ),
            |s, c, req| answer(favorites_query(req).and_then(|q| s.favorites(c, &q.zone))),
        )
        .query_schema::<FavoritesQuery>(true)
        .response_schema::<Vec<FavoriteDto>>(200, "The favorites, in the household's order"),
        cx.route(
            &Op::post(
                "/play/favorite",
                "play_favorite",
                FAVORITES,
                "Play one of the household's Sonos favorites",
            ),
            |s, c, req| {
                answer(body::<PlayFavoriteRequest>(req).and_then(|b| s.play_favorite(c, &b)))
            },
        )
        .request_schema::<PlayFavoriteRequest>(true)
        .response_schema::<OutcomeDto>(200, "What was done"),
        cx.route(
            &Op::get(
                "/actions",
                "recent_actions",
                LOG,
                "The action log, newest first",
            ),
            |s, c, req| {
                let listed = actions_query(req).and_then(|q| s.recent_actions(c, &q.filter()));
                answer(listed.map(|a| a.iter().map(ActionDto::from).collect::<Vec<_>>()))
            },
        )
        .query_schema::<ActionsQuery>(false)
        .response_schema::<Vec<ActionDto>>(200, "The logged actions"),
        cx.route(
            &Op::post("/undo", "undo_last", LOG, "Undo the newest undoable action"),
            |s, c, req| {
                let undone = body_or_default::<UndoRequest>(req, UndoRequest { own_only: true })
                    .and_then(|r| s.undo(c, r.own_only));
                answer(undone.map(UndoDto::from))
            },
        )
        .request_schema::<UndoRequest>(false)
        .response_schema::<UndoDto>(200, "What was undone, if anything"),
    ]
}

/// The speaker controls and the DJ.
fn controls(cx: &Ctx<'_>) -> Vec<RouteEntry> {
    let mut routes = vec![
        cx.control(
            Op::post(
                "/play",
                "play",
                CONTROL,
                "Play a Spotify item, link or stream on a zone",
            ),
            |h, r: &PlayRequest| plan_play(h, r),
        ),
        cx.control(
            Op::post(
                "/volume",
                "set_volume",
                CONTROL,
                "Set or change a room's volume, or its group's",
            ),
            |h, r: &VolumeRequest| plan_volume(h, r),
        ),
        cx.control(
            Op::post("/mute", "mute", CONTROL, "Mute or unmute a room"),
            |h, r: &MuteRequest| plan_mute(h, r),
        ),
        cx.control(
            Op::post(
                "/group",
                "group",
                CONTROL,
                "Move a room into another room's group",
            ),
            |h, r: &GroupRequest| plan_group(h, r),
        ),
        cx.control(
            Op::post(
                "/ungroup",
                "ungroup",
                CONTROL,
                "Take a room out of its group",
            ),
            |h, r: &ZoneRequest| plan_ungroup(h, r),
        ),
    ];
    for (path, id, summary, action) in [
        (
            "/pause",
            "pause",
            "Pause a room's zone",
            TransportAction::Pause,
        ),
        (
            "/resume",
            "resume",
            "Resume a room's zone",
            TransportAction::Resume,
        ),
        (
            "/next",
            "next",
            "Skip to the next track",
            TransportAction::Next,
        ),
        (
            "/previous",
            "previous",
            "Go back a track",
            TransportAction::Previous,
        ),
    ] {
        routes.push(cx.control(
            Op::post(path, id, CONTROL, summary),
            move |h, r: &ZoneRequest| plan::plan_transport(h, r, action),
        ));
    }
    for (path, id, summary, action) in [
        (
            "/dj/start",
            "dj_start",
            "Start the DJ on a room's zone",
            DjAction::Start,
        ),
        (
            "/dj/skip",
            "dj_skip",
            "Skip the DJ's current pick",
            DjAction::Skip,
        ),
        ("/dj/stop", "dj_stop", "Stop the DJ", DjAction::Stop),
    ] {
        routes.push(cx.control(
            Op::post(path, id, DJ, summary),
            move |h, r: &ZoneRequest| plan::plan_dj(h, r, action),
        ));
    }
    routes
}

/// What every route works with.
struct Ctx<'a> {
    surface: &'a Arc<Surface>,
    identity: &'a Identity,
    web: &'a Arc<WebPolicy>,
}

impl Ctx<'_> {
    /// The route `op`, doing `work` on the surface as the listener's caller.
    fn route<F>(&self, op: &Op, work: F) -> RouteEntry
    where
        F: Fn(&Surface, &Client, &mut Request) -> Response + Send + Sync + 'static,
    {
        let (surface, identity) = (Arc::clone(self.surface), self.identity.clone());
        op.entry(
            self.web,
            Box::new(move |req| {
                let client = identity.of(req);
                work(&surface, &client, req)
            }),
        )
    }

    /// A control route: parse the JSON body as `B`, plan it, carry it out.
    /// The action is logged under the operation id (the MCP tool's name).
    fn control<B, P>(&self, op: Op, plan: P) -> RouteEntry
    where
        B: DeserializeOwned + fastapi_openapi::JsonSchema + 'static,
        P: Fn(&Rooms<'_>, &B) -> Result<Command, Failure> + Send + Sync + 'static,
    {
        let tool = op.id;
        self.route(&op, move |surface, client, req| {
            answer(body::<B>(req).and_then(|b| surface.control(client, tool, |h| plan(h, &b))))
        })
        .request_schema::<B>(true)
        .response_schema::<OutcomeDto>(200, "What was done")
    }
}

/// One route as the OpenAPI document names it.
#[derive(Clone, Copy)]
struct Op {
    method: Method,
    path: &'static str,
    /// The operation id: the name of the MCP tool doing the same, if any.
    id: &'static str,
    tag: &'static str,
    summary: &'static str,
}

impl Op {
    const fn get(
        path: &'static str,
        id: &'static str,
        tag: &'static str,
        summary: &'static str,
    ) -> Self {
        Self {
            method: Method::Get,
            path,
            id,
            tag,
            summary,
        }
    }

    const fn post(
        path: &'static str,
        id: &'static str,
        tag: &'static str,
        summary: &'static str,
    ) -> Self {
        Self {
            method: Method::Post,
            ..Self::get(path, id, tag, summary)
        }
    }

    /// The route: `handle` behind `web`'s checks (a `POST` must also be
    /// JSON), documented with the error answers it can give.
    fn entry(&self, web: &Arc<WebPolicy>, handle: Handler) -> RouteEntry {
        let write = self.method == Method::Post;
        let web = Arc::clone(web);
        let route = Route::new(self.method, self.path)
            .operation_id(self.id)
            .summary(self.summary)
            .tag(self.tag);
        let entry = RouteEntry::from_route(route, move |_, req| {
            let response = match web.admit(req, write) {
                Ok(()) => handle(req),
                Err(refused) => refused.http_response(),
            };
            Box::pin(ready(response)) as BoxFuture<'_, Response>
        });
        error_answers(entry, write)
    }
}

/// A route's work, after the browser-safety checks passed.
type Handler = Box<dyn Fn(&mut Request) -> Response + Send + Sync>;

/// Document each status the API can fail with and the codes it carries
/// (only writes are refused for their media type).
fn error_answers(mut entry: RouteEntry, write: bool) -> RouteEntry {
    let mut by_status: BTreeMap<u16, Vec<&str>> = BTreeMap::new();
    for code in ErrorCode::ALL {
        if write || code != ErrorCode::UnsupportedMediaType {
            by_status
                .entry(code.status())
                .or_default()
                .push(code.as_str());
        }
    }
    for (status, codes) in by_status {
        let codes = codes.join(", ");
        entry = entry.response_schema::<ApiError>(status, format!("{codes} (docs/ERRORS.md)"));
    }
    entry
}

fn health() -> Response {
    let body = HealthDto {
        status: "ok".into(),
        version: env!("CARGO_PKG_VERSION").into(),
    };
    Response::json(&body).expect("HealthDto serializes")
}

fn json_text(text: &str) -> Response {
    Response::ok()
        .header("content-type", b"application/json".to_vec())
        .body(ResponseBody::Bytes(text.as_bytes().to_vec()))
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

/// `GET /favorites?zone=<room>`.
#[derive(JsonSchema)]
struct FavoritesQuery {
    /// Any room of the household.
    zone: String,
}

fn favorites_query(req: &Request) -> Result<FavoritesQuery, Failure> {
    let zone = query_param(req, "zone")?.ok_or_else(|| {
        Failure::invalid("name the room: GET /favorites?zone=<room>")
            .with_hint("Add ?zone=<room>; any room of the household will do.")
    })?;
    Ok(FavoritesQuery { zone })
}

/// `GET /library/search?q=<words>&zone=<room>&limit=<n>`.
#[derive(JsonSchema)]
struct SearchQuery {
    /// What to look for (see `SearchRequest::query`).
    q: String,
    /// A room: its household's favorites are searched too.
    zone: Option<String>,
    /// 1 to 50, default 10.
    limit: Option<usize>,
}

fn search_query(req: &Request) -> Result<SearchRequest, Failure> {
    let q = SearchQuery {
        q: query_param(req, "q")?.ok_or_else(|| {
            Failure::invalid("say what to look for: GET /library/search?q=<words>")
        })?,
        zone: query_param(req, "zone")?,
        limit: whole_number(req, "limit")?,
    };
    Ok(SearchRequest {
        query: q.q,
        zone: q.zone,
        limit: q.limit,
    })
}

/// `GET /history?zone=<room>&limit=<n>`: one zone's plays, or everyone's.
#[derive(JsonSchema)]
struct HistoryQuery {
    /// A room: only its group's plays.
    zone: Option<String>,
    /// At most this many, newest first (default 20, at most 200).
    limit: Option<usize>,
}

fn history_query(req: &Request) -> Result<HistoryQuery, Failure> {
    let limit = whole_number(req, "limit")?;
    if limit.is_some_and(|n| n == 0 || n > 200) {
        return Err(Failure::invalid("limit must be 1 to 200"));
    }
    Ok(HistoryQuery {
        zone: query_param(req, "zone")?,
        limit,
    })
}

/// A non-negative whole-number query parameter.
fn whole_number(req: &Request, name: &str) -> Result<Option<usize>, Failure> {
    query_param(req, name)?
        .map(|v| {
            v.parse::<usize>()
                .map_err(|_| Failure::invalid(format!("{name} must be a whole number, got {v:?}")))
        })
        .transpose()
}

/// `GET /events?since=<id>`: resume after event `since` (or the
/// `Last-Event-ID` header); by default only events from now on.
#[derive(JsonSchema)]
struct EventsQuery {
    /// The last event id the client has seen.
    since: Option<u64>,
}

fn events_query(req: &Request, bus: &crate::events::EventBus) -> Result<EventsQuery, Failure> {
    let header = req
        .headers()
        .get("last-event-id")
        .map(|v| String::from_utf8_lossy(v).trim().to_string());
    let since = match header.or(query_param(req, "since")?) {
        None => bus.last_id(),
        Some(raw) => raw.parse().map_err(|_| {
            Failure::invalid(format!(
                "since / Last-Event-ID must be an event id, got {raw:?}"
            ))
        })?,
    };
    Ok(EventsQuery { since: Some(since) })
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
#[must_use]
pub fn percent_decode(raw: &str) -> Option<String> {
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
