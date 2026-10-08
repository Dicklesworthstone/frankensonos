//! The `dj_feedback` tool (see `fsonos_api::surface::dj_feedback`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::dj_feedback::DjFeedbackRequest;

use super::{Backend, respond, with_backend};

impl Backend {
    /// The `dj_feedback` tool.
    pub fn dj_feedback(&self, req: &DjFeedbackRequest) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let given = self.surface.dj_feedback(&self.client, req)?;
            Ok((given.done.clone(), given))
        })
    }
}

#[tool(
    description = "Tell the classical DJ the owner likes or dislikes the work playing now: `signal` is 'like' or 'dislike'. It nudges the work, its composer and its performer (decaying over weeks), so the DJ plays them more or less; two dislikes of a work keep it out for 180 days. `zone` is a room of the group it plays in (left out: the group the DJ plays in, when it plays in just one). Skips within 30 s and works heard to the end are noted on their own."
)]
fn dj_feedback(
    _ctx: &McpContext,
    signal: String,
    zone: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = DjFeedbackRequest { zone, signal };
    with_backend(move |b| b.dj_feedback(&req))
}
