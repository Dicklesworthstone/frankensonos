//! Agent controls for the existing queue's shuffle/repeat settings.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::queue::PlayModeRequest;

use crate::worker;

#[tool(
    description = "Read the group's queue play mode, whether its own queue is selected, and whether the running DJ owns queue order. Modes are NORMAL, REPEAT_ALL, REPEAT_ONE, SHUFFLE (shuffle plus repeat all), SHUFFLE_NOREPEAT, and SHUFFLE_REPEAT_ONE. Unknown or unsupported settings return an error, never an assumed NORMAL. A group member resolves to its current coordinator.",
    annotations(read_only, idempotent)
)]
async fn get_play_mode(
    ctx: &McpContext,
    zone: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    ctx.checkpoint()?;
    worker::run(ctx, move |backend| backend.get_play_mode(&zone)).await
}

#[tool(
    description = "Set an already-selected queue's shuffle/repeat mode without changing its source, track, time, volume, contents or playing/paused state. mode is a complete uppercase mode: NORMAL, REPEAT_ALL, REPEAT_ONE, SHUFFLE, SHUFFLE_NOREPEAT, or SHUFFLE_REPEAT_ONE. SHUFFLE repeats all; choose SHUFFLE_NOREPEAT for shuffle alone. Pass a previous get_play_mode value as expected_mode to reject a stale setting. Stop an active DJ before non-NORMAL modes; it owns sequential order. A non-queue source is never started or replaced to enable this change. A logged mutation supports explicit best-effort full-playback snapshot undo, not mode-only undo. On an uncertain response read the mode again; never replay automatically or discard expected_mode."
)]
async fn set_play_mode(
    ctx: &McpContext,
    zone: String,
    mode: String,
    expected_mode: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    ctx.checkpoint()?;
    worker::run(ctx, move |backend| {
        backend.set_play_mode(&PlayModeRequest {
            zone,
            mode,
            expected_mode,
        })
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn both_mode_tools_register_on_the_dual_era_server() {
        fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
            "mode-test",
            "1.0",
            fastmcp::ProtocolPolicy::Auto,
        )
        .unwrap()
        .tool(GetPlayMode)
        .tool(SetPlayMode)
        .try_build()
        .expect("mode tools register with their generated input schemas");
    }
}
