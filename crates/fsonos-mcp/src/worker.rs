//! Bounded workers for tool handlers that wait on the daemon or speaker LAN.

use crate::{tool_error, tools};
use fastmcp::prelude::*;
use fastmcp::{CompleteResult, ContentBlock, FinalCallToolResult, ResultMeta};

pub(crate) async fn run(
    ctx: &McpContext,
    call: impl FnOnce(&tools::Backend) -> McpResult<FinalCallToolResult> + Send + 'static,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    ctx.checkpoint()?;
    let backend = tools::backend_for(ctx).map_err(|failure| tool_error(&failure))?;
    let done = perform(move || call(&backend)).await?;
    if done.is_error && !ctx.is_final_request_surface() {
        return Err(legacy_tool_error(done));
    }
    Ok(CompleteResult::new(done, ResultMeta::empty()))
}

/// The legacy macro adapter cannot project a final payload's error bit into
/// its content-only return type. Its tool-error channel retains `isError`;
/// include the complete receipt as JSON text because 2024-11-05 has no
/// structured-content field. Modern clients keep their original final result.
fn legacy_tool_error(result: FinalCallToolResult) -> McpError {
    let mut message = result
        .content
        .iter()
        .filter_map(|content| match content {
            ContentBlock::Text { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if let Some(receipt) = result.structured_content {
        message.push_str("\n\nReceipt: ");
        message.push_str(&receipt.to_string());
        McpError::with_data(fastmcp::McpErrorCode::ToolExecutionError, message, receipt)
    } else {
        McpError::tool_error(message)
    }
}

async fn perform(
    call: impl FnOnce() -> McpResult<FinalCallToolResult> + Send + 'static,
) -> McpResult<FinalCallToolResult> {
    // The shared limiter refuses overload before running a closure. Once a
    // closure starts, dropping the awaiting request cannot undo its I/O and
    // must not release its slot early or cause another attempt.
    fsonos_api::off_thread::off_thread(call)
        .map_err(|failure| tool_error(&failure))?
        .await
        // The generic tool runner has no proof that its closure was read-only.
        .ok_or_else(|| tool_error(&fsonos_api::off_thread::interrupted(true)))?
}

#[cfg(test)]
mod tests;
