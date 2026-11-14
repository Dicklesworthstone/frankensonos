//! Owner-managed room names and default targets on the selected daemon.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::room_config::{
    RoomConfigurationChange, RoomConfigurationDto, RoomConfigureRequest, RoomConfiguredDto,
};
use fsonos_api::{ErrorCode, Failure};

use super::{Backend, respond, with_backend_off_thread};

fn summary(configuration: &RoomConfigurationDto) -> String {
    format!(
        "Room configuration revision {}: {} aliases and {} client defaults; global default {}. Read the complete definitions before configure_rooms. No speaker command occurs.",
        configuration.revision,
        configuration.aliases.len(),
        configuration.client_defaults.len(),
        configuration.default_room.as_deref().unwrap_or("unset"),
    )
}

impl Backend {
    pub fn room_configuration(&self) -> McpResult<FinalCallToolResult> {
        if let Some(answer) = self.on_daemon("GET", "/rooms/configuration", None, summary) {
            return answer;
        }
        respond(|| {
            let configuration = self.surface.room_configuration(&self.client)?;
            Ok((summary(&configuration), configuration))
        })
    }

    pub fn configure_rooms(
        &self,
        request: &RoomConfigureRequest,
    ) -> McpResult<FinalCallToolResult> {
        request
            .validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        let body = serde_json::to_value(request).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(answer) = self.on_daemon(
            "POST",
            "/rooms/configuration",
            Some(&body),
            |saved: &RoomConfiguredDto| saved.done.clone(),
        ) {
            return answer;
        }
        respond(|| {
            let saved = self.surface.configure_rooms(&self.client, request)?;
            Ok((saved.done.clone(), saved))
        })
    }
}

#[tool(
    description = "Read all authored room aliases and global/per-client default targets on the selected daemon, including offline targets, an exact revision, and advisory inventory warnings. This is the complete saved configuration, unlike list_rooms, which reports discovered rooms. Aliases select physical rooms; they do not group speakers. The default room resolves here; a client's explicit default overrides the global one. Read before configure_rooms. No speaker command or network survey occurs.",
    annotations(read_only, idempotent)
)]
async fn room_configuration(ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, Backend::room_configuration).await
}

#[tool(
    description = "Change one saved room alias or default target in the selected daemon's configuration. Requires configure_rooms (house-config) and the owner or manage_others administrator permission. First call room_configuration and supply its exact revision as expected_revision. change is one tagged object: {kind:'set_alias',name:'downstairs',rooms:['Kitchen@S1','Office@S2']}, {kind:'remove_alias',name:'downstairs'}, {kind:'set_default',room:'Office@S2',client:'optional exact policy client key'}, or {kind:'clear_default',client:'optional exact policy client key'}. Omit client to change the global default; clearing a client's override restores the global fallback. Aliases name physical rooms or player IDs, never other aliases; all/everywhere/here are reserved. Preserve offline targets intentionally. Unrelated entries/comments remain. An identical change is a verified no-op. Changes affect future target resolution, including saved routines, and send no playback/group/volume commands. This configuration edit is not undoable through playback history. CONFIG_CHANGED or a lost/uncertain response requires a fresh read and review; never automatically retry or fall back to a second store."
)]
async fn configure_rooms(
    ctx: &McpContext,
    expected_revision: String,
    change: RoomConfigurationChange,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let request = RoomConfigureRequest {
        expected_revision,
        change,
    };
    request
        .validate()
        .map_err(|failure| crate::tool_error(&failure))?;
    with_backend_off_thread(ctx, move |backend| backend.configure_rooms(&request)).await
}

#[cfg(test)]
mod tests;
