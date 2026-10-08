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
use fsonos_api::{
    ActionDto, ActionsQuery, DjStartRequest, DjSteerRequest, ErrorCode, Failure, GroupRequest,
    MoveRequest, MuteRequest, PartyRequest, PlayFavoriteRequest, PlayRequest, SearchRequest,
    SteerConstraints, Surface, UndoDto, VolumeRequest, ZoneRequest,
};
use fsonos_core::clock::Clock;
use fsonos_core::policy::{Client, Policy};
use fsonos_proto::Transport;
use serde::Serialize;
use std::sync::{Arc, OnceLock};

use crate::tool_error;

pub mod dj_feedback;
pub mod schedules;

mod house_policy;
pub use house_policy::GetPolicy;

mod scenes;
pub use scenes::{ApplyScene, ListScenes, SaveScene};

mod announce;
pub use announce::Announce;

mod documents;
/// Finds the households (a LAN survey, say); see [`fsonos_api::surface`].
pub use fsonos_api::surface::Survey;

/// What the tools act on: the shared [`Surface`] (LAN, households, house
/// policy) and the identity this server's callers have under the policy.
pub struct Backend {
    surface: Arc<Surface>,
    client: Client,
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
        Self::shared(
            Arc::new(Surface::new(transport, survey, policy, clock)),
            client,
        )
    }

    /// Act on a surface the process shares with other servers (the daemon's
    /// HTTP API, say), as `client`.
    #[must_use]
    pub fn shared(surface: Arc<Surface>, client: Client) -> Self {
        Self { surface, client }
    }

    /// Run a control tool: authorize `tool`, plan, execute under the policy.
    pub fn control(
        &self,
        tool: &str,
        plan: impl FnOnce(&fsonos_api::plan::Rooms<'_>) -> Result<fsonos_api::Command, Failure>,
    ) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let outcome = self.surface.control(&self.client, tool, plan)?;
            let text = std::iter::once(outcome.done.clone())
                .chain(outcome.notes.iter().map(|n| format!("Note: {}.", n.detail)))
                .collect::<Vec<_>>()
                .join(" ");
            Ok((text, outcome))
        })
    }

    /// The `dj_status` tool.
    pub fn dj_status(&self, zone: &str) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let status = self.surface.dj_status(&self.client, zone)?;
            Ok((status.summary(), status))
        })
    }

    /// The `dj_moods` tool.
    pub fn dj_moods(&self, zone: Option<&str>) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let moods = self.surface.dj_moods(&self.client, zone)?;
            Ok((moods.summary(), moods))
        })
    }

    /// The `get_zone_state` tool.
    pub fn zone_state(&self, zone: &str) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let state = self.surface.zone_state(&self.client, zone)?;
            let on = state.track.as_ref().map_or_else(
                || "nothing".to_string(),
                |t| {
                    let title = t.title.clone().unwrap_or_else(|| t.uri.clone());
                    t.creator
                        .as_ref()
                        .map_or_else(|| title.clone(), |c| format!("{title} by {c}"))
                },
            );
            let volume = state
                .volume
                .map_or_else(String::new, |v| format!(", volume {v}"));
            let text = format!(
                "{} [{}]: {} ({on}){volume}",
                state.zone.members.join(" + "),
                state.zone.household,
                state.transport_state
            );
            Ok((text, state))
        })
    }

    /// The `list_favorites` tool.
    pub fn list_favorites(&self, zone: &str) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let favorites = self.surface.favorites(&self.client, zone)?;
            let text = favorites
                .iter()
                .enumerate()
                .map(|(i, f)| format!("{}. {} ({})", i + 1, f.title, f.kind))
                .collect::<Vec<_>>()
                .join("\n");
            Ok((text, FavoritesDto { favorites }))
        })
    }

    /// The `play_favorite` tool.
    pub fn play_favorite(&self, zone: String, favorite: String) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let outcome = self
                .surface
                .play_favorite(&self.client, &PlayFavoriteRequest { zone, favorite })?;
            Ok((outcome.done.clone(), outcome))
        })
    }

    /// The `search_library` tool.
    pub fn search_library(&self, req: &SearchRequest) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let hits = self.surface.search_library(&self.client, req)?;
            let text = if hits.is_empty() {
                format!("nothing matches {:?}", req.query.trim())
            } else {
                hits.iter()
                    .enumerate()
                    .map(|(i, h)| {
                        let by = h
                            .subtitle
                            .as_deref()
                            .map_or_else(String::new, |s| format!(" ({s})"));
                        let how = match (&h.source_uri, &h.favorite) {
                            (Some(uri), _) => format!("play source_uri={uri}"),
                            (None, Some(id)) => format!("play_favorite favorite={id}"),
                            (None, None) => String::new(),
                        };
                        format!("{}. {}{by}: {how}", i + 1, h.title)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok((text, HitsDto { hits }))
        })
    }

    /// The `recent_plays` tool.
    pub fn recent_plays(&self, zone: Option<&str>, limit: usize) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let plays = self.surface.recent_plays(&self.client, zone, limit)?;
            let text = if plays.is_empty() {
                "no plays recorded".to_string()
            } else {
                plays
                    .iter()
                    .map(|p| {
                        let room = p.room.as_deref().unwrap_or("a room no longer seen");
                        format!("{} in {room} at {}", p.source_uri, p.played_at)
                    })
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok((text, PlaysDto { plays }))
        })
    }

    /// `sonos://zones`: every zone, as `list_zones` returns them.
    pub fn zones_document(&self) -> Result<String, Failure> {
        let zones = self.surface.zones(&self.client)?;
        json_document(&ZonesDto { zones })
    }

    /// `sonos://zones/{room}`: the room's zone state, as `get_zone_state`.
    pub fn zone_document(&self, room: &str) -> Result<String, Failure> {
        json_document(&self.surface.zone_state(&self.client, room)?)
    }

    /// The `recent_actions` tool.
    pub fn recent_actions(&self, query: &ActionsQuery) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let actions: Vec<ActionDto> = self
                .surface
                .recent_actions(&self.client, &query.filter())?
                .iter()
                .map(ActionDto::from)
                .collect();
            let text = if actions.is_empty() {
                "no actions logged".to_string()
            } else {
                actions
                    .iter()
                    .map(|a| format!("#{} [{}] {} -> {}", a.id, a.client, a.intent, a.result))
                    .collect::<Vec<_>>()
                    .join("\n")
            };
            Ok((text, ActionsDto { actions }))
        })
    }

    /// The `undo_last` tool: undo this caller's own newest action.
    pub fn undo_last(&self) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let undone = UndoDto::from(self.surface.undo(&self.client, true)?);
            Ok((undone.summary.clone(), undone))
        })
    }

    /// The `doctor` tool.
    pub fn doctor(&self) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let report = self.surface.doctor(&self.client)?;
            let counts = report.counts();
            let mut lines = vec![format!(
                "{} pass, {} warn, {} fail, {} skipped",
                counts.pass, counts.warn, counts.fail, counts.skip
            )];
            for entry in &report.entries {
                let status = serde_json::to_value(entry.result.status)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_uppercase))
                    .unwrap_or_default();
                let mut line = format!("{status} {}: {}", entry.id, entry.result.summary);
                if let Some(remedy) = &entry.result.remedy {
                    line.push_str(" -- ");
                    line.push_str(remedy);
                }
                lines.push(line);
            }
            Ok((lines.join("\n"), report.to_json()))
        })
    }

    /// The `list_zones` tool.
    /// The `list_rooms` tool.
    pub fn list_rooms(&self) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let rooms = self.surface.rooms(&self.client)?;
            let text = rooms
                .iter()
                .map(|r| {
                    let aliases = if r.aliases.is_empty() {
                        String::new()
                    } else {
                        format!(", aliases: {}", r.aliases.join(", "))
                    };
                    format!("{} [{}] in {}'s zone{aliases}", r.name, r.household, r.zone)
                })
                .collect::<Vec<_>>()
                .join("\n");
            Ok((text, RoomsDto { rooms }))
        })
    }

    pub fn list_zones(&self) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let zones = self.surface.zones(&self.client)?;
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

