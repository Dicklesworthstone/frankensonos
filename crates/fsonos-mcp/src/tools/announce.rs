//! The `announce` tool: speak, chime or upload a WAV, then put the music back.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::announce::{AnnounceDto, AnnounceRequest, AnnouncementJobDto};

use super::{Backend, respond, with_backend_off_thread};

impl Backend {
    pub fn announce_start(&self, req: &AnnounceRequest) -> McpResult<FinalCallToolResult> {
        let body = serde_json::to_value(req).ok();
        if let Some(answer) = self.on_daemon(
            "POST",
            "/announcements",
            body.as_ref(),
            |job: &AnnouncementJobDto| job.done.clone(),
        ) {
            return answer;
        }
        respond(|| {
            let job = self.surface.start_announcement(&self.client, req)?;
            Ok((job.done.clone(), job))
        })
    }

    pub fn list_announcements(&self) -> McpResult<FinalCallToolResult> {
        #[derive(serde::Serialize)]
        struct JobsDto {
            announcements: Vec<AnnouncementJobDto>,
        }
        respond(|| {
            let jobs: Vec<AnnouncementJobDto> = self.read("/announcements", || {
                self.surface.list_announcements(&self.client)
            })?;
            let text = if jobs.is_empty() {
                "No retained announcements visible to this caller.\n".into()
            } else {
                jobs.iter().map(AnnouncementJobDto::text).collect()
            };
            Ok((
                text,
                JobsDto {
                    announcements: jobs,
                },
            ))
        })
    }

    pub fn get_announcement(&self, id: &str) -> McpResult<FinalCallToolResult> {
        let path = format!("/announcements/{}", super::segment(id));
        if let Some(answer) = self.on_daemon("GET", &path, None, |job: &AnnouncementJobDto| {
            job.done.clone()
        }) {
            return answer;
        }
        respond(|| {
            let job = self.surface.get_announcement(&self.client, id)?;
            Ok((job.done.clone(), job))
        })
    }

    pub fn cancel_announcement(&self, id: &str) -> McpResult<FinalCallToolResult> {
        let path = format!("/announcements/{}/cancel", super::segment(id));
        if let Some(answer) = self.on_daemon(
            "POST",
            &path,
            Some(&serde_json::json!({})),
            |job: &AnnouncementJobDto| job.done.clone(),
        ) {
            return answer;
        }
        respond(|| {
            let job = self.surface.cancel_announcement(&self.client, id)?;
            Ok((job.done.clone(), job))
        })
    }

    /// The `announce` tool: on the daemon when one runs (its media listener
    /// serves the clip to the players), else here.
    pub fn announce(&self, req: &AnnounceRequest) -> McpResult<FinalCallToolResult> {
        let body = serde_json::to_value(req).ok();
        if let Some(answer) =
            self.on_daemon("POST", "/announce", body.as_ref(), |a: &AnnounceDto| {
                a.done.clone()
            })
        {
            return answer;
        }
        respond(|| {
            let announced = self.surface.announce(&self.client, req)?;
            Ok((announced.done.clone(), announced))
        })
    }
}

#[tool(
    description = "Requires the daemon's media listener (fsonos serve). Give exactly one of `text` (local offline speech backend, at most 1000 characters; optional `voice`), `chime` (bell, beep, rise), or `wav_base64` (16-bit PCM WAV bytes as standard padded base64, at most 16 MiB decoded and five minutes; never a file path). Play it in `rooms` (names, aliases or 'all'; every room when omitted), then restore grouping, source, position, volumes and mute. `volume` 0-100, default 35, capped per room by the house policy. Waits until the clip has played and the music is back. Not undoable: there is nothing left to undo."
)]
async fn announce(
    ctx: &McpContext,
    text: Option<String>,
    chime: Option<String>,
    wav_base64: Option<String>,
    voice: Option<String>,
    rooms: Option<Vec<String>>,
    volume: Option<u32>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = AnnounceRequest {
        text,
        chime,
        wav_base64,
        voice,
        rooms: rooms.unwrap_or_default(),
        volume: volume.map(i64::from),
    };
    with_backend_off_thread(ctx, move |b| b.announce(&req)).await
}

#[tool(
    description = "Start an announcement in the daemon without holding this call through playback. Give the same one source and room/volume fields as announce. Returns a job id immediately after bounded admission; accepted does not mean heard or restored. Inspect get_announcement or list_announcements, and use cancel_announcement to cancel queued or active delivery. Existing per-client/global limits and quiet-hour caps apply. Never repeat an uncertain start: inspect the job list first. Jobs and recent receipts are process-local and expire after daemon restart or bounded retention."
)]
async fn announce_start(
    ctx: &McpContext,
    text: Option<String>,
    chime: Option<String>,
    wav_base64: Option<String>,
    voice: Option<String>,
    rooms: Option<Vec<String>>,
    volume: Option<u32>,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let req = AnnounceRequest {
        text,
        chime,
        wav_base64,
        voice,
        rooms: rooms.unwrap_or_default(),
        volume: volume.map(i64::from),
    };
    with_backend_off_thread(ctx, move |backend| backend.announce_start(&req)).await
}

#[tool(
    description = "Read up to 64 active/recent announcement jobs visible to this caller, newest first. Includes queued/preparing/playing/restoring stages, cancellation intent and final restoration receipts; no speech text or uploaded audio. Ordinary callers see their own jobs; the local CLI and configured owner may inspect all. Process-local ids retire after restart. Read-only; never starts or repeats playback.",
    annotations(read_only, idempotent)
)]
async fn list_announcements(ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, Backend::list_announcements).await
}

#[tool(
    description = "Read one retained announcement by its exact returned id. Cancellation requested is not completion: continue reading until completed, cancelled or failed and inspect the final restoration report. Read-only; unknown/expired or another caller's job returns NO_MATCH.",
    annotations(read_only, idempotent)
)]
async fn get_announcement(
    ctx: &McpContext,
    id: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| backend.get_announcement(&id)).await
}

#[tool(
    description = "Request cancellation of your exact announcement job, or any job when acting as the owner. Waiting work is released without speaker writes. Active playback is stopped only while the daemon still owns its announcement source/group, then normal restoration continues; manual changes are preserved. The returned status may still be playing/restoring. Read get_announcement until terminal and inspect not_restored. Repeating cancellation or cancelling a finished job never repeats Stop or playback."
)]
async fn cancel_announcement(
    ctx: &McpContext,
    id: String,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, move |backend| backend.cancel_announcement(&id)).await
}

#[cfg(test)]
mod tests;
