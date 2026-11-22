//! Review a smart target, then play or queue on that exact physical audience.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::players::targets::ResolvedRoomsDto;
use fsonos_api::surface::queue::QueueAddOutcome;
use fsonos_api::surface::source_control::{
    ReviewedSource, ReviewedSourceMode, ReviewedSourceOutcome, ReviewedSourceRequest,
};
use fsonos_api::{ErrorCode, Failure, OutcomeDto, ZoneRequest};

use super::{Backend, from_daemon, respond, segment, sentence, with_backend_off_thread};

fn resolution_text(view: &ResolvedRoomsDto) -> String {
    let selected = view
        .rooms
        .iter()
        .map(|room| format!("{:?} [{}] ({})", room.room, room.household, room.player))
        .collect::<Vec<_>>()
        .join(", ");
    let groups = view
        .groups
        .iter()
        .map(|group| {
            let members = group
                .members
                .iter()
                .map(|room| format!("{:?} ({})", room.room, room.player))
                .collect::<Vec<_>>()
                .join(", ");
            format!(
                "{}: coordinator {:?} ({}); members: {members}",
                group.household, group.coordinator_room, group.coordinator,
            )
        })
        .collect::<Vec<_>>()
        .join("\n");
    format!(
        "Target {:?} ({}) selects {} room(s) in {} existing group(s).\nRevision: {}\nSelected rooms: {selected}\nAffected groups:\n{groups}\nUse this exact target and revision with control_source after reviewing every affected group. The revision grants no permission and does not reserve the speakers.",
        view.target,
        view.matched,
        view.rooms.len(),
        view.groups.len(),
        view.revision,
    )
}

fn source(
    source_uri: Option<String>,
    favorite: Option<String>,
    playlist: Option<String>,
    title: Option<String>,
) -> Result<ReviewedSource, Failure> {
    match (source_uri, favorite, playlist) {
        (Some(source_uri), None, None) => Ok(ReviewedSource::Uri { source_uri, title }),
        (None, Some(favorite), None) if title.is_none() => {
            Ok(ReviewedSource::Favorite { favorite })
        }
        (None, None, Some(playlist)) if title.is_none() => {
            Ok(ReviewedSource::Playlist { playlist })
        }
        (None, Some(_), None) | (None, None, Some(_)) => Err(Failure::invalid(
            "title is only valid with source_uri; native sources retain their own metadata",
        )),
        _ => Err(Failure::invalid(
            "give exactly one of source_uri, favorite or playlist",
        )),
    }
}

fn checked_mode(value: &str) -> Result<ReviewedSourceMode, Failure> {
    match value {
        "play" => Ok(ReviewedSourceMode::Play),
        "next" => Ok(ReviewedSourceMode::Next),
        "append" => Ok(ReviewedSourceMode::Append),
        _ => Err(Failure::invalid("mode must be play, next or append")),
    }
}

fn outcome_text(outcome: &ReviewedSourceOutcome) -> (String, bool) {
    match outcome {
        ReviewedSourceOutcome::Play(played) => (sentence(played), played.complete()),
        ReviewedSourceOutcome::Queue(queued) => {
            let complete = match queued {
                QueueAddOutcome::Single(_) => true,
                QueueAddOutcome::Groups(batch) => {
                    batch.complete && batch.failed.is_empty() && !batch.added.is_empty()
                }
            };
            (queued.text(), complete)
        }
    }
}

impl Backend {
    /// Observe aliases and complete groups on the same authority that will
    /// execute the reviewed source. Legacy clients also receive the revision.
    pub fn resolve_rooms(&self, target: &str) -> McpResult<FinalCallToolResult> {
        let request = ZoneRequest {
            zone: target.into(),
        };
        let target = request
            .zone()
            .map_err(|failure| crate::tool_error(&failure))?;
        let path = format!("/rooms/resolve?target={}", segment(target));
        respond(|| {
            let view = self.read(&path, || self.surface.resolve_rooms(&self.client, target))?;
            Ok((resolution_text(&view), view))
        })
    }

