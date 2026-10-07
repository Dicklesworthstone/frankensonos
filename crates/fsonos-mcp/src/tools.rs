//! The control tools, and the [`Backend`] of speakers they act on.
//!
//! Every tool is a thin wrapper: authorize the call under the house policy,
//! plan the shared `fsonos-api` request against the current households, carry
//! it out (volume bounded for capped callers), and answer with the outcome's
//! sentence as text and its JSON as structured content. Failures answer as
//! tool errors (`CODE: detail. Hint: ...`, see `docs/ERRORS.md`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, ContentBlock, FinalCallToolResult, ResultMeta};
use fsonos_api::plan::{self, DjAction, TransportAction};
use fsonos_api::zones::zone_views;
use fsonos_api::{
    ErrorCode, Failure, GroupRequest, Guard, MuteRequest, PlayRequest, VolumeRequest, ZoneRequest,
    execute_guarded,
};
use fsonos_core::clock::Clock;
use fsonos_core::policy::{Client, Policy};
use fsonos_core::{HouseholdState, control};
use fsonos_proto::Transport;
use fsonos_types::TransportState;
use serde::Serialize;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::tool_error;

/// Finds the households (a LAN survey, say). Called again once the last
/// answer is older than the backend's refresh interval.
pub type Survey = Box<dyn Fn(&dyn Transport) -> Result<Vec<HouseholdState>, Failure> + Send + Sync>;

/// What the tools act on: the LAN, how to find the households, and the house
/// policy for this server's callers.
pub struct Backend {
    transport: Box<dyn Transport + Send + Sync>,
    survey: Survey,
    refresh: Duration,
    cache: Mutex<Option<(Instant, Vec<HouseholdState>)>>,
    policy: Policy,
    client: Client,
    clock: Box<dyn Clock>,
}

impl Backend {
    #[must_use]
    pub fn new(
        transport: Box<dyn Transport + Send + Sync>,
        survey: Survey,
        policy: Policy,
        client: Client,
        clock: Box<dyn Clock>,
    ) -> Self {
        Self {
            transport,
            survey,
            refresh: Duration::from_secs(30),
            cache: Mutex::new(None),
            policy,
            client,
            clock,
        }
    }

    fn guard(&self) -> Guard<'_> {
        Guard {
            policy: &self.policy,
            client: &self.client,
            clock: &*self.clock,
        }
    }

    /// The households, from the last survey while it is fresh.
    fn households(&self) -> Result<Vec<HouseholdState>, Failure> {
        let mut cache = self
            .cache
            .lock()
            .map_err(|_| Failure::new(ErrorCode::Internal, "household cache poisoned"))?;
        if let Some((at, households)) = cache.as_ref()
            && at.elapsed() < self.refresh
            && households.iter().any(|h| !h.rooms.is_empty())
        {
            return Ok(households.clone());
        }
        let households = (self.survey)(&*self.transport)?;
        *cache = Some((Instant::now(), households.clone()));
        Ok(households)
    }

    /// Run a control tool: authorize `tool`, plan, execute under the policy.
    pub fn control(
        &self,
        tool: &str,
        plan: impl FnOnce(&[HouseholdState]) -> Result<fsonos_api::Command, Failure>,
    ) -> McpResult<FinalCallToolResult> {
        let guard = self.guard();
        respond(|| {
            guard.authorize(tool, false)?;
            let households = self.households()?;
            let command = plan(&households)?;
            let outcome = execute_guarded(&*self.transport, &households, &guard, command)?;
            let text = std::iter::once(outcome.done.clone())
                .chain(outcome.notes.iter().map(|n| format!("Note: {}.", n.detail)))
                .collect::<Vec<_>>()
                .join(" ");
            Ok((text, outcome))
        })
    }

    /// The `list_zones` tool.
    pub fn list_zones(&self) -> McpResult<FinalCallToolResult> {
        respond(|| {
            self.guard().authorize("list_zones", true)?;
            let households = self.households()?;
            if households.iter().all(|h| h.rooms.is_empty()) {
                return Err(Failure::new(ErrorCode::NotReady, "no Sonos rooms answered"));
            }
            let zones = zone_views(&households, |coordinator| {
                control::playback(&*self.transport, &households, coordinator)
                    .map_or(TransportState::Unknown, |p| p.transport.state)
            });
            let text = zones
                .iter()
                .map(|z| {
                    format!(
                        "{} [{}]: {}",
                        z.members.join(" + "),
                        z.household,
                        z.transport_state
                    )
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok((text, ZonesDto { zones }))
        })
    }
}

/// `list_zones` structured content (MCP wants an object).
#[derive(Serialize)]
struct ZonesDto {
    zones: Vec<fsonos_api::ZoneDto>,
}

/// A tool result: `text` plus `value` as structured content, or the failure
/// as a tool error.
fn respond<T: Serialize>(
    run: impl FnOnce() -> Result<(String, T), Failure>,
) -> McpResult<FinalCallToolResult> {
    let (text, value) = run().map_err(|failure| tool_error(&failure))?;
    Ok(FinalCallToolResult {
        content: vec![ContentBlock::text(text)],
        is_error: false,
        structured_content: serde_json::to_value(value).ok(),
    })
}

static BACKEND: OnceLock<Backend> = OnceLock::new();

/// Install the process-wide backend the tools act on. `false` when one is
/// already installed (the new one is dropped).
#[must_use]
pub fn install(backend: Backend) -> bool {
    BACKEND.set(backend).is_ok()
}

fn with_backend(
    run: impl FnOnce(&Backend) -> McpResult<FinalCallToolResult>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let backend = BACKEND.get().ok_or_else(|| {
        tool_error(&Failure::new(
            ErrorCode::NotReady,
            "this MCP server has no speakers attached yet",
        ))
    })?;
    Ok(CompleteResult::new(run(backend)?, ResultMeta::empty()))
}

#[tool(
    description = "List the zone groups: which rooms play together, in which household (S1/S2), and whether each group is playing, paused or stopped.",
    annotations(read_only, idempotent)
)]
fn list_zones(_ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(Backend::list_zones)
}

