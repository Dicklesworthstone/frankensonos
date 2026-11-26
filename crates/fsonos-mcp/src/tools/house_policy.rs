//! Active policy and reviewed administration on the selected daemon.

use fastmcp::prelude::*;
use fastmcp::{CompleteResult, FinalCallToolResult};
use fsonos_api::surface::house_policy::{
    PolicyConfigurationChange, PolicyConfigurationDto, PolicyConfigureRequest, PolicyConfiguredDto,
};
use fsonos_api::{ErrorCode, Failure};

use super::{Backend, respond, with_backend_off_thread};

impl Backend {
    /// The `get_policy` tool.
    pub fn get_policy(&self) -> McpResult<FinalCallToolResult> {
        respond(|| {
            let policy = self.read("/policy", || self.surface.policy_view(&self.client))?;
            Ok((policy.text(), policy))
        })
    }

    pub fn policy_configuration(&self) -> McpResult<FinalCallToolResult> {
        if let Some(answer) = self.on_daemon("GET", "/policy/configuration", None, summary) {
            return answer;
        }
        respond(|| {
            let configuration = self.surface.policy_configuration(&self.client)?;
            Ok((summary(&configuration), configuration))
        })
    }

    pub fn configure_policy(
        &self,
        request: &PolicyConfigureRequest,
    ) -> McpResult<FinalCallToolResult> {
        request
            .validate()
            .map_err(|failure| crate::tool_error(&failure))?;
        let body = serde_json::to_value(request).map_err(|error| {
            crate::tool_error(&Failure::new(ErrorCode::Internal, error.to_string()))
        })?;
        if let Some(answer) = self.on_daemon(
            "POST",
            "/policy/configuration",
            Some(&body),
            |saved: &PolicyConfiguredDto| saved.done.clone(),
        ) {
            return answer;
        }
        respond(|| {
            let saved = self.surface.configure_policy(&self.client, request)?;
            Ok((saved.done.clone(), saved))
        })
    }
}

fn summary(configuration: &PolicyConfigurationDto) -> String {
    format!(
        "Policy revision {}: default volume cap {}, maximum increase {}, fade {} seconds; {} room overrides and {} client rules. Read the complete saved values before configure_policy.",
        configuration.revision,
        configuration.policy.defaults.max_volume,
        configuration.policy.defaults.max_step,
        configuration.policy.defaults.fade_secs,
        configuration.policy.rooms.len(),
        configuration.policy.clients.len(),
    )
}

#[tool(
    description = "The house policy in effect, and who you are under it: the default room volume cap and the largest single increase, the quiet-hours window and its lower cap, rooms with caps of their own, and clients with tool rules (only these tools, never these) or caps switched on or off. Check it before a volume change that might be clamped, or when a tool answers POLICY_DENIED.",
    annotations(read_only, idempotent)
)]
async fn get_policy(ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, Backend::get_policy).await
}

#[tool(
    description = "Read the selected daemon's complete saved house policy and its exact revision before configure_policy. Includes default volume/step/fade settings, quiet hours, every named room cap, and full client allow/deny/grant/capped/manage_others/MCP-profile rules. Requires owner or manage_others administration and read permission; get_policy remains the ordinary caller's view of active limits. A missing file returns built-in defaults. Invalid saved policy is reported rather than replaced. Sends no speaker command or discovery.",
    annotations(read_only, idempotent)
)]
async fn policy_configuration(ctx: &McpContext) -> McpResult<CompleteResult<FinalCallToolResult>> {
    with_backend_off_thread(ctx, Backend::policy_configuration).await
}

#[tool(
    description = "Edit one complete house-policy section or room/client rule on the selected daemon. Requires configure_policy (house-config) plus current owner or manage_others administration. First read policy_configuration and supply its exact revision as expected_revision. change is a tagged object: set_defaults {max_volume,max_step,fade_secs}, set_quiet_hours {start:'22:00',end:'07:00',max_volume}, clear_quiet_hours, set_room {room,max_volume?,max_step?}, remove_room {room}, set_client {client,allow?,deny:[],grant:[],capped?,manage_others:false,mcp_profile?}, or remove_client {client}. Include kind with the tag. Each set replaces the COMPLETE named rule; preserve every existing rule field you intend to retain. Optional null/omitted fields restore inheritance. allow:null permits tools normally; allow:[] permits none unless separately granted. deny wins over allow/grant. Volume and step limits are 0-100; quiet hours are differing local HH:MM times and may cross midnight. manage_others grants administration; never add it merely to bypass a denial. MCP profile is essentials or full. The current saved policy authorizes the edit under the file lock, so proposed grants cannot authorize themselves. Unrelated entries/comments are preserved. Identical settings are a verified no-op. Saved limits affect future operations and may revoke your own access; this sends no speaker command and has no playback-history undo. CONFIG_CHANGED or an uncertain/lost response requires a fresh read and review; never automatically retry or write to a fallback store."
)]
async fn configure_policy(
    ctx: &McpContext,
    expected_revision: String,
    change: PolicyConfigurationChange,
) -> McpResult<CompleteResult<FinalCallToolResult>> {
    let request = PolicyConfigureRequest {
        expected_revision,
        change,
    };
    request
        .validate()
        .map_err(|failure| crate::tool_error(&failure))?;
    with_backend_off_thread(ctx, move |backend| backend.configure_policy(&request)).await
}

#[cfg(test)]
mod tests;