    /// A selected daemon is authoritative. Decode the receipt for the requested
    /// mode; neither a failed reply nor a mismatched shape permits local replay.
    pub fn control_source(
        &self,
        request: &ReviewedSourceRequest,
    ) -> McpResult<FinalCallToolResult> {
        request
            .validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        let body = serde_json::to_value(request).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        let mut complete = false;
        let mut result = respond(|| {
            let outcome = match self
                .daemon
                .as_ref()
                .and_then(|daemon| daemon.call("POST", "/sources/control", Some(&body)))
            {
                Some(answer) => match request.mode {
                    ReviewedSourceMode::Play => {
                        ReviewedSourceOutcome::Play(from_daemon::<OutcomeDto>(
                            answer,
                            "POST",
                            "/sources/control",
                        )?)
                    }
                    ReviewedSourceMode::Next | ReviewedSourceMode::Append => {
                        ReviewedSourceOutcome::Queue(from_daemon::<QueueAddOutcome>(
                            answer,
                            "POST",
                            "/sources/control",
                        )?)
                    }
                },
                None => self
                    .surface
                    .control_reviewed_source(&self.client, request)?,
            };
            let (text, succeeded) = outcome_text(&outcome);
            complete = succeeded;
            Ok((text, outcome))
        })?;
        // Keep the full receipt. The common worker preserves it in both MCP
        // protocol eras when an incomplete result needs the tool-error flag.
        result.is_error = !complete;
        Ok(result)
    }
}

#[tool(
    description = "Resolve a room, player id, alias, here, all or everywhere before reviewed source playback. Qualify targets with @S1 or @S2 when needed. Returns the exact target, an opaque revision, selected physical room identities and every affected existing group, including members not explicitly named by an alias. Read every group's current state if needed, then pass this target and revision unchanged to control_source. A revision observes identities and membership; it grants no permission and does not reserve the speakers. Requires resolve_rooms, list_rooms and list_zones read permissions. Uses the selected daemon's inventory and aliases without sending playback commands.",
    annotations(read_only, idempotent)
)]
async fn resolve_rooms(
    ctx: &McpContext,
    target: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| backend.resolve_rooms(&target)).await
}

#[tool(
    description = "Play or queue one source in the exact audience reviewed with resolve_rooms. Pass its target as zone and its revision as REQUIRED expected_revision; never invent, drop or silently refresh the revision. mode is play (start/replace playback), next (insert after the current queue track), or append (add at the queue end); queue modes preserve playback. Give exactly one of source_uri, favorite or playlist. source_uri accepts a Spotify URI/link or, for play only, a renderer/stream URI; title is optional for URI sources only. favorite and playlist use each household's own Sonos catalog; prefer an unambiguous shared title across households, since FV:/SQ: ids are household-local. Artist/show links require Spotify catalog sign-in. Existing groups stay independent and grouped rooms act once. A changed target or physical group is refused; inspect again before a new reviewed request. Permissions, caps, logging and verified undo follow the existing play/play_favorite/add_to_queue operations. Inspect every per-group result even on a tool error: confirmed or uncertain writes may already exist. Never automatically replay an incomplete, lost or malformed response or switch to ordinary playback to bypass its review."
)]
#[allow(clippy::too_many_arguments)] // explicit source alternatives and review fields on the tool
async fn control_source(
    ctx: &McpContext,
    zone: String,
    expected_revision: String,
    mode: String,
    source_uri: Option<String>,
    favorite: Option<String>,
    playlist: Option<String>,
    title: Option<String>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let request = ReviewedSourceRequest {
        zone,
        expected_revision,
        mode: checked_mode(&mode).map_err(|failure| crate::tool_error(&failure))?,
        source: source(source_uri, favorite, playlist, title)
            .map_err(|failure| crate::tool_error(&failure))?,
    };
    with_backend_off_thread(ctx, move |backend| backend.control_source(&request)).await
}

#[cfg(test)]
mod tests;
