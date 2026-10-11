//! The `add_to_queue` tool (see `fsonos_api::surface::queue`).

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::queue::QueueAddRequest;

use super::{Backend, respond, with_backend};

impl Backend {
    /// The `add_to_queue` tool.
    pub fn add_to_queue(&self, req: &QueueAddRequest) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let added = self.surface.queue_add(&self.client, req)?;
            Ok((added.done.clone(), added))
        })
    }
}

#[tool(
    description = "Add a Spotify track, album or playlist (`spotify:track|album|playlist:<id>`, or an open.spotify.com link) to the queue of the group a room plays in: at the end, or with `next` true right after the track playing. An album or playlist adds all its tracks; `title` names it in the queue. Undoable with undo_last. `zone` is a room name, Room@S1 / Room@S2, or one of your aliases."
)]
fn add_to_queue(
    _ctx: &McpContext,
    zone: String,
    source_uri: String,
    title: Option<String>,
    next: Option<bool>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend(move |b| {
        b.add_to_queue(&QueueAddRequest {
            zone,
            source_uri,
            title,
            next: next.unwrap_or(false),
        })
    })
}
