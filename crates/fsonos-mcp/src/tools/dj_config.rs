//! Typed editor tools for the daemon's reusable moods and daily programs.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::dj_config::{
    DjConfigDocument, DjConfigDto, DjConfigureRequest, DjConfiguredDto,
};
use fsonos_api::{ErrorCode, Failure};

use super::{Backend, respond, with_backend_off_thread};

fn summary(configuration: &DjConfigDto) -> String {
    format!(
        "DJ configuration revision {}: {} effective moods and {} automatic programs. Edit config and pass this revision to dj_configure.",
        configuration.revision,
        configuration.moods.len(),
        configuration.programs.len(),
    )
}

impl Backend {
    pub fn dj_configuration(&self) -> McpResult<FinalCallToolResult> {
        if let Some(answer) = self.on_daemon("GET", "/dj/configuration", None, summary) {
            return answer;
        }
        respond(|| {
            let configuration = self.surface.dj_configuration(&self.client)?;
            Ok((summary(&configuration), configuration))
        })
    }

    /// Dispatch once, keeping both the requested document and its revision.
    /// A remote conflict or uncertain response never falls back or retries.
    pub fn dj_configure(&self, request: &DjConfigureRequest) -> McpResult<FinalCallToolResult> {
        let request = request
            .checked()
            .map_err(|error| crate::tool_error(&error))?;
        let body = serde_json::to_value(&request).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(answer) = self.on_daemon(
            "POST",
            "/dj/configuration",
            Some(&body),
            |saved: &DjConfiguredDto| saved.done.clone(),
        ) {
            return answer;
        }
        respond(|| {
            let saved = self.surface.dj_configure(&self.client, &request)?;
            Ok((saved.done.clone(), saved))
        })
    }
}

#[tool(
    description = "Read the owner's reusable DJ moods and automatic daily programs, their exact revision, and effective/factory definitions. The editable config contains only mood overrides; programs null uses built-in defaults, [] disables automatic moods, and an array replaces them in order. Read this before editing with dj_configure. No speaker or Spotify operation occurs.",
    annotations(read_only, idempotent)
)]
async fn dj_configuration(ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, Backend::dj_configuration).await
}

#[tool(
    description = "Replace reusable DJ mood overrides and automatic daily programs for the owner. First read dj_configuration; copy its editable config, make the requested changes, and pass that read's exact revision as expected_revision. All stored mood filter fields are preserved, including song_keywords, strict, and expires_at. Removing a built-in override restores its factory definition; removing a custom mood is refused while preferences, unexpired steering, saved scenes, or schedules still refer to it. Programs null restores defaults; [] disables automatic moods; ordered entries have days mon..sun, from/to HH:MM and a mood. Overnight windows belong to their starting day and the first matching entry wins. Saves affect future picks and preserve playback and queued music. On CONFIG_CHANGED or an uncertain result, read again and reconcile; never automatically retry or discard the revision."
)]
async fn dj_configure(
    ctx: &McpContext,
    expected_revision: String,
    config: DjConfigDocument,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let request = DjConfigureRequest {
        expected_revision,
        config,
    };
    with_backend_off_thread(ctx, move |backend| backend.dj_configure(&request)).await
}

#[cfg(test)]
mod tests;