#[tool(
    description = "Play something in the group a room plays in. `source_uri` is a spotify:track:<id> URI or open.spotify.com track link (rendered through the household's own Spotify link), or a radio/HTTP stream or Sonos favorite URI. Optional `title` is shown on the speaker. `zone` is a room name (case-insensitive; Room@S1 / Room@S2 picks a household)."
)]
fn play(
    _ctx: &McpContext,
    zone: String,
    source_uri: String,
    title: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(|b| {
        b.control("play", |h| {
            plan::plan_play(
                h,
                &PlayRequest {
                    zone,
                    source_uri,
                    title,
                },
            )
        })
    })
}

fn transport(
    zone: String,
    tool: &str,
    action: TransportAction,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(|b| {
        b.control(tool, |h| {
            plan::plan_transport(h, &ZoneRequest { zone }, action)
        })
    })
}

#[tool(
    description = "Pause the group a room plays in. `zone` is a room name (case-insensitive; Room@S1 / Room@S2 picks a household).",
    annotations(idempotent)
)]
fn pause(_ctx: &McpContext, zone: String) -> McpResult<CompleteResult<FinalCallToolResult>> {
    transport(zone, "pause", TransportAction::Pause)
}

#[tool(
    description = "Resume (or start) playback in the group a room plays in. `zone` is a room name (case-insensitive; Room@S1 / Room@S2 picks a household).",
    annotations(idempotent)
)]
fn resume(_ctx: &McpContext, zone: String) -> McpResult<CompleteResult<FinalCallToolResult>> {
    transport(zone, "resume", TransportAction::Resume)
}

#[tool(
    description = "Skip to the next track in the group a room plays in. `zone` is a room name (case-insensitive; Room@S1 / Room@S2 picks a household)."
)]
fn next(_ctx: &McpContext, zone: String) -> McpResult<CompleteResult<FinalCallToolResult>> {
    transport(zone, "next", TransportAction::Next)
}

#[tool(
    description = "Go back a track in the group a room plays in. `zone` is a room name (case-insensitive; Room@S1 / Room@S2 picks a household)."
)]
fn previous(_ctx: &McpContext, zone: String) -> McpResult<CompleteResult<FinalCallToolResult>> {
    transport(zone, "previous", TransportAction::Previous)
}

#[tool(
    description = "Change a room's volume: give exactly one of `volume` (0-100, absolute) or `delta` (-100..100, relative; e.g. +5 for 'a bit louder'). With `group` true the change applies to the room's whole group. The house policy may lower a loud request; the reply then says so. `zone` is a room name (case-insensitive; Room@S1 / Room@S2 picks a household)."
)]
fn set_volume(
    _ctx: &McpContext,
    zone: String,
    volume: Option<i64>,
    delta: Option<i64>,
    group: Option<bool>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(|b| {
        b.control("set_volume", |h| {
            plan::plan_volume(
                h,
                &VolumeRequest {
                    zone,
                    volume,
                    delta,
                    group: group.unwrap_or(false),
                },
            )
        })
    })
}

