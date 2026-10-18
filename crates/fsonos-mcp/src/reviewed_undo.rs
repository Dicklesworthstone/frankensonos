//! Review an exact caller-owned action before restoring its before-state.

use crate::{tools::Backend, worker};
use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};

#[tool(
    description = "Read your newest eligible action from the authoritative action log before undoing it. Returns structured action (or null), with its id, owner, intent and result. This is an observation, not a reservation or permission grant. Review the result, then pass exactly its id to undo_checked. The caller identity can be shared by other local agents. Restoring a before-state may overwrite later changes and has no redo.",
    annotations(read_only, idempotent)
)]
async fn preview_undo(ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    ctx.checkpoint()?;
    worker::run(ctx, Backend::preview_undo).await
}

#[tool(
    description = "Undo exactly the caller-owned action reviewed with preview_undo. expected_id is the positive id from that preview. A superseded, consumed or foreign target is refused rather than replaced by a different action; never drop expected_id or fall back to undo_last. Inspect the summary and failures for partial restoration. This can overwrite later speaker changes and has no redo. On a lost reply, inspect playback and preview again; never automatically replay a sent undo. Uses the same policy and daemon as the other controls."
)]
async fn undo_checked(
    ctx: &McpContext,
    expected_id: i64,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    ctx.checkpoint()?;
    worker::run(ctx, move |backend| backend.undo_checked(expected_id)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tools_register_with_the_dual_era_server() {
        fastmcp_server::ServerBuilder::try_new_with_fixed_protocol_policy(
            "reviewed-undo-test",
            "1.0",
            fastmcp::ProtocolPolicy::Auto,
        )
        .unwrap()
        .tool(PreviewUndo)
        .tool(UndoChecked)
        .try_build()
        .unwrap();
    }
}
