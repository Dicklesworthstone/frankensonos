//! The `announce` tool: speak or chime in rooms, then put the music back.

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
    description = "Speak `text` (macOS say, at most 1000 characters; optional `voice`) or play a `chime` (bell, beep, rise) in `rooms` (names, aliases or 'all'; every room when omitted), then put every zone back as it was: grouping, what played, position, volumes and mute. `volume` 0-100, default 35, capped per room by the house policy. Waits until the clip has played and the music is back. Not undoable: there is nothing left to undo."
)]
fn announce(
    _ctx: &McpContext,
    text: Option<String>,
    chime: Option<String>,
    voice: Option<String>,
    rooms: Option<Vec<String>>,
    volume: Option<u32>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = AnnounceRequest {
        text,
        chime,
        voice,
        rooms: rooms.unwrap_or_default(),
        volume: volume.map(i64::from),
    };
    with_backend(move |b| b.announce(&req))
}
