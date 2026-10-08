//! Direct mode: the CLI finds the speakers itself (SSDP plus any seeds) and
//! acts through the same [`Surface`] the daemon's HTTP API and MCP server
//! use, as the house policy's `cli` client.

use fsonos_api::{
    ActionDto, ActionsQuery, Command, DjMoodsDto, DjStatusDto, ErrorCode, Failure, FavoriteDto,
    HitDto, OutcomeDto, PlayFavoriteRequest, RoomDto, SearchRequest, Surface, UndoDto, ZoneDto,
    ZoneStateDto,
};
use fsonos_core::HouseholdState;
use fsonos_core::clock::SystemClock;
use fsonos_core::inventory::{self, Survey};
use fsonos_core::policy::{Client, Policy};
use fsonos_core::rooms::household_labels;
use fsonos_core::store::{SqliteStore, Store as _};
use fsonos_types::Generation;
use serde::Serialize;
use std::fmt::Write as _;
use std::sync::Mutex;

use crate::config::GlobalArgs;

/// A surveyed LAN, ready to take commands.
pub struct Direct {
    surface: Surface,
    survey: Survey,
}

/// One player, as `fsonos discover` lists it.
#[derive(Debug, Serialize)]
pub struct PlayerDto {
    pub room: String,
    pub id: String,
    pub model: String,
    pub generation: Generation,
    pub ip: String,
    pub household: String,
}

/// `fsonos discover --json`.
#[derive(Debug, Serialize)]
pub struct DiscoverDto {
    pub players: Vec<PlayerDto>,
    /// Addresses that answered discovery but could not be read, with why.
    pub unreachable: Vec<(String, String)>,
    pub ssdp_error: Option<String>,
}

impl Direct {
    /// Survey the LAN (SSDP for `global.wait`, plus the seeds) and load the
    /// house policy from the data directory (defaults when there is none).
    pub fn survey(global: &GlobalArgs) -> Result<Self, Failure> {
        Self::open(global, None)
    }

    /// [`Self::survey`], with `doctor_checks` added to the doctor's report.
    pub fn open(
        global: &GlobalArgs,
        doctor_checks: Option<fsonos_api::surface::DoctorChecks>,
    ) -> Result<Self, Failure> {
        let seeds = global.seed_addrs()?;
        let wait = global.wait();
        let lan = global.lan()?;
        let survey = inventory::survey(&*lan, &seeds, wait)?;
        if let Some(dir) = global.data_dir() {
            crate::completions::remember_rooms(
                &dir,
                survey
                    .households
                    .iter()
                    .flat_map(|h| h.rooms.iter().map(|r| r.name.clone())),
            );
        }
        let policy = match global.data_dir() {
            Some(dir) => crate::daemon::policy(&dir)?,
            None => Policy::default(),
        };
        // The surface starts from this survey and only surveys again if it
        // has to (after a regroup, say).
        let first = Mutex::new(Some(survey.households.clone()));
        let again: fsonos_api::surface::Survey = Box::new(move |transport| {
            if let Some(households) = first.lock().ok().and_then(|mut f| f.take()) {
                return Ok(households);
            }
            Ok(inventory::survey(transport, &seeds, wait)?.households)
        });
        let mut surface =
            Surface::new(lan, again, policy, Box::new(SystemClock)).with_dj(Box::new(
                crate::dj::SpotifyDj::new(global.data_dir().map(|d| d.join(crate::dj::MOODS_FILE))),
            ));
        if let Some(dir) = global.data_dir() {
            surface = crate::daemon::with_action_log(surface, &dir, "cli");
        }
        if let Some(checks) = doctor_checks {
            surface = surface.with_doctor_checks(checks);
        }
        Ok(Self { surface, survey })
    }

    /// The surveyed households, or `NOT_READY` with what the survey saw when
    /// no room answered.
    pub fn households(&self) -> Result<&[HouseholdState], Failure> {
        let households = &self.survey.households;
        if households.iter().any(|h| !h.rooms.is_empty()) {
            return Ok(households);
        }
        let mut parts = vec!["no Sonos rooms answered".to_string()];
        if let Some(err) = &self.survey.ssdp_error {
            parts.push(format!("SSDP failed: {err}"));
        }
        if !self.survey.unreachable.is_empty() {
            parts.push(format!(
                "{} player(s) found but unreadable",
                self.survey.unreachable.len()
            ));
        }
        let detail = parts.join("; ");
        Err(Failure::new(ErrorCode::NotReady, detail).with_hint(
            "Run fsonos on the speaker LAN, or name players with --seed <ip> or FSONOS_SEEDS; \
             under launchd, macOS may need Local Network permission (docs/DEPLOY.md).",
        ))
    }