/// `recent_actions` structured content.
#[derive(Serialize)]
struct ActionsDto {
    actions: Vec<ActionDto>,
}

/// `list_rooms` structured content.
#[derive(Serialize)]
struct RoomsDto {
    rooms: Vec<fsonos_api::RoomDto>,
}

/// `search_library` structured content.
#[derive(Serialize)]
struct HitsDto {
    hits: Vec<fsonos_api::HitDto>,
}

/// `recent_plays` structured content.
#[derive(Serialize)]
struct PlaysDto {
    plays: Vec<fsonos_api::PlayDto>,
}

fn json_document<T: Serialize>(value: &T) -> Result<String, Failure> {
    serde_json::to_string_pretty(value)
        .map_err(|e| Failure::new(ErrorCode::Internal, e.to_string()))
}

/// `list_favorites` structured content.
#[derive(Serialize)]
struct FavoritesDto {
    favorites: Vec<fsonos_api::FavoriteDto>,
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

/// The installed backend, or `NOT_READY` before one is.
pub(crate) fn backend() -> Result<&'static Backend, Failure> {
    BACKEND.get().ok_or_else(|| {
        Failure::new(
            ErrorCode::NotReady,
            "this MCP server has no speakers attached yet",
        )
    })
}

fn with_backend(
    run: impl FnOnce(&Backend) -> McpResult<FinalCallToolResult>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let backend = backend().map_err(|f| tool_error(&f))?;
    Ok(CompleteResult::new(run(backend)?, ResultMeta::empty()))
}

