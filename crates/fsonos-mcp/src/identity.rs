//! Transport-authenticated MCP callers and resource subscription policy.
//! HTTP admission owns authentication; tool arguments and request metadata do
//! not participate in identity resolution.

use fastmcp::prelude::*;
use fastmcp::{FinalSubscriptionsListenParams, JsonRpcRequest};
use fsonos_core::policy::{self, Client, McpProfile};

use crate::{tool_error, tools};

/// Native MCP never grants the owner's uncapped CLI or stdio-bridge identity.
pub(crate) fn auth(caller: Client) -> AuthContext {
    let caller = match caller {
        Client::Cli | Client::McpStdio => Client::Unknown,
        Client::Tailnet(name) => match client(Some(&name)) {
            caller @ Client::Tailnet(_) => caller,
            _ => Client::Unknown,
        },
        other => other,
    };
    AuthContext::with_subject(caller.key())
}

pub(crate) fn client(subject: Option<&str>) -> Client {
    let Some(subject) = subject else {
        return Client::Unknown;
    };
    match subject {
        "loopback-http" => Client::LoopbackHttp,
        "unknown" => Client::Unknown,
        name if !name.trim().is_empty()
            && name == name.trim()
            && name.len() <= 256
            && !name.chars().any(char::is_control)
            && !policy::reserved_key(name) =>
        {
            Client::Tailnet(name.to_string())
        }
        _ => Client::Unknown,
    }
}

/// Invalidations reveal that a resource changed. Authorize their event feed and
/// the document's reads before either protocol era registers a subscription.
pub(crate) struct ResourcePolicy;

impl Middleware for ResourcePolicy {
    fn on_request(
        &self,
        ctx: &McpContext,
        request: &JsonRpcRequest,
    ) -> McpResult<MiddlewareDecision> {
        let uris = match request.method.as_str() {
            "resources/subscribe" => request
                .params
                .as_ref()
                .and_then(|params| params.get("uri"))
                .and_then(serde_json::Value::as_str)
                .map(|uri| vec![uri.to_string()])
                .unwrap_or_default(),
            "subscriptions/listen" => {
                let params: FinalSubscriptionsListenParams =
                    serde_json::from_value(request.params.clone().ok_or_else(|| {
                        McpError::invalid_params("subscriptions/listen requires parameters")
                    })?)
                    .map_err(|error| McpError::invalid_params(error.to_string()))?;
                params
                    .notifications
                    .resource_subscriptions
                    .unwrap_or_default()
            }
            _ => return Ok(MiddlewareDecision::Continue),
        };
        let mut reads = Vec::new();
        for uri in uris {
            let required: &[&str] = match uri.as_str() {
                "sonos://zones" => &["events", "list_zones"],
                "sonos://dj" => &["events", "list_zones", "dj_status", "dj_moods"],
                uri if uri.starts_with("sonos://zones/") => &["events", "get_zone_state"],
                _ => continue,
            };
            reads.extend_from_slice(required);
        }
        if !reads.is_empty() {
            reads.sort_unstable();
            reads.dedup();
            tools::backend_for(ctx)
                .and_then(|backend| backend.authorize_reads(&reads))
                .map_err(|error| tool_error(&error))?;
        }
        Ok(MiddlewareDecision::Continue)
    }

    /// Shape `tools/list` to the caller's [`McpProfile`]: an `essentials`
    /// client is offered only [`crate::ESSENTIALS`], so an agent over the
    /// serve listener does not pay for every tool's schema. Policy still
    /// governs tool *use*; this only trims what is listed.
    fn on_response(
        &self,
        ctx: &McpContext,
        request: &JsonRpcRequest,
        response: serde_json::Value,
    ) -> McpResult<serde_json::Value> {
        if request.method != "tools/list" {
            return Ok(response);
        }
        let essentials = tools::backend_for(ctx)
            .is_ok_and(|backend| backend.mcp_profile() == Some(McpProfile::Essentials));
        Ok(if essentials {
            essentials_only(response)
        } else {
            response
        })
    }
}

/// `response` (a `tools/list` result) with only the [`crate::ESSENTIALS`]
/// tools kept. A result without a `tools` array is returned unchanged.
fn essentials_only(mut response: serde_json::Value) -> serde_json::Value {
    if let Some(tools) = response
        .get_mut("tools")
        .and_then(serde_json::Value::as_array_mut)
    {
        tools.retain(|tool| {
            tool.get("name")
                .and_then(serde_json::Value::as_str)
                .is_some_and(|name| crate::ESSENTIALS.contains(&name))
        });
    }
    response
}

#[cfg(test)]
mod profile_tests {
    use super::essentials_only;
    use serde_json::json;

    #[test]
    fn essentials_only_keeps_the_curated_tools_and_drops_the_rest() {
        let listed = json!({"tools": [
            {"name": "list_zones"},
            {"name": "announce"},
            {"name": "play"},
            {"name": "set_eq"},
        ]});
        let trimmed = essentials_only(listed);
        let names: Vec<&str> = trimmed["tools"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        assert_eq!(names, ["list_zones", "play"], "{trimmed}");
    }

    #[test]
    fn a_result_without_tools_is_unchanged() {
        let other = json!({"nextCursor": "x"});
        assert_eq!(essentials_only(other.clone()), other);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_subjects_never_become_cli_or_stdio() {
        for name in [
            "cli",
            "CLI",
            "mcp-stdio",
            "MCP-STDIO",
            " cli ",
            "",
            " ",
            "reader\n",
            "bridge-compat",
            "dj",
            "DJ",
            "schedule",
            "sleep",
            "selftest",
            "setup",
        ] {
            assert_eq!(client(Some(name)), Client::Unknown, "{name:?}");
        }
        assert_eq!(client(None), Client::Unknown);
        assert_eq!(client(Some(&"x".repeat(257))), Client::Unknown);
        for caller in [
            Client::Cli,
            Client::McpStdio,
            Client::Tailnet("cli".into()),
            Client::Tailnet("loopback-http".into()),
            Client::Tailnet("dj".into()),
        ] {
            assert_eq!(auth(caller).subject.as_deref(), Some("unknown"));
        }
    }

    #[test]
    fn named_and_local_callers_keep_distinct_policy_keys() {
        for caller in [
            Client::Unknown,
            Client::LoopbackHttp,
            Client::Tailnet("alice@example.com".into()),
            Client::Tailnet("tag:music-agent".into()),
            Client::Tailnet("music-agent".into()),
        ] {
            assert_eq!(client(auth(caller.clone()).subject.as_deref()), caller);
        }
    }
}
