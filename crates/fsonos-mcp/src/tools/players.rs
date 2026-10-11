//! The `list_players` tool, over the surface's players
//! (`fsonos_api::surface::players`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::players::PlayerDto;
use serde::Serialize;

use super::{Backend, respond, with_backend};

/// `list_players` structured content (MCP wants an object).
#[derive(Serialize)]
struct PlayersDto {
    players: Vec<PlayerDto>,
}

/// One player in a line: room, household, model, generation, address.
fn player_line(p: &PlayerDto) -> String {
    let model = if p.model.is_empty() {
        "Sonos"
    } else {
        p.model.as_str()
    };
    format!(
        "{} [{}] {model} ({}) at {}",
        p.room, p.household, p.generation, p.ip
    )
}

impl Backend {
    /// The `list_players` tool.
    pub fn list_players(&self) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let players = self.surface.players(&self.client)?;
            let text = players
                .iter()
                .map(player_line)
                .collect::<Vec<_>>()
                .join("\n");
            Ok((text, PlayersDto { players }))
        })
    }
}

#[tool(
    description = "List every Sonos player of every household: its room, household (S1/S2), model, generation, and IP address. This is the device inventory `fsonos discover` shows; use list_zones for how they group and list_rooms for room aliases.",
    annotations(read_only, idempotent)
)]
fn list_players(_ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(Backend::list_players)
}