#[tool(
    description = "Search the owner's saved Spotify library for music, and with `zone` also that room's household Sonos favorites. `query` is title words, a composer or performer, an album, or a catalog number ('goldberg gould', 'bwv 988', 'dvorak 9', 'op 67'); case, accents and small typos are forgiven. Results come best first: play a track with `play` (zone, source_uri) or a favorite with `play_favorite` (zone, favorite). `limit` 1-50, default 10.",
    annotations(read_only, idempotent)
)]
fn search_library(
    _ctx: &McpContext,
    query: String,
    zone: Option<String>,
    limit: Option<u32>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = SearchRequest {
        query,
        zone,
        limit: limit.map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
    };
    with_backend(move |b| b.search_library(&req))
}

#[tool(
    description = "What played recently, newest first: each play's URI, the room whose group played it, and when (unix seconds). Plays started through fsonos and by the DJ are recorded. Optional `zone` (a room) narrows it to that group; `limit` default 20, at most 200.",
    annotations(read_only, idempotent)
)]
fn recent_plays(
    _ctx: &McpContext,
    zone: Option<String>,
    limit: Option<u32>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let limit = limit.map_or(20, |n| {
        usize::try_from(n).unwrap_or(usize::MAX).clamp(1, 200)
    });
    with_backend(move |b| b.recent_plays(zone.as_deref(), limit))
}

#[tool(
    description = "List every room: its household (S1/S2), the zone (group) it plays in, and the owner's aliases for it. Any tool's `zone` accepts a room name, Room@S1 / Room@S2, or one of these aliases.",
    annotations(read_only, idempotent)
)]
fn list_rooms(_ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(Backend::list_rooms)
}

#[tool(
    description = "List the zone groups: which rooms play together, in which household (S1/S2), and whether each group is playing, paused or stopped.",
    annotations(read_only, idempotent)
)]
fn list_zones(_ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(Backend::list_zones)
}

#[tool(
    description = "What a room is doing right now: its group, whether that group is playing, paused or stopped, the current track or station (title, artist, position), and the room's volume. Call this before changing anything relative ('a bit louder', 'what's playing?'). `zone` is a room name (case-insensitive; Room@S1 / Room@S2 picks a household).",
    annotations(read_only, idempotent)
)]
fn get_zone_state(
    _ctx: &McpContext,
    zone: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(move |b| b.zone_state(&zone))
}

