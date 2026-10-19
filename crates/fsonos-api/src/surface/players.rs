//! `GET /players`: every player of every household, as `fsonos discover`
//! lists them. [`player_views`] builds the list for both, so a discover
//! through the daemon prints what a direct one does.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::HouseholdState;
use fsonos_core::policy::Client;
use fsonos_core::rooms::household_labels;
use fsonos_types::Generation;
use serde::{Deserialize, Serialize};

use super::Surface;
use crate::failure::{ErrorCode, Failure};

pub mod targets;

/// The tool name the players read is authorized as.
pub const TOOL: &str = "list_players";

/// One player.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PlayerDto {
    pub room: String,
    pub id: String,
    pub model: String,
    /// `S1` or `S2`.
    pub generation: String,
    pub ip: String,
    /// The household's label (`S1`, `S2`, or its id).
    pub household: String,
    /// The room whose player leads this one's group: its own room when it
    /// leads or plays alone, empty when the topology does not say.
    #[serde(default)]
    pub coordinator_room: String,
}

impl PlayerDto {
    /// " (in Den's group)" for a player another room's player leads, else
    /// nothing.
    #[must_use]
    pub fn grouped(&self) -> String {
        if self.coordinator_room.is_empty() || self.coordinator_room == self.room {
            String::new()
        } else {
            format!(" (in {}'s group)", self.coordinator_room)
        }
    }
}

/// Every player in `households`, by household and then room.
#[must_use]
pub fn player_views(households: &[HouseholdState]) -> Vec<PlayerDto> {
    let labels = household_labels(households);
    let mut players: Vec<PlayerDto> = households
        .iter()
        .zip(&labels)
        .flat_map(|(h, label)| {
            h.players.iter().map(move |p| PlayerDto {
                room: p.room_name.clone(),
                id: p.id.0.clone(),
                model: p.model.clone(),
                generation: match p.generation {
                    Generation::S1 => "S1",
                    Generation::S2 => "S2",
                }
                .to_string(),
                ip: p.ip.to_string(),
                household: label.clone(),
                coordinator_room: h
                    .coordinator_of(&p.id)
                    .and_then(|c| h.players.iter().find(|q| q.id == *c))
                    .map_or_default(|q| q.room_name.clone()),
            })
        })
        .collect();
    players.sort_by(|a, b| (&a.household, &a.room).cmp(&(&b.household, &b.room)));
    players
}

impl Surface {
    /// Every player the surface knows, for `client`.
    pub fn players(&self, client: &Client) -> Result<Vec<PlayerDto>, Failure> {
        self.guard(client).authorize(TOOL, true)?;
        let households = self.households()?;
        if households.iter().all(|h| h.players.is_empty()) {
            return Err(Failure::new(
                ErrorCode::NotReady,
                "no Sonos players answered",
            ));
        }
        Ok(player_views(&households))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generations_read_as_their_serde_names() {
        // The CLI's discover printed Generation through serde: keep the text.
        for (generation, label) in [(Generation::S1, "S1"), (Generation::S2, "S2")] {
            assert_eq!(serde_json::to_value(generation).unwrap(), label);
        }
        assert!(player_views(&[]).is_empty());
    }

    #[test]
    fn each_player_names_the_room_that_leads_its_group() {
        let players = player_views(&crate::zones::fixtures::households());
        let leader = |room: &str, household: &str| {
            let p = players
                .iter()
                .find(|p| p.room == room && p.household == household)
                .unwrap();
            (p.coordinator_room.clone(), p.grouped())
        };
        // Kitchen@S1 plays in Den's group; Den leads it; the Studio plays alone.
        assert_eq!(
            leader("Kitchen", "S1"),
            ("Den".to_string(), " (in Den's group)".to_string())
        );
        assert_eq!(leader("Den", "S1"), ("Den".to_string(), String::new()));
        assert_eq!(leader("Kitchen", "S2").1, "");
        assert_eq!(leader("Ada\u{2019}s Studio", "S1").1, "");
    }
}
