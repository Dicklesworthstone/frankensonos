//! Native inputs are selected by the player that owns the jack, independently
//! of the coordinator that will render it. Discovery never changes playback.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::audio_inputs;
use fsonos_core::policy::Client;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::{Surface, resolve, room_view};
use crate::request::zone_name;
use crate::{Command, Failure, OutcomeDto};

/// A currently advertised native input in the destination's household.
/// The AudioIn service has no connection-state getter: a listed jack does
/// not establish that a cable is connected or that audio is arriving.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LineInSourceDto {
    /// Exact player UUID, including a bonded secondary when it owns the input.
    pub player: String,
    pub room: String,
    pub name: String,
    pub icon: String,
}

/// A player whose input capability could not be read during this discovery.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LineInUnavailableDto {
    pub player: String,
    pub room: String,
    pub detail: String,
}

/// Healthy sources remain usable when another player's discovery fails.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LineInSourcesDto {
    pub sources: Vec<LineInSourceDto>,
    pub unavailable: Vec<LineInUnavailableDto>,
}

impl From<audio_inputs::Catalog> for LineInSourcesDto {
    fn from(catalog: audio_inputs::Catalog) -> Self {
        Self {
            sources: catalog
                .sources
                .into_iter()
                .map(|source| LineInSourceDto {
                    player: source.player.0,
                    room: source.room,
                    name: source.name,
                    icon: source.icon,
                })
                .collect(),
            unavailable: catalog
                .unavailable
                .into_iter()
                .map(|source| LineInUnavailableDto {
                    player: source.player.0,
                    room: source.room,
                    detail: source.detail,
                })
                .collect(),
        }
    }
}

impl LineInSourcesDto {
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = String::new();
        for source in &self.sources {
            let name = if source.name.is_empty() {
                "Line-in"
            } else {
                &source.name
            };
            let _ = writeln!(out, "{} — {name}  ({})", source.room, source.player);
        }
        if self.sources.is_empty() {
            out.push_str(if self.unavailable.is_empty() {
                "no native line-in sources advertised in this household\n"
            } else {
                "no native line-in sources confirmed; some players could not be read\n"
            });
        } else {
            out.push_str(
                "Input availability does not confirm a connected cable or an audio signal.\n",
            );
        }
        for missing in &self.unavailable {
            let _ = writeln!(
                out,
                "{}: input discovery unavailable ({})",
                missing.room, missing.detail
            );
        }
        out
    }
}

/// Select a native input for one destination group, retaining its queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlayLineInRequest {
    pub zone: String,
    /// An exact player UUID from discovery, or one source room/alias.
    pub source: String,
}

impl PlayLineInRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        zone_name("zone", &self.zone)?;
        zone_name("source", &self.source)?;
        if self
            .zone
            .chars()
            .chain(self.source.chars())
            .any(char::is_control)
        {
            return Err(Failure::invalid(
                "line-in targets must not contain control characters",
            ));
        }
        Ok(())
    }
}

impl Surface {
    /// Read the target household's advertised inputs under player-read policy.
    pub fn line_in_sources(
        &self,
        client: &Client,
        zone: &str,
    ) -> Result<LineInSourcesDto, Failure> {
        self.guard(client).authorize("list_players", true)?;
        zone_name("zone", zone)?;
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(
            room_view(&households, aliases.as_ref(), client),
            zone.trim(),
        )
        .map_err(|failure| self.explain(failure))?;
        let catalog = audio_inputs::list(&*self.transport, &households, &target.coordinator.id)?;
        // A read may span several players; an edited policy governs its result.
        self.guard(client).authorize("list_players", true)?;
        Ok(catalog.into())
    }

    /// Resolve and prepare the input before snapshots, fades or volume writes.
    /// The retained source and destination are rechecked at execution.
    pub fn play_line_in(
        &self,
        client: &Client,
        request: &PlayLineInRequest,
    ) -> Result<OutcomeDto, Failure> {
        request.validate()?;
        self.control_io(client, "play", |transport, rooms| {
            let target = resolve(rooms, request.zone.trim())?;
            let selected = request.source.trim();
            // Room resolution normally maps bonded players to their primary.
            // A catalog UUID identifies the actual input and must retain it.
            let mut exact = rooms
                .households
                .iter()
                .flat_map(|house| &house.players)
                .filter(|player| player.id.0.eq_ignore_ascii_case(selected));
            let source = match (exact.next(), exact.next()) {
                (Some(player), None) => player.id.clone(),
                (None, None) => resolve(rooms, selected)?.player.id.clone(),
                _ => return Err(Failure::invalid("the input player identity is ambiguous")),
            };
            let coordinator = target.coordinator.id.clone();
            let prepared =
                audio_inputs::prepare(transport, rooms.households, &coordinator, &source)?;
            Ok(Command::PlayLineIn {
                coordinator,
                prepared,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn input_requests_require_bounded_targets_and_reject_renderer_fields() {
        for (zone, source) in [("", "Office"), ("Den", " "), ("Den", "Office\n")] {
            assert!(
                PlayLineInRequest {
                    zone: zone.into(),
                    source: source.into()
                }
                .validate()
                .is_err()
            );
        }
        for source in ["Office@S1", "RINCON_INPUT_01400", "here"] {
            assert!(
                PlayLineInRequest {
                    zone: "Den@S1".into(),
                    source: source.into()
                }
                .validate()
                .is_ok()
            );
        }
        assert!(
            serde_json::from_value::<PlayLineInRequest>(serde_json::json!({
                "zone": "Den", "source": "Office", "source_uri": "x-rincon-stream:invented"
            }))
            .is_err()
        );
        assert!(
            serde_json::from_value::<PlayLineInRequest>(serde_json::json!({
                "zone": "Den", "source": "Office", "mode": "append"
            }))
            .is_err()
        );
    }

    #[test]
    fn incomplete_discovery_is_distinct_from_a_household_without_inputs() {
        let mut catalog = LineInSourcesDto {
            sources: Vec::new(),
            unavailable: Vec::new(),
        };
        assert!(
            catalog
                .text()
                .contains("no native line-in sources advertised")
        );
        catalog.unavailable.push(LineInUnavailableDto {
            player: "RINCON_INPUT_01400".into(),
            room: "Office".into(),
            detail: "offline".into(),
        });
        assert!(catalog.text().contains("some players could not be read"));
        catalog.sources.push(LineInSourceDto {
            player: "RINCON_OTHER_01400".into(),
            room: "Den".into(),
            name: "Turntable".into(),
            icon: "Phono".into(),
        });
        let value = serde_json::to_value(&catalog).unwrap();
        assert!(value["sources"][0].get("connected").is_none());
        assert!(
            catalog
                .text()
                .contains("does not confirm a connected cable")
        );
        assert!(catalog.text().contains("Turntable"));
    }
}
