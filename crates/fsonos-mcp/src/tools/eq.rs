//! The `get_eq` and `set_eq` tools (see `fsonos_api::surface::eq`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::eq::{EqRequest, EqSetDto};

use super::{Backend, respond, segment, with_backend_off_thread};

impl Backend {
    /// The `get_eq` tool.
    pub fn get_eq(&self, zone: &str) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let path = format!("/zones/{}/eq", segment(zone));
            let eq = self.read(&path, || self.surface.get_eq(&self.client, zone))?;
            Ok((eq.text(), eq))
        })
    }

    /// The `set_eq` tool.
    pub fn set_eq(&self, req: &EqRequest) -> McpResult<FinalCallToolResult> {
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&fsonos_api::Failure::new(
                fsonos_api::ErrorCode::Internal,
                error.to_string(),
            ))
        })?;
        if let Some(answer) = self.on_daemon("POST", "/eq", Some(&body), |set: &EqSetDto| {
            set.done.clone()
        }) {
            return answer;
        }
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
async fn get_eq(ctx: &McpContext, zone: String) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |b| b.get_eq(&zone)).await
}

#[tool(
    description = "Change a room's tone: any of `bass` and `treble` (-10 to 10, 0 is flat) and `loudness` (true/false); what you leave out stays as it is. Undoable with undo_last. `zone` is a room name, Room@S1 / Room@S2, or one of your aliases.",
    annotations(idempotent)
)]
async fn set_eq(
    ctx: &McpContext,
    zone: String,
    bass: Option<i64>,
    treble: Option<i64>,
    loudness: Option<bool>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |b| {
        b.set_eq(&EqRequest {
            zone,
            bass,
            treble,
            loudness,
        })
    })
    .await
}
