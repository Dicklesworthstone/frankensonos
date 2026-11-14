//! One revision-bound routine replacement; daemon failures never replay locally.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::schedules::{ScheduleDto, ScheduleRequest, UpdateScheduleRequest};
use fsonos_api::{ErrorCode, Failure};

use super::{Backend, respond, with_backend_off_thread};

impl Backend {
    pub fn update_schedule(&self, req: &UpdateScheduleRequest) -> McpResult<FinalCallToolResult> {
        req.validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        let body = serde_json::to_value(req).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(answer) = self.on_daemon(
            "POST",
            "/schedules/update",
            Some(&body),
            |updated: &ScheduleDto| format!("updated {}", updated.line()),
        ) {
            return answer;
        }
        respond(|| {
            let updated = self.surface.update_schedule(&self.client, req)?;
            Ok((format!("updated {}", updated.line()), updated))
        })
    }
}

fn request(
    id: i64,
    expected_revision: String,
    schedule: serde_json::Value,
) -> Result<UpdateScheduleRequest, Failure> {
    let schedule = serde_json::from_value::<ScheduleRequest>(schedule)
        .map_err(|error| Failure::invalid(format!("invalid replacement schedule: {error}")))?;
    let request = UpdateScheduleRequest {
        id,
        expected_revision,
        schedule,
    };
    request.validate()?;
    Ok(request)
}

#[tool(
    description = "Edit an existing routine in place without running its action. First call list_schedules, then pass the exact id and revision as expected_revision. schedule is a complete replacement definition using add_schedule fields: {when, action, zone?, scene?, mood?, direction?, volume?, fade_secs?, announcement?}. Actions are dj_start, pause, volume, apply_scene and announce; retain the existing direction or announcement object when changing only the time. Examples: {when:'weekdays 08:00',action:'dj_start',zone:'Kitchen@S1',mood:'bright'} or {when:'daily 22:00',action:'pause',zone:'Bedroom',fade_secs:30}. Preserves schedule id, original creator and enabled/paused state. New definitions affect future occurrences; an already claimed run may finish. Same-definition edits are validated no-ops. Both editor and original creator must be allowed the replacement action. Uses update_schedule / house-config permission. A stale revision, changed definition or intervening scheduler claim returns CONFIG_CHANGED; read and review again. A changed one-shot needs a future time. Not undoable through action history. Never automatically replay an uncertain response or fall back to a second store; inspect list_schedules on the selected daemon first."
)]
async fn update_schedule(
    ctx: &McpContext,
    id: i64,
    expected_revision: String,
    schedule: serde_json::Value,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let request =
        request(id, expected_revision, schedule).map_err(|failure| crate::tool_error(&failure))?;
    with_backend_off_thread(ctx, move |backend| backend.update_schedule(&request)).await
}

#[cfg(test)]
mod tests;