#[tool(
    description = "List the Sonos favorites of the household a room belongs to (tracks, stations, albums and playlists the owner saved in the Sonos app), numbered. Play one with play_favorite. `zone` is any room of that household.",
    annotations(read_only, idempotent)
)]
fn list_favorites(
    _ctx: &McpContext,
    zone: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(move |b| b.list_favorites(&zone))
}

#[tool(
    description = "Play one of the household's Sonos favorites in the group a room plays in. `favorite` is its title (case, accents and punctuation ignored; a unique prefix or all its words in any order will do), its number from list_favorites, or its FV:2/<n> id. Albums and playlists replace the queue. `zone` is a room name."
)]
fn play_favorite(
    _ctx: &McpContext,
    zone: String,
    favorite: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(|b| b.play_favorite(zone, favorite))
}

#[tool(
    description = "Diagnose the setup: can the daemon reach the speakers, is Spotify linked in each household, are the listeners bound safely. Each check says pass, warn or fail, what was seen, and what to do about it. Read-only; nothing changes.",
    annotations(read_only, idempotent)
)]
fn doctor(_ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(Backend::doctor)
}

#[tool(
    description = "The house's recent actions, newest first: who asked (client), what (intent), the policy decision (allow / clamp / deny) and what happened, and whether each can be undone. Optional `limit` (default 20), `since` (unix seconds) and `client` filter.",
    annotations(read_only, idempotent)
)]
fn recent_actions(
    _ctx: &McpContext,
    limit: Option<u32>,
    since: Option<i64>,
    client: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let query = ActionsQuery {
        client,
        since,
        limit: limit.map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
    };
    with_backend(move |b| b.recent_actions(&query))
}

#[tool(
    description = "Undo your own most recent action: restore the volumes, grouping and what was playing in the zones it changed. The reply says what could not be restored."
)]
fn undo_last(_ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(Backend::undo_last)
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

#[tool(
    description = "Move the music a room plays to another room, as if it followed you: in one household the group is handed over (or regrouped) without a gap; between households (S1 and S2 never group) set `copy` true to replay the same track, at the same position, there and stop it here. `zone` is the room the music is in, `to` the room it goes to (names case-insensitive; Room@S1 / Room@S2 picks a household; aliases work)."
)]
fn move_playback(
    _ctx: &McpContext,
    zone: String,
    to: String,
    copy: Option<bool>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = MoveRequest {
        zone,
        to,
        copy: copy.unwrap_or(false),
    };
    with_backend(|b| b.control("move_playback", |h| plan::plan_move(h, &req)))
}

#[tool(
    description = "Party mode: group every room of a household into one zone. Give `zone` (the room whose group the others join), or `household` (S1, S2: the group playing now leads, else the first room); with a single household, neither.",
    annotations(idempotent)
)]
fn group_all(
    _ctx: &McpContext,
    zone: Option<String>,
    household: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = PartyRequest { zone, household };
    with_backend(|b| b.control("group_all", |h| plan::plan_party(h, &req)))
}

fn dj(
    zone: String,
    tool: &str,
    action: DjAction,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(|b| b.control(tool, |h| plan::plan_dj(h, &ZoneRequest { zone }, action)))
}

#[tool(
    description = "Start the classical-music DJ in the group a room plays in: it queues whole works (every movement, in order) and keeps the queue topped up. `zone` is a room name. Optional `mood` starts it steered (dj_moods lists the moods), for `for_minutes` (omit: until cleared)."
)]
fn dj_start(
    _ctx: &McpContext,
    zone: String,
    mood: Option<String>,
    for_minutes: Option<u32>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = DjStartRequest {
        zone,
        mood,
        for_secs: for_minutes.map(|m| u64::from(m) * 60),
    };
    with_backend(|b| b.control("dj_start", |h| plan::plan_dj_start(h, &req)))
}

