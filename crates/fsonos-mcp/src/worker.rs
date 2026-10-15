//! Bounded workers for tool handlers that wait on the daemon or speaker LAN.

use crate::{tool_error, tools};
use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult, ResultMeta};
use fsonos_api::{ErrorCode, Failure};

pub(crate) async fn run(
    call: impl FnOnce(&tools::Backend) -> McpResult<FinalCallToolResult> + Send + 'static,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let backend = tools::backend().map_err(|failure| tool_error(&failure))?;
    let done = fsonos_api::off_thread::off_thread(move || call(backend))
        .map_err(|failure| tool_error(&failure))?
        .await
        .ok_or_else(|| {
            tool_error(&Failure::new(
                ErrorCode::Internal,
                "the call stopped before it answered; inspect state before trying again",
            ))
        })??;
    Ok(CompleteResult::new(done, ResultMeta::empty()))
}
