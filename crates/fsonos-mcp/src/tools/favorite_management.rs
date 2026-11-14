//! Native favorite authoring forwards one reviewed operation to the selected daemon.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::favorite_management::{
    self, InspectFavoriteRequest, ManagedFavoriteDto, RemoveFavoriteRequest, RenameFavoriteRequest,
    SaveFavoriteRequest,
};
use fsonos_api::{ErrorCode, Failure};
use serde::Serialize;

use super::{Backend, respond, with_backend_off_thread};

impl Backend {
    pub fn current_favorite(&self, zone: &str) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let path = favorite_management::current_path(zone)?;
            let observed =
                self.read(&path, || self.surface.current_favorite(&self.client, zone))?;
            Ok((observed.text(), observed))
        })
    }

    pub fn inspect_favorite(&self, req: &InspectFavoriteRequest) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let path = req.path()?;
            let observed =
                self.read(&path, || self.surface.inspect_favorite(&self.client, req))?;
            Ok((observed.text(), observed))
        })
    }

    pub fn save_favorite(&self, req: &SaveFavoriteRequest) -> McpResult<FinalCallToolResult> {
        req.validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        self.favorite_change("/favorites/save", req, || {
            self.surface.save_favorite(&self.client, req)
        })
    }

    pub fn rename_favorite(&self, req: &RenameFavoriteRequest) -> McpResult<FinalCallToolResult> {
        req.validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        self.favorite_change("/favorites/rename", req, || {
            self.surface.rename_favorite(&self.client, req)
        })
    }

    pub fn remove_favorite(&self, req: &RemoveFavoriteRequest) -> McpResult<FinalCallToolResult> {
        req.validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        self.favorite_change("/favorites/remove", req, || {
            self.surface.remove_favorite(&self.client, req)
        })
    }

    fn favorite_change(
        &self,
        path: &str,
        req: &impl Serialize,
        local: impl FnOnce() -> Result<ManagedFavoriteDto, Failure>,
    ) -> McpResult<FinalCallToolResult> {
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(result) = self.on_daemon::<ManagedFavoriteDto>(
            "POST",
            path,
            Some(&body),
            ManagedFavoriteDto::text,
        ) {
            return result;
        }
        respond(|| {
            let result = local()?;
            Ok((result.text(), result))
        })
    }
}

#[tool(
    description = "Preview the current native track or station in a room for saving to My Sonos. Returns favorite metadata and an opaque source revision; favorite.id is empty because nothing was saved. A queue source selects its exact current track occurrence; a radio source selects the station, not the transient song. Live inputs and group/queue URLs cannot become favorites. No speaker writes or Spotify lookup. Uses list_favorites and get_zone_state read permissions. Pass revision to save_favorite on this same household/group after reviewing the title.",
    annotations(read_only, idempotent)
)]
async fn current_favorite(
    ctx: &McpContext,
    zone: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| backend.current_favorite(&zone)).await
}

#[tool(
    description = "Inspect one exact native My Sonos favorite before renaming or removing it. Give zone in its household (Room@S1 / Room@S2 disambiguates room names) and favorite as the FV:2/<n> id from list_favorites; names and numbered positions are not accepted. Returns the native entry and its opaque revision. No playback changes. Pass this exact identity and revision to rename_favorite or remove_favorite on the same household/group. Uses list_favorites read permission.",
    annotations(read_only, idempotent)
)]
async fn inspect_favorite(
    ctx: &McpContext,
    zone: String,
    favorite: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| {
        backend.inspect_favorite(&InspectFavoriteRequest { zone, favorite })
    })
    .await
}

#[tool(
    description = "Save the reviewed current native track or station as a My Sonos favorite in this room's household. First call current_favorite and review its actual source, then supply that revision as expected_revision and a nonblank title (no controls, at most 1023 UTF-8 bytes after trimming). The daemon retains the speaker's renderer metadata, rechecks the source, selected occurrence, queue and group, sends one CreateObject, and confirms the new native id. Does not save a whole queue, change playback, volume or DJ ownership. Existing duplicate content or a full catalog can be refused by Sonos. Uses save_favorite / house-config permission. Not undoable through action history. Never automatically retry a lost/uncertain reply; inspect list_favorites in the original household first."
)]
async fn save_favorite(
    ctx: &McpContext,
    zone: String,
    expected_revision: String,
    title: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| {
        backend.save_favorite(&SaveFavoriteRequest {
            zone,
            expected_revision,
            title,
        })
    })
    .await
}

#[tool(
    description = "Rename one reviewed My Sonos favorite while preserving its native identity and playback metadata. First call inspect_favorite in this household/group, then pass its exact favorite.id and revision as expected_revision. title must be nonblank, contain no controls and fit 1023 UTF-8 bytes after trimming. A changed native entry or group is refused; a same-title request is a checked no-op. Playback, queue, levels and DJ continue. Uses rename_favorite / house-config permission; not undoable through action history. Do not automatically retry an uncertain reply: read the original household's favorites before another edit."
)]
async fn rename_favorite(
    ctx: &McpContext,
    zone: String,
    favorite: String,
    expected_revision: String,
    title: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| {
        backend.rename_favorite(&RenameFavoriteRequest {
            zone,
            favorite,
            expected_revision,
            title,
        })
    })
    .await
}

#[tool(
    description = "Remove one exact reviewed My Sonos favorite from its household. First inspect_favorite and retain the favorite.id (FV:2/<n>) and revision as expected_revision. Names and catalog numbers cannot identify a deletion; a changed entry or group is refused before the write. A fresh catalog confirms absence. This removes the shortcut, not the music, playing source or queued copies. Uses remove_favorite / house-config permission; cannot be undone through action history. Never automatically retry an uncertain response: read list_favorites in the original household before another edit."
)]
async fn remove_favorite(
    ctx: &McpContext,
    zone: String,
    favorite: String,
    expected_revision: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| {
        backend.remove_favorite(&RemoveFavoriteRequest {
            zone,
            favorite,
            expected_revision,
        })
    })
    .await
}

#[cfg(test)]
mod tests;
