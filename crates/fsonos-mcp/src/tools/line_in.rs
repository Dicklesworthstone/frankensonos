//! Native speaker inputs, discovered and played through one shared authority.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::{LineInSourcesDto, PlayLineInRequest, ZoneRequest};

use super::{Backend, respond, said, segment, with_backend_off_thread};

impl Backend {
    /// Read the selected daemon's household, preserving partial read failures.
    pub fn list_line_in(&self, zone: &str) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let request = ZoneRequest {
                zone: zone.to_owned(),
            };
            let zone = request.zone()?;
            let sources: LineInSourcesDto = self
                .read(&format!("/line-in?zone={}", segment(zone)), || {
                    self.surface.line_in_sources(&self.client, zone)
                })?;
            Ok((sources.text(), sources))
        })
    }

    /// A dispatched native-input command never falls back to local speakers.
    pub fn play_line_in(&self, request: &PlayLineInRequest) -> McpResult<FinalCallToolResult> {
        request
            .validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        self.write("/line-in/play", request, || {
            respond(|| self.surface.play_line_in(&self.client, request).map(said))
        })
    }
}

#[tool(
    description = "List native line-in sources on the Sonos household containing zone. A source includes its exact player id, room, input name and icon. Read failures remain in unavailable, so an empty sources list does not prove that no input exists. A listed input does not prove that a cable is connected or audio is present. This read changes no playback, queue, volume or grouping and needs no Spotify sign-in. Uses list_players permission. Select a returned player id with play_line_in to play that exact input on a group in the same household; Room@S1 or Room@S2 disambiguates the destination.",
    annotations(read_only, idempotent)
)]
async fn list_line_in(
    ctx: &McpContext,
    zone: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| backend.list_line_in(&zone)).await
}

#[tool(
    description = "Play a Sonos native line-in on zone's current group. source is an exact player id from list_line_in, or a source room/single-room alias. Exact ids preserve the selected input even when its player belongs to a stereo pair. Source and destination must be in the same household, and the source must advertise a usable native input; discovery does not prove that a cable is connected or audio is present. Starts the destination group's coordinator with the existing play permission, volume caps, action log and playback undo. Retains queue entries and does not regroup rooms. A live input cannot be queued. No Spotify sign-in is needed. Never automatically retry an uncertain playback response."
)]
async fn play_line_in(
    ctx: &McpContext,
    zone: String,
    source: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| {
        backend.play_line_in(&PlayLineInRequest { zone, source })
    })
    .await
}

#[cfg(test)]
mod tests;
