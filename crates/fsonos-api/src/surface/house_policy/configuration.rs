//! Owner-reviewed changes to the daemon's persisted house policy.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::configuration::{self, ConfigurationError, PolicyEdit, PolicySnapshot};
use fsonos_core::policy::{Client, Decision, McpProfile, Policy, PolicyView, ToolCall};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

use crate::surface::Surface;
use crate::{ErrorCode, Failure};

/// The complete saved policy, visible only to the owner or an administrator.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyConfigurationDto {
    pub revision: String,
    #[serde(flatten)]
    pub policy: PolicyView,
}

impl From<PolicySnapshot> for PolicyConfigurationDto {
    fn from(snapshot: PolicySnapshot) -> Self {
        Self {
            revision: snapshot.revision,
            policy: snapshot.policy.view(),
        }
    }
}

/// Replace one complete rule; unrelated policy entries are retained.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PolicyConfigurationChange {
    SetDefaults {
        max_volume: u8,
        max_step: u8,
        fade_secs: u32,
    },
    SetQuietHours {
        start: String,
        end: String,
        max_volume: u8,
    },
    // A struct variant makes deny_unknown_fields apply here too. Serde's
    // internally tagged unit variants otherwise discard extra fields.
    ClearQuietHours {},
    SetRoom {
        room: String,
        max_volume: Option<u8>,
        max_step: Option<u8>,
    },
    RemoveRoom {
        room: String,
    },
    SetClient {
        client: String,
        allow: Option<Vec<String>>,
        deny: Vec<String>,
        grant: Vec<String>,
        capped: Option<bool>,
        manage_others: bool,
        mcp_profile: Option<McpProfile>,
    },
    RemoveClient {
        client: String,
    },
}

impl PolicyConfigurationChange {
    /// Validate before configuration reads, discovery, or any mutation.
    pub fn validate(&self) -> Result<(), Failure> {
        self.edit().validate().map_err(config_failure)
    }

    fn edit(&self) -> PolicyEdit {
        match self {
            Self::SetDefaults {
                max_volume,
                max_step,
                fade_secs,
            } => PolicyEdit::SetDefaults {
                max_volume: *max_volume,
                max_step: *max_step,
                fade_secs: *fade_secs,
            },
            Self::SetQuietHours {
                start,
                end,
                max_volume,
            } => PolicyEdit::SetQuietHours {
                start: start.clone(),
                end: end.clone(),
                max_volume: *max_volume,
            },
            Self::ClearQuietHours {} => PolicyEdit::ClearQuietHours,
            Self::SetRoom {
                room,
                max_volume,
                max_step,
            } => PolicyEdit::SetRoom {
                room: room.clone(),
                max_volume: *max_volume,
                max_step: *max_step,
            },
            Self::RemoveRoom { room } => PolicyEdit::RemoveRoom { room: room.clone() },
            Self::SetClient {
                client,
                allow,
                deny,
                grant,
                capped,
                manage_others,
                mcp_profile,
            } => PolicyEdit::SetClient {
                client: client.clone(),
                allow: allow.clone(),
                deny: deny.clone(),
                grant: grant.clone(),
                capped: *capped,
                manage_others: *manage_others,
                mcp_profile: *mcp_profile,
            },
            Self::RemoveClient { client } => PolicyEdit::RemoveClient {
                client: client.clone(),
            },
        }
    }

    /// External request schemas use the exact serde-tagged wire form.
    #[must_use]
    pub fn json_schema() -> serde_json::Value {
        serde_json::to_value(<Self as fastapi_openapi::JsonSchema>::schema())
            .expect("policy configuration schema is JSON")
    }

    fn description(&self) -> String {
        match self {
            Self::SetDefaults { .. } => "saved default house limits".into(),
            Self::SetQuietHours { .. } => "saved quiet hours".into(),
            Self::ClearQuietHours {} => "cleared quiet hours".into(),
            Self::SetRoom { room, .. } => format!("saved limits for room {room:?}"),
            Self::RemoveRoom { room } => format!("removed limits for room {room:?}"),
            Self::SetClient { client, .. } => format!("saved policy rules for client {client:?}"),
            Self::RemoveClient { client } => format!("removed policy rules for client {client:?}"),
        }
    }
}

impl fastapi_openapi::JsonSchema for PolicyConfigurationChange {
    fn schema() -> fastapi_openapi::Schema {
        use fastapi_openapi::Schema;
        Schema::one_of(vec![
            change_schema(
                "set_defaults",
                &["kind", "max_volume", "max_step", "fade_secs"],
                [
                    ("max_volume", u8::schema()),
                    ("max_step", u8::schema()),
                    ("fade_secs", u32::schema()),
                ],
            ),
            change_schema(
                "set_quiet_hours",
                &["kind", "start", "end", "max_volume"],
                [
                    ("start", String::schema()),
                    ("end", String::schema()),
                    ("max_volume", u8::schema()),
                ],
            ),
            change_schema("clear_quiet_hours", &["kind"], []),
            change_schema(
                "set_room",
                &["kind", "room"],
                [
                    ("room", String::schema()),
                    ("max_volume", Option::<u8>::schema()),
                    ("max_step", Option::<u8>::schema()),
                ],
            ),
            change_schema(
                "remove_room",
                &["kind", "room"],
                [("room", String::schema())],
            ),
            change_schema(
                "set_client",
                &["kind", "client", "deny", "grant", "manage_others"],
                [
                    ("client", String::schema()),
                    ("allow", Option::<Vec<String>>::schema()),
                    ("deny", Vec::<String>::schema()),
                    ("grant", Vec::<String>::schema()),
                    ("capped", Option::<bool>::schema()),
                    ("manage_others", bool::schema()),
                    (
                        "mcp_profile",
                        Schema::string_enum(vec!["essentials".into(), "full".into()]).nullable(),
                    ),
                ],
            ),
            change_schema(
                "remove_client",
                &["kind", "client"],
                [("client", String::schema())],
            ),
        ])
    }