#[tool(
    name = "mute",
    description = "Mute a room, or unmute it with `mute` false (default true). `zone` is a room name (case-insensitive; Room@S1 / Room@S2 picks a household).",
    annotations(idempotent)
)]
fn mute_tool(
    _ctx: &McpContext,
    zone: String,
    mute: Option<bool>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(|b| {
        b.control("mute", |h| {
            plan::plan_mute(
                h,
                &MuteRequest {
                    zone,
                    mute: mute.unwrap_or(true),
                },
            )
        })
    })
}

#[tool(
    description = "Move the room `zone` into the group that the room `to` plays in, so they play the same thing in sync. Both must be in the same household. Room names are case-insensitive; Room@S1 / Room@S2 picks a household.",
    annotations(idempotent)
)]
fn group(
    _ctx: &McpContext,
    zone: String,
    to: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(|b| b.control("group", |h| plan::plan_group(h, &GroupRequest { zone, to })))
}

#[tool(
    description = "Take a room out of its group so it plays on its own. `zone` is a room name (case-insensitive; Room@S1 / Room@S2 picks a household).",
    annotations(idempotent)
)]
fn ungroup(_ctx: &McpContext, zone: String) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(|b| b.control("ungroup", |h| plan::plan_ungroup(h, &ZoneRequest { zone })))
}

fn dj(
    zone: String,
    tool: &str,
    action: DjAction,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(|b| b.control(tool, |h| plan::plan_dj(h, &ZoneRequest { zone }, action)))
}

#[tool(
    description = "Start the classical-music DJ in the group a room plays in. `zone` is a room name."
)]
fn dj_start(_ctx: &McpContext, zone: String) -> McpResult<CompleteResult<FinalCallToolResult>> {
    dj(zone, "dj_start", DjAction::Start)
}

#[tool(
    description = "Skip the DJ's current piece in the group a room plays in. `zone` is a room name."
)]
fn dj_skip(_ctx: &McpContext, zone: String) -> McpResult<CompleteResult<FinalCallToolResult>> {
    dj(zone, "dj_skip", DjAction::Skip)
}