#[tool(
    description = "Steer the classical DJ in the group a room plays in. Turn the owner's words into these fields; the daemon never parses language. `mood`: a preset (dj_moods lists them: focus, dinner, sunday-morning, bright, calm, and the owner's own). `include_composers` / `exclude_composers`: names (\"Bach\", \"J.S. Bach\", \"Saint-Saëns\"). `periods`: medieval, renaissance, baroque, classical, romantic, late_romantic, impressionist, modern, contemporary. `include_keywords` / `exclude_keywords`: piano, chamber, orchestral, choral, opera, song and vocal find their forms; other words match whole words. `min_minutes` / `max_minutes`: work length. `energy_bias`: -2 (much calmer) to 2 (much brighter). `allow_long`: let operas and Passions in. `for_minutes`: how long, then the time-of-day program returns (omit: until cleared). Examples: 'something calmer for an hour' is energy_bias -1, for_minutes 60; 'no opera' is exclude_keywords [\"opera\"]; 'just piano' is include_keywords [\"piano\"]; 'dinner music' is mood \"dinner\"; 'Bach for the next two hours' is include_composers [\"Bach\"], for_minutes 120. A steer replaces the group's previous steering and applies from the DJ's next piece, whether or not it is running. `clear` true, alone, goes back to the time-of-day program. undo_last puts the previous steering back."
)]
#[allow(clippy::too_many_arguments)] // one optional argument per steering field, as agents call it
fn dj_steer(
    _ctx: &McpContext,
    zone: String,
    mood: Option<String>,
    include_composers: Option<Vec<String>>,
    exclude_composers: Option<Vec<String>>,
    periods: Option<Vec<String>>,
    include_keywords: Option<Vec<String>>,
    exclude_keywords: Option<Vec<String>>,
    min_minutes: Option<u32>,
    max_minutes: Option<u32>,
    energy_bias: Option<i8>,
    allow_long: Option<bool>,
    for_minutes: Option<u32>,
    clear: Option<bool>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = DjSteerRequest {
        zone,
        mood,
        constraints: SteerConstraints {
            include_composers: include_composers.unwrap_or_default(),
            exclude_composers: exclude_composers.unwrap_or_default(),
            periods: periods.unwrap_or_default(),
            include_keywords: include_keywords.unwrap_or_default(),
            exclude_keywords: exclude_keywords.unwrap_or_default(),
            min_work_minutes: min_minutes,
            max_work_minutes: max_minutes,
            energy_bias: energy_bias.unwrap_or(0),
            allow_long_works: allow_long.unwrap_or(false),
        },
        for_secs: for_minutes.map(|m| u64::from(m) * 60),
        clear: clear.unwrap_or(false),
    };
    with_backend(|b| b.control("dj_steer", |h| plan::plan_dj_steer(h, &req)))
}

#[tool(
    description = "What the classical DJ plays in the group a room plays in, and why: the work and movement (i of n), composer, performers, album, the reason it was chosen (a summary and the weighted factors), the next two works, and the steering in effect (from dj_steer, or the time-of-day program: mood, constraints, how long it lasts). `zone` is a room name.",
    annotations(read_only, idempotent)
)]
fn dj_status(_ctx: &McpContext, zone: String) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(move |b| b.dj_status(&zone))
}

#[tool(
    description = "The DJ's moods (built-in and the owner's moods.toml) with their constraints, the time-of-day programs (which mood plays when a zone isn't steered), and the steering in effect now: in `zone`'s group when a room is given, else the house's program. Use a mood's name with dj_steer or dj_start.",
    annotations(read_only, idempotent)
)]
fn dj_moods(
    _ctx: &McpContext,
    zone: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(move |b| b.dj_moods(zone.as_deref()))
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
    use fsonos_core::HouseholdState;
    use fsonos_core::Room;
    use fsonos_core::clock::SystemClock;
    use fsonos_proto::ProtoError;
    use fsonos_types::{Generation, Player, PlayerId, ZoneGroup};
    use std::net::IpAddr;
    use std::sync::Mutex;
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