    /// Plan a command as `tool` and carry it out under the house policy.
    pub fn run(
        &self,
        tool: &str,
        plan: impl FnOnce(&fsonos_api::plan::Rooms<'_>) -> Result<Command, Failure>,
    ) -> Result<OutcomeDto, Failure> {
        self.households()?;
        self.surface.control(&Client::Cli, tool, plan)
    }

    /// Announce `req` as the CLI, the players fetching the clip from
    /// `announcements`' listener.
    pub fn announce(
        self,
        announcements: fsonos_api::surface::announce::Announcements,
        req: &fsonos_api::surface::announce::AnnounceRequest,
    ) -> Result<fsonos_api::surface::announce::AnnounceDto, Failure> {
        self.households()?;
        self.surface
            .with_announcements(announcements)
            .announce(&Client::Cli, req)
    }

    /// Every zone with its live transport state.
    pub fn zones(&self) -> Result<Vec<ZoneDto>, Failure> {
        self.households()?;
        self.surface.zones(&Client::Cli)
    }

    /// What `zone` is doing right now.
    pub fn status(&self, zone: &str) -> Result<ZoneStateDto, Failure> {
        self.households()?;
        self.surface.zone_state(&Client::Cli, zone)
    }

    /// The DJ in `zone`'s group (`dj_status`). The DJ runs in the daemon,
    /// so from here this shows the zone's steering, not a running DJ.
    pub fn dj_status(&self, zone: &str) -> Result<DjStatusDto, Failure> {
        self.households()?;
        self.surface.dj_status(&Client::Cli, zone)
    }

    /// The DJ's moods and programs, and the steering now (`dj_moods`).
    pub fn dj_moods(&self, zone: Option<&str>) -> Result<DjMoodsDto, Failure> {
        if zone.is_some() {
            self.households()?;
        }
        self.surface.dj_moods(&Client::Cli, zone)
    }

    /// The library (and `req.zone`'s favorites) searched for `req.query`.
    pub fn search(&self, req: &SearchRequest) -> Result<Vec<HitDto>, Failure> {
        if req.zone.is_some() {
            self.households()?;
        }
        self.surface.search_library(&Client::Cli, req)
    }

    /// Every room with its household, zone and aliases.
    pub fn rooms(&self) -> Result<Vec<RoomDto>, Failure> {
        self.households()?;
        self.surface.rooms(&Client::Cli)
    }

    /// The favorites of `zone`'s household.
    pub fn favorites(&self, zone: &str) -> Result<Vec<FavoriteDto>, Failure> {
        self.households()?;
        self.surface.favorites(&Client::Cli, zone)
    }

    /// The doctor's report, even when nothing answered.
    pub fn doctor(&self, client: &Client) -> Result<fsonos_core::doctor::Report, Failure> {
        self.surface.doctor(client)
    }

    /// Undo the newest logged action (only the CLI's own with `own_only`).
    pub fn undo(&self, own_only: bool) -> Result<UndoDto, Failure> {
        self.households()?;
        Ok(UndoDto::from(self.surface.undo(&Client::Cli, own_only)?))
    }

    /// Play a favorite of `req.zone`'s household.
    pub fn play_favorite(&self, req: &PlayFavoriteRequest) -> Result<OutcomeDto, Failure> {
        self.households()?;
        self.surface.play_favorite(&Client::Cli, req)
    }

    /// Every player found, by household then room.
    #[must_use]
    pub fn discover(&self) -> DiscoverDto {
        let households = &self.survey.households;
        let labels = household_labels(households);
        let mut players: Vec<PlayerDto> = households
            .iter()
            .zip(&labels)
            .flat_map(|(h, label)| {
                h.players.iter().map(move |p| PlayerDto {
                    room: p.room_name.clone(),
                    id: p.id.0.clone(),
                    model: p.model.clone(),
                    generation: p.generation,
                    ip: p.ip.to_string(),
                    household: label.clone(),
                })
            })
            .collect();
        players.sort_by(|a, b| (&a.household, &a.room).cmp(&(&b.household, &b.room)));
        DiscoverDto {
            players,
            unreachable: self.survey.unreachable.clone(),
            ssdp_error: self.survey.ssdp_error.clone(),
        }
    }
}

/// `fsonos discover` as text: one player per line.
#[must_use]
pub fn discover_text(d: &DiscoverDto) -> String {
    if d.players.is_empty() {
        return "no players found\n".to_string();
    }
    d.players.iter().fold(String::new(), |mut out, p| {
        let generation = match p.generation {
            Generation::S1 => "S1",
            Generation::S2 => "S2",
        };
        // Writing to a String cannot fail.
        let _ = writeln!(
            out,
            "{:<24} {generation}  {:<18} {:<15} {}",
            p.room, p.model, p.ip, p.household
        );
        out
    })
}

/// `fsonos zones` as text: one group per line.
#[must_use]
pub fn zones_text(zones: &[ZoneDto]) -> String {
    zones.iter().fold(String::new(), |mut out, z| {
        let _ = write!(
            out,
            "{:<4} {:<40} {}",
            z.household,
            z.members.join(" + "),
            z.transport_state
        );
        if !z.degraded.is_empty() {
            let _ = write!(out, "  (degraded: {})", z.degraded.join(", "));
        }
        out.push('\n');
        out
    })
}

/// The action log in the data directory, newest first (no LAN needed).
pub fn actions(global: &GlobalArgs, query: &ActionsQuery) -> Result<Vec<ActionDto>, Failure> {
    let dir = crate::daemon::data_dir(global)?;
    let path = dir.join(crate::daemon::DB_FILE);
    if !path.exists() {
        return Ok(Vec::new());
    }
    let store = SqliteStore::open(&path).map_err(|e| {
        Failure::new(
            ErrorCode::Internal,
            format!("cannot open {}: {e}", path.display()),
        )
    })?;
    let listed = store
        .recent_actions(&query.filter())
        .map_err(|e| Failure::new(ErrorCode::Internal, e.to_string()))?;
    Ok(listed.iter().map(ActionDto::from).collect())
}

/// `fsonos log` as text: one action per line, newest first.
#[must_use]
pub fn actions_text(actions: &[ActionDto]) -> String {
    if actions.is_empty() {
        return "no actions logged\n".to_string();
    }
    actions.iter().fold(String::new(), |mut out, a| {
        let undo = if a.undoable { "" } else { "  (not undoable)" };
        let _ = writeln!(
            out,
            "#{:<5} {:<12} {:<10} {} -> {} [{}]{undo}",
            a.id, a.client, a.surface, a.intent, a.result, a.decision
        );
        out
    })
}

/// `fsonos status` as text.
#[must_use]
pub fn status_text(state: &ZoneStateDto) -> String {
    let mut out = format!(
        "{} [{}]: {}",
        state.zone.members.join(" + "),
        state.zone.household,
        state.transport_state
    );
    if let Some(track) = &state.track {
        let title = track.title.as_deref().unwrap_or(&track.uri);
        let _ = write!(out, "\n  {title}");
        if let Some(by) = &track.creator {
            let _ = write!(out, " by {by}");
        }
        if let Some(n) = track.queue_position {
            let _ = write!(out, " (queue #{n})");
        }
    }
    if let Some(volume) = state.volume {
        let _ = write!(out, "\n  volume {volume}");
    }
    out.push('\n');
    out
}

/// One line per room: its household, its zone, and its aliases.
#[must_use]
pub fn rooms_text(rooms: &[RoomDto]) -> String {
    rooms.iter().fold(String::new(), |mut out, r| {
        let _ = write!(out, "{} [{}]", r.name, r.household);
        if r.zone != r.name {
            let _ = write!(out, " in {}'s zone", r.zone);
        }
        if !r.aliases.is_empty() {
            let _ = write!(out, "  aliases: {}", r.aliases.join(", "));
        }
        out.push('\n');
        out
    })
}

/// `fsonos favorites` as text: numbered, as `play --favorite <n>` accepts.
#[must_use]
pub fn favorites_text(favorites: &[FavoriteDto]) -> String {
    if favorites.is_empty() {
        return "no favorites in this household\n".to_string();
    }
    favorites
        .iter()
        .enumerate()
        .fold(String::new(), |mut out, (i, f)| {
            let _ = writeln!(out, "{:>3}. {}  ({})", i + 1, f.title, f.kind);
            out
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn zone(members: &[&str], state: &str, degraded: &[&str]) -> ZoneDto {
        ZoneDto {
            coordinator_room: members[0].into(),
            members: members.iter().map(|m| (*m).to_string()).collect(),
            transport_state: state.into(),
            household: "S1".into(),
            degraded: degraded.iter().map(|m| (*m).to_string()).collect(),
        }
    }

    #[test]
    fn zones_render_one_line_per_group() {
        let text = zones_text(&[
            zone(&["Den", "Kitchen"], "playing", &[]),
            zone(&["Studio"], "stopped", &["Studio"]),
        ]);
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].starts_with("S1   Den + Kitchen") && lines[0].ends_with("playing"));
        assert!(lines[1].ends_with("stopped  (degraded: Studio)"), "{text}");
    }

    #[test]
    fn discover_renders_players_or_says_none() {
        let d = DiscoverDto {
            players: vec![PlayerDto {
                room: "Den".into(),
                id: "RINCON_DEN".into(),
                model: "Sonos One".into(),
                generation: Generation::S2,
                ip: "192.0.2.10".into(),
                household: "S2".into(),
            }],
            unreachable: Vec::new(),
            ssdp_error: None,
        };
        let text = discover_text(&d);
        assert!(
            text.starts_with("Den")
                && text.contains(" S2  Sonos One")
                && text.contains("192.0.2.10")
        );
        let empty = DiscoverDto {
            players: Vec::new(),
            ..d
        };
        assert_eq!(discover_text(&empty), "no players found\n");
    }
}
