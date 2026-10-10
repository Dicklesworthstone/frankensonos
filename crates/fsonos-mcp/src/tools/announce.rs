//! The `announce` tool: speak, chime or upload a WAV, then put the music back.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::announce::AnnounceRequest;

use super::{Backend, respond, with_backend};

impl Backend {
    /// The `announce` tool.
    pub fn announce(&self, req: &AnnounceRequest) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let announced = self.surface.announce(&self.client, req)?;
            Ok((announced.done.clone(), announced))
        })
    }
}

#[tool(
    description = "Requires the daemon's media listener (fsonos serve). Give exactly one of `text` (local offline speech backend, at most 1000 characters; optional `voice`), `chime` (bell, beep, rise), or `wav_base64` (16-bit PCM WAV bytes as standard padded base64, at most 16 MiB decoded and five minutes; never a file path). Play it in `rooms` (names, aliases or 'all'; every room when omitted), then restore grouping, source, position, volumes and mute. `volume` 0-100, default 35, capped per room by the house policy. Waits until the clip has played and the music is back. Not undoable: there is nothing left to undo."
)]
fn announce(
    _ctx: &McpContext,
    text: Option<String>,
    chime: Option<String>,
    wav_base64: Option<String>,
    voice: Option<String>,
    rooms: Option<Vec<String>>,
    volume: Option<u32>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = AnnounceRequest {
        text,
        chime,
        wav_base64,
        voice,
        rooms: rooms.unwrap_or_default(),
        volume: volume.map(i64::from),
    };
    with_backend(move |b| b.announce(&req))
}
