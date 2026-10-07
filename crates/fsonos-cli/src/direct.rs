//! Direct mode: the CLI finds the speakers itself (SSDP plus any seeds),
//! plans each command with the shared `fsonos-api` layer, and sends it
//! through the core's control orchestration.

use fsonos_api::zones::zone_views;
use fsonos_api::{Command, ErrorCode, Failure, OutcomeDto, ZoneDto, execute};
use fsonos_core::inventory::{self, Survey};
use fsonos_core::rooms::household_labels;
use fsonos_core::{CoreError, HouseholdState, control};
use fsonos_proto::net::Lan;
use fsonos_types::{Generation, TransportState};
use serde::Serialize;
use std::fmt::Write as _;

use crate::config::GlobalArgs;

/// A surveyed LAN, ready to take commands.
pub struct Direct {
    lan: Lan,
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
    /// Survey the LAN: SSDP for `global.wait`, plus the seeds.
    pub fn survey(global: &GlobalArgs) -> Result<Self, Failure> {
        let seeds = global.seed_addrs()?;
        let lan = Lan::start().map_err(|e| Failure::from(CoreError::from(e)))?;
        let survey = inventory::survey(&lan, &seeds, global.wait())?;
        Ok(Self { lan, survey })
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

    /// Plan a command against the surveyed households and carry it out.
    pub fn run(
        &self,
        plan: impl FnOnce(&[HouseholdState]) -> Result<Command, Failure>,
    ) -> Result<OutcomeDto, Failure> {
        let households = self.households()?;
        let command = plan(households)?;
        execute(&self.lan, households, &command)
    }

    /// Every zone with its live transport state (read from each coordinator;
    /// `unknown` when one does not answer).
    pub fn zones(&self) -> Result<Vec<ZoneDto>, Failure> {
        let households = self.households()?;
        Ok(zone_views(households, |coordinator| {
            control::playback(&self.lan, households, coordinator)
                .map_or(TransportState::Unknown, |p| p.transport.state)
        }))
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
