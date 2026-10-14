//! The `get_policy` tool (see `fsonos_api::surface::house_policy`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};

use super::{Backend, respond, with_backend};

impl Backend {
    /// The `get_policy` tool.
    pub fn get_policy(&self) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let policy = self.surface.policy_view(&self.client)?;
            Ok((policy.text(), policy))
        })
    }
}

#[tool(
    description = "The house policy in effect, and who you are under it: the default room volume cap and the largest single increase, the quiet-hours window and its lower cap, rooms with caps of their own, and clients with tool rules (only these tools, never these) or caps switched on or off. Check it before a volume change that might be clamped, or when a tool answers POLICY_DENIED.",
    annotations(read_only, idempotent)
)]
fn get_policy(_ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(Backend::get_policy)
}
