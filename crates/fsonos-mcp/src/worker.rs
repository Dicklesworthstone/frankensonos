//! Bounded workers for tool handlers that wait on the daemon or speaker LAN.

use crate::{tool_error, tools};
use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult, ResultMeta};
use fsonos_api::{ErrorCode, Failure};

pub(crate) async fn run(
    ctx: &McpContext,
    call: impl FnOnce(&tools::Backend) -> McpResult<FinalCallToolResult> + Send + 'static,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    ctx.checkpoint()?;
    let backend = tools::backend_for(ctx).map_err(|failure| tool_error(&failure))?;
    let done = perform(move || call(&backend)).await?;
    Ok(CompleteResult::new(done, ResultMeta::empty()))
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
        .ok_or_else(|| {
            tool_error(&Failure::new(
                ErrorCode::Internal,
                "the call stopped before it answered; inspect state before trying again",
            ))
        })?
}

#[cfg(test)]
mod tests;
