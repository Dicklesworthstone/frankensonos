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
}
