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
    description = "Play one Sonos native line-in on every selected existing group. zone accepts a room, a multi-room alias, all/everywhere, or all@S1 / all@S2; grouped rooms act once. source is an exact player id from list_line_in, or a source room/single-room alias. Exact ids preserve the selected input even when its player belongs to a stereo pair. Every destination must be in the source's household; a cross-household selection is rejected before playback or volume changes. The source must advertise a usable native input; discovery does not prove that a cable is connected or audio is present. Starts each destination coordinator with the existing play permission and per-room volume caps, sharing one action log and playback undo. All destinations are prepared before writes. Multi-group receipts retain each success and failure; an incomplete result marks the tool as an error. Groups remain independent, without synchronized starts. Retains queue entries and does not regroup rooms. A live input cannot be queued. No Spotify sign-in is needed. Never automatically retry an uncertain playback response."
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
