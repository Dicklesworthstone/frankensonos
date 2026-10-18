//! Transport-authenticated MCP callers and resource subscription policy.
//! HTTP admission owns authentication; tool arguments and request metadata do
//! not participate in identity resolution.

use fastmcp::prelude::*;
use fastmcp::{FinalSubscriptionsListenParams, JsonRpcRequest};
use fsonos_core::policy::Client;

use crate::{tool_error, tools};

/// Native MCP never grants the owner's uncapped CLI or stdio-bridge identity.
#[cfg(test)]
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
            && !["cli", "mcp-stdio", "loopback-http", "unknown"]
                .iter()
                .any(|reserved| reserved.eq_ignore_ascii_case(name)) =>
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
