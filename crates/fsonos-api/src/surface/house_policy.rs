//! The house policy as the surfaces show it (`get_policy`, `GET /policy`,
//! `fsonos policy show`): the effective limits, quiet hours, per-room caps
//! and per-client rules from core's `Policy::view`, and who the caller is
//! under it.

use fsonos_core::policy::{Client, Policy, PolicyView};
use serde::Serialize;
use std::fmt::Write as _;

use super::Surface;
use crate::failure::Failure;

/// The effective policy, and the caller under it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct PolicyDto {
    /// The caller, as the policy names it: `cli`, `mcp-stdio`,
    /// `loopback-http`, a tailnet login, or `unknown`.
    pub you: String,
    /// Whether the volume caps apply to the caller.
    pub you_are_capped: bool,
    #[serde(flatten)]
    pub policy: PolicyView,
}

impl PolicyDto {
    /// `policy` as `client` is subject to it.
    #[must_use]
    pub fn of(policy: &Policy, client: &Client) -> Self {
        Self {
            you: client.key().to_owned(),
            you_are_capped: policy.is_capped(client),
            policy: policy.view(),
        }
    }

    /// The policy in a few lines.
    #[must_use]
    pub fn text(&self) -> String {
        let p = &self.policy;
        let mut text = format!(
            "Defaults: rooms up to {}, steps of at most {}, fades of {} s.\n",
            p.defaults.max_volume, p.defaults.max_step, p.defaults.fade_secs
        );
        match &p.quiet_hours {
            Some(q) => {
                let _ = writeln!(
                    text,
                    "Quiet hours: {}–{}, rooms up to {}.",
                    q.start, q.end, q.max_volume
                );
            }
            None => text.push_str("Quiet hours: none.\n"),
        }
        for room in &p.rooms {
            let mut limits = Vec::new();
            if let Some(max) = room.max_volume {
                limits.push(format!("up to {max}"));
            }
            if let Some(step) = room.max_step {
                limits.push(format!("steps of at most {step}"));
            }
            let _ = writeln!(text, "Room {}: {}.", room.room, limits.join(", "));
        }
        for client in &p.clients {
            let mut rules = Vec::new();
            if let Some(allow) = &client.allow {
                rules.push(format!("only {}", list(allow)));
            }
            if !client.deny.is_empty() {
                rules.push(format!("never {}", client.deny.join(", ")));
            }
            match client.capped {
                Some(true) => rules.push("capped".to_owned()),
                Some(false) => rules.push("uncapped".to_owned()),
                None => {}
            }
            let _ = writeln!(text, "Client {}: {}.", client.client, rules.join("; "));
        }
        let _ = writeln!(
            text,
            "You ({}): {}.",
            self.you,
            if self.you_are_capped {
                "the volume caps apply"
            } else {
                "uncapped"
            }
        );
        text
    }
}

/// `allow` as words: "no tools" when empty.
fn list(tools: &[String]) -> String {
    if tools.is_empty() {
        "no tools".to_owned()
    } else {
        tools.join(", ")
    }
}

impl Surface {
    /// The effective house policy and the caller under it (`get_policy`).
    pub fn policy_view(&self, client: &Client) -> Result<PolicyDto, Failure> {
        self.guard(client).authorize("get_policy", true)?;
        Ok(PolicyDto::of(&self.policy, client))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const POLICY: &str = r#"
[defaults]
max_volume = 60
max_step = 15

[quiet_hours]
start = "22:00"
end = "07:00"
max_volume = 25

[rooms."Ada’s Studio"]
max_volume = 40

[clients."agent@example.com"]
allow = ["list_zones", "set_volume"]
deny = ["dj_start"]
capped = true
"#;

    #[test]
    fn the_policy_reads_as_lines_and_names_the_caller() {
        let policy = Policy::from_toml(POLICY).unwrap();
        let shown = PolicyDto::of(&policy, &Client::Cli);
        assert_eq!(shown.you, "cli");
        assert!(!shown.you_are_capped, "the CLI is uncapped by default");
        assert_eq!(
            shown.text(),
            "Defaults: rooms up to 60, steps of at most 15, fades of 0 s.\n\
             Quiet hours: 22:00–07:00, rooms up to 25.\n\
             Room Ada’s Studio: up to 40.\n\
             Client agent@example.com: only list_zones, set_volume; never dj_start; capped.\n\
             You (cli): uncapped.\n"
        );
        let agent = PolicyDto::of(&policy, &Client::Tailnet("agent@example.com".into()));
        assert!(agent.you_are_capped);
        let json = serde_json::to_value(&agent).unwrap();
        assert_eq!(json["you"], "agent@example.com");
        assert_eq!(json["defaults"]["max_volume"], 60);
        assert_eq!(json["rooms"][0]["max_volume"], 40);
        assert_eq!(json["clients"][0]["deny"], serde_json::json!(["dj_start"]));
    }

    #[test]
    fn the_defaults_have_no_quiet_hours() {
        let shown = PolicyDto::of(&Policy::default(), &Client::Unknown);
        assert!(shown.text().contains("Quiet hours: none."));
        assert!(
            shown
                .text()
                .ends_with("You (unknown): the volume caps apply.\n")
        );
    }
}