    fn schema_name() -> Option<&'static str> {
        Some("PolicyConfigurationChange")
    }
}

fn change_schema<const N: usize>(
    kind: &str,
    required: &[&str],
    fields: [(&str, fastapi_openapi::Schema); N],
) -> fastapi_openapi::Schema {
    use fastapi_openapi::{ObjectSchema, Schema};
    let mut properties: HashMap<_, _> = fields
        .into_iter()
        .map(|(name, schema)| (name.to_owned(), schema))
        .collect();
    properties.insert(
        "kind".to_owned(),
        Schema::string_enum(vec![kind.to_owned()]),
    );
    Schema::Object(ObjectSchema {
        title: None,
        description: None,
        properties,
        required: required.iter().map(|name| (*name).to_owned()).collect(),
        additional_properties: Some(Box::new(Schema::Boolean(false))),
    })
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PolicyConfigureRequest {
    pub expected_revision: String,
    pub change: PolicyConfigurationChange,
}

impl PolicyConfigureRequest {
    /// Validate the reviewed revision and authored values before a read/write.
    pub fn validate(&self) -> Result<(), Failure> {
        if self.expected_revision.trim().is_empty()
            || self.expected_revision.len() > 4096
            || self.expected_revision.chars().any(char::is_control)
        {
            return Err(Failure::invalid(
                "read the policy configuration and supply its revision",
            ));
        }
        self.change.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyConfiguredDto {
    pub done: String,
    pub changed: bool,
    pub configuration: PolicyConfigurationDto,
}

fn config_failure(error: ConfigurationError) -> Failure {
    let code = match &error {
        ConfigurationError::Conflict { .. } => ErrorCode::ConfigChanged,
        ConfigurationError::Invalid(_) | ConfigurationError::Parse(_) => ErrorCode::InvalidArgument,
        ConfigurationError::Denied(_) => ErrorCode::PolicyDenied,
        ConfigurationError::Publish {
            published: true, ..
        } => ErrorCode::OutcomeUncertain,
        _ => ErrorCode::Internal,
    };
    let failure = Failure::new(code, error.to_string());
    match code {
        ErrorCode::ConfigChanged | ErrorCode::OutcomeUncertain => failure.with_hint(
            "Read the daemon's policy configuration and review its current values before another edit. The write was not retried.",
        ),
        _ => failure,
    }
}

impl Surface {
    /// Read stored policy without LAN I/O. Client rules are administrative data.
    pub fn policy_configuration(&self, client: &Client) -> Result<PolicyConfigurationDto, Failure> {
        self.guard(client).authorize("policy_configuration", true)?;
        self.guard(client)
            .authorize_change("policy_configuration", "the house policy", None)?;
        let path = self.policy_configuration_path()?;
        let snapshot = configuration::read(&path).map_err(config_failure)?;
        // A concurrent file edit may have revoked this reader's administration.
        self.authorize_policy_snapshot(client, &snapshot.policy, true)
            .map_err(|reason| Failure::new(ErrorCode::PolicyDenied, reason))?;
        Ok(snapshot.into())
    }

    /// Apply one rule at its exact reviewed revision. Both tool permission and
    /// administration are checked again under the shared configuration lock.
    pub fn configure_policy(
        &self,
        client: &Client,
        request: &PolicyConfigureRequest,
    ) -> Result<PolicyConfiguredDto, Failure> {
        self.authorize_write(client, "configure_policy")?;
        self.authorize_change(client, "configure_policy", "the house policy", None)?;
        request.validate()?;
        let path = self.policy_configuration_path()?;
        let _restoring = self.log.as_ref().map(|log| {
            log.restoring
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let result = configuration::apply(
            &path,
            &request.expected_revision,
            &request.change.edit(),
            |current| self.authorize_policy_snapshot(client, current, false),
        )
        .map_err(config_failure)
        .map(|saved| PolicyConfiguredDto {
            done: if saved.changed {
                request.change.description()
            } else {
                "house policy unchanged".into()
            },
            changed: saved.changed,
            configuration: saved.snapshot.into(),
        });
        // Refresh through the existing publisher, which preserves a simultaneous
        // trusted owner update. A failed publish is observed, never replayed.
        let _ = self.policy();
        let barrier = result.as_ref().map_or_else(
            |error| error.code == ErrorCode::OutcomeUncertain,
            |saved| saved.changed,
        );
        self.log_kept_write(
            client,
            "configure_policy".into(),
            &result,
            |saved| saved.done.clone(),
            barrier.then_some("house policy edit"),
        );
        result
    }

    fn policy_configuration_path(&self) -> Result<std::path::PathBuf, Failure> {
        self.data_dir()
            .map(|dir| dir.join(fsonos_core::policy::FILE_NAME))
            .ok_or_else(|| {
                Failure::new(
                    ErrorCode::NotReady,
                    "this surface has no policy configuration file",
                )
                .with_hint("Use the running daemon or configure its data directory.")
            })
    }

    fn authorize_policy_snapshot(
        &self,
        client: &Client,
        saved: &Policy,
        read: bool,
    ) -> Result<(), String> {
        let owner = self
            .policy
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .owner()
            .map(str::to_owned);
        let current = saved.clone().with_owner(owner);
        let name = if read {
            "policy_configuration"
        } else {
            "configure_policy"
        };
        for decision in [
            current.authorize(
                ToolCall {
                    name,
                    read_only: read,
                },
                client,
            ),
            current.may_change(client, None),
        ] {
            if let Decision::Deny { reason } = decision {
                return Err(reason);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
