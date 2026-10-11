//! The `get_eq` and `set_eq` tools (see `fsonos_api::surface::eq`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::eq::EqRequest;

use super::{Backend, respond, with_backend};

impl Backend {
    /// The `get_eq` tool.
    pub fn get_eq(&self, zone: &str) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let eq = self.surface.get_eq(&self.client, zone)?;
            Ok((eq.text(), eq))
        })
    }

    /// The `set_eq` tool.
    pub fn set_eq(&self, req: &EqRequest) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let set = self.surface.set_eq(&self.client, req)?;
            Ok((set.done.clone(), set))
        })
    }
}

#[tool(
    description = "A room's tone: bass and treble from -10 to 10 (0 is flat), and whether loudness compensation (more bass and treble at low volume) is on. `zone` is a room name, Room@S1 / Room@S2, or one of your aliases.",
    annotations(read_only, idempotent)
)]
fn get_eq(_ctx: &McpContext, zone: String) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(move |b| b.get_eq(&zone))
}

#[tool(
    description = "Change a room's tone: any of `bass` and `treble` (-10 to 10, 0 is flat) and `loudness` (true/false); what you leave out stays as it is. Undoable with undo_last. `zone` is a room name, Room@S1 / Room@S2, or one of your aliases.",
    annotations(idempotent)
)]
fn set_eq(
    _ctx: &McpContext,
    zone: String,
    bass: Option<i64>,
    treble: Option<i64>,
    loudness: Option<bool>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(move |b| {
        b.set_eq(&EqRequest {
            zone,
            bass,
            treble,
            loudness,
        })
    })
}