#[tool(description = "Stop the DJ in the group a room plays in. `zone` is a room name.")]
fn dj_stop(_ctx: &McpContext, zone: String) -> McpResult<CompleteResult<FinalCallToolResult>> {
    dj(zone, "dj_stop", DjAction::Stop)
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::Room;
    use fsonos_core::clock::SystemClock;
    use fsonos_proto::ProtoError;
    use fsonos_types::{Generation, Player, PlayerId, ZoneGroup};
    use std::net::IpAddr;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Answers every SOAP action with success and `out_args`, and records the
    /// action names.
    #[derive(Default)]
    struct Canned {
        out_args: &'static str,
        sent: Arc<Mutex<Vec<String>>>,
    }

    impl Transport for Canned {
        fn soap_post(
            &self,
            _: IpAddr,
            _: &str,
            action: &str,
            _: &str,
        ) -> Result<String, ProtoError> {
            let action = action
                .trim_matches('"')
                .rsplit('#')
                .next()
                .unwrap_or_default()
                .to_string();
            self.sent.lock().unwrap().push(action.clone());
            Ok(format!(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                 <u:{action}Response xmlns:u=\"urn:x\">{}</u:{action}Response></s:Body></s:Envelope>",
                self.out_args
            ))
        }
    }

    /// One S2 household: `Den` coordinating `Kitchen`.
    fn house() -> Vec<HouseholdState> {
        let id = |s: &str| PlayerId(s.into());
        let player = |pid: &str, room: &str| Player {
            id: id(pid),
            room_name: room.into(),
            ip: "192.0.2.10".parse().unwrap(),
            model: String::new(),
            generation: Generation::S2,
        };
        let room = |name: &str, pid: &str| Room {
            name: name.into(),
            primary: id(pid),
            players: vec![id(pid)],
            missing: Vec::new(),
            coordinator: id("RINCON_DEN"),
        };
        vec![HouseholdState {
            players: vec![player("RINCON_DEN", "Den"), player("RINCON_KIT", "Kitchen")],
            groups: vec![ZoneGroup {
                coordinator: id("RINCON_DEN"),
                members: vec![id("RINCON_DEN"), id("RINCON_KIT")],
            }],
            rooms: vec![room("Den", "RINCON_DEN"), room("Kitchen", "RINCON_KIT")],
            ..Default::default()
        }]
    }

    fn backend(
        out_args: &'static str,
        client: Client,
    ) -> (Backend, Arc<Mutex<Vec<String>>>, Arc<AtomicUsize>) {
        let canned = Canned {
            out_args,
            ..Canned::default()
        };
        let sent = Arc::clone(&canned.sent);
        let surveys = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&surveys);
        let survey: Survey = Box::new(move |_| {
            counter.fetch_add(1, Ordering::SeqCst);
            Ok(house())
        });
        let b = Backend::new(
            Box::new(canned),
            survey,
            Policy::default(),
            client,
            Box::new(SystemClock),
        );
        (b, sent, surveys)
    }

    fn text(result: &FinalCallToolResult) -> String {
        serde_json::to_value(result).unwrap()["content"][0]["text"]
            .as_str()
            .unwrap()
            .to_string()
    }

    fn error_text(result: McpResult<FinalCallToolResult>) -> String {
        match result {
            Ok(r) => panic!("expected a tool error, got {}", text(&r)),
            Err(e) => e.message,
        }
    }

    #[test]
    fn control_tools_plan_execute_and_report() {
        let (b, sent, surveys) = backend("", Client::McpStdio);
        let r = b
            .control("pause", |h| {
                plan::plan_transport(
                    h,
                    &ZoneRequest {
                        zone: "kitchen".into(),
                    },
                    TransportAction::Pause,
                )
            })
            .unwrap();
        assert!(!r.is_error, "{}", text(&r));
        assert_eq!(text(&r), "paused Den's group");
        assert_eq!(r.structured_content.as_ref().unwrap()["changed"], true);
        assert_eq!(*sent.lock().unwrap(), ["Pause"]);
        // A second call within the refresh interval reuses the survey.
        b.control("resume", |h| {
            plan::plan_transport(
                h,
                &ZoneRequest { zone: "Den".into() },
                TransportAction::Resume,
            )
        })
        .unwrap();
        assert_eq!(surveys.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn agents_get_clamped_volume_with_a_note() {
        let (b, sent, _) = backend("<CurrentVolume>60</CurrentVolume>", Client::McpStdio);
        let r = b
            .control("set_volume", |h| {
                plan::plan_volume(
                    h,
                    &VolumeRequest {
                        zone: "Kitchen".into(),
                        volume: Some(100),
                        delta: None,
                        group: false,
                    },
                )
            })
            .unwrap();
        assert!(!r.is_error, "{}", text(&r));
        assert!(
            text(&r).starts_with("Kitchen volume is 70 Note: volume 100 lowered to 70"),
            "{}",
            text(&r)
        );
        let structured = r.structured_content.unwrap();
        assert_eq!(structured["notes"][0]["code"], "VOLUME_CLAMPED");
        assert_eq!(*sent.lock().unwrap(), ["GetVolume", "SetVolume"]);
    }

    #[test]
    fn failures_are_tool_errors_with_codes() {
        let (b, sent, _) = backend("", Client::McpStdio);
        let message = error_text(b.control("pause", |h| {
            plan::plan_transport(
                h,
                &ZoneRequest {
                    zone: "Kitchn".into(),
                },
                TransportAction::Pause,
            )
        }));
        assert!(message.starts_with("UNKNOWN_ROOM: "), "{message}");
        assert!(message.ends_with("Did you mean: Kitchen@S2?"), "{message}");
        assert_eq!(*sent.lock().unwrap(), Vec::<String>::new());
    }

    #[test]
    fn unknown_callers_cannot_control() {
        let (b, sent, surveys) = backend("", Client::Unknown);
        let message = error_text(b.control("pause", |_| unreachable!("denied before planning")));
        assert!(
            message.starts_with("POLICY_DENIED: unknown may not use pause"),
            "{message}"
        );
        assert_eq!(
            (sent.lock().unwrap().len(), surveys.load(Ordering::SeqCst)),
            (0, 0)
        );
    }

    #[test]
    fn list_zones_reports_groups_and_state() {
        let (b, _, _) = backend(
            "<CurrentTransportState>PLAYING</CurrentTransportState>",
            Client::McpStdio,
        );
        let r = b.list_zones().unwrap();
        assert!(!r.is_error, "{}", text(&r));
        assert_eq!(text(&r), "Den + Kitchen [S2]: playing");
        let zones = &r.structured_content.unwrap()["zones"];
        assert_eq!(zones[0]["members"], serde_json::json!(["Den", "Kitchen"]));
    }

    #[test]
    fn without_a_backend_tools_say_not_ready() {
        // The process-wide backend is never installed in unit tests.
        let Err(err) = with_backend(|_| unreachable!()) else {
            panic!("expected NOT_READY")
        };
        assert!(err.message.starts_with("NOT_READY: "), "{}", err.message);
    }
}
