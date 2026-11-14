//! Edit the selected daemon's aliases and default targets without LAN I/O.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::HouseholdState;
use fsonos_core::policy::Client;
use fsonos_core::rooms::configuration::{self, AliasEdit, AliasesSnapshot, ConfigurationError};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap};

use super::Surface;
use crate::{ErrorCode, Failure};

/// An authored name for physical room selectors, including offline rooms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RoomAliasDto {
    pub name: String,
    pub rooms: Vec<String>,
}

/// An advisory from the available inventory, never a reason to discard a saved
/// offline target. Line zero describes unavailable inventory for the document.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RoomConfigWarningDto {
    pub line: usize,
    pub message: String,
}

/// The complete stored configuration, rather than only currently online rooms.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RoomConfigurationDto {
    pub revision: String,
    pub aliases: Vec<RoomAliasDto>,
    pub default_room: Option<String>,
    pub client_defaults: BTreeMap<String, String>,
    pub warnings: Vec<RoomConfigWarningDto>,
}

impl RoomConfigurationDto {
    fn from_snapshot(snapshot: AliasesSnapshot, households: Option<&[HouseholdState]>) -> Self {
        let warnings = households.map_or_else(
            || {
                vec![RoomConfigWarningDto {
                    line: 0,
                    message: "Room inventory is unavailable; saved targets were not checked."
                        .into(),
                }]
            },
            |households| {
                snapshot
                    .aliases
                    .check(households)
                    .into_iter()
                    .map(|warning| RoomConfigWarningDto {
                        line: warning.line,
                        message: warning.message,
                    })
                    .collect()
            },
        );
        Self {
            revision: snapshot.revision,
            aliases: snapshot
                .aliases
                .aliases()
                .iter()
                .map(|alias| RoomAliasDto {
                    name: alias.name.clone(),
                    rooms: alias.rooms.clone(),
                })
                .collect(),
            default_room: snapshot.aliases.global_default_room().map(str::to_owned),
            client_defaults: snapshot
                .aliases
                .client_defaults()
                .map(|(client, room)| (client.to_owned(), room.to_owned()))
                .collect(),
            warnings,
        }
    }
}

/// One change at the exact configuration revision the caller reviewed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RoomConfigurationChange {
    SetAlias {
        name: String,
        rooms: Vec<String>,
    },
    RemoveAlias {
        name: String,
    },
    SetDefault {
        room: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client: Option<String>,
    },
    ClearDefault {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        client: Option<String>,
    },
}

impl RoomConfigurationChange {
    /// Validate a change before selecting a daemon or reading its revision.
    pub fn validate(&self) -> Result<(), Failure> {
        self.edit().validate().map_err(config_failure)
    }

    fn edit(&self) -> AliasEdit {
        match self {
            Self::SetAlias { name, rooms } => AliasEdit::SetAlias {
                name: name.clone(),
                rooms: rooms.clone(),
            },
            Self::RemoveAlias { name } => AliasEdit::RemoveAlias { name: name.clone() },
            Self::SetDefault { room, client } => AliasEdit::SetDefault {
                room: room.clone(),
                client: client.clone(),
            },
            Self::ClearDefault { client } => AliasEdit::ClearDefault {
                client: client.clone(),
            },
        }
    }

    /// Schema fallback for the MCP derive when this external type is a field.
    #[must_use]
    pub fn json_schema() -> serde_json::Value {
        serde_json::to_value(<Self as fastapi_openapi::JsonSchema>::schema())
            .expect("room configuration schema is JSON")
    }

    fn description(&self) -> String {
        match self {
            Self::SetAlias { name, .. } => format!("set room alias {name:?}"),
            Self::RemoveAlias { name } => format!("removed room alias {name:?}"),
            Self::SetDefault { room, client } => format!(
                "set the default room for {} to {room:?}",
                client.as_deref().unwrap_or("everyone")
            ),
            Self::ClearDefault { client } => format!(
                "cleared the default room for {}",
                client.as_deref().unwrap_or("everyone")
            ),
        }
    }
}

// The pinned derive emits `type` for data enums regardless of serde's tag.
// Construct the exact `kind` alternatives so HTTP and MCP publish the wire form.
impl fastapi_openapi::JsonSchema for RoomConfigurationChange {
    fn schema() -> fastapi_openapi::Schema {
        use fastapi_openapi::{JsonSchema as _, Schema};
        Schema::one_of(vec![
            change_schema(
                "set_alias",
                &["kind", "name", "rooms"],
                [
                    ("name", String::schema()),
                    ("rooms", Vec::<String>::schema()),
                ],
            ),
            change_schema(
                "remove_alias",
                &["kind", "name"],
                [("name", String::schema())],
            ),
            change_schema(
                "set_default",
                &["kind", "room"],
                [
                    ("room", String::schema()),
                    ("client", Option::<String>::schema()),
                ],
            ),
            change_schema(
                "clear_default",
                &["kind"],
                [("client", Option::<String>::schema())],
            ),
        ])
    }

    fn schema_name() -> Option<&'static str> {
        Some("RoomConfigurationChange")
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
pub struct RoomConfigureRequest {
    pub expected_revision: String,
    pub change: RoomConfigurationChange,
}

impl RoomConfigureRequest {
    /// Validate before daemon discovery, configuration reads, or writes.
    pub fn validate(&self) -> Result<(), Failure> {
        if self.expected_revision.trim().is_empty()
            || self.expected_revision.len() > 4096
            || self.expected_revision.chars().any(char::is_control)
        {
            return Err(Failure::invalid(
                "read the room configuration and supply its revision",
            ));
        }
        self.change.validate()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RoomConfiguredDto {
    pub done: String,
    pub changed: bool,
    pub configuration: RoomConfigurationDto,
}

fn config_failure(error: ConfigurationError) -> Failure {
    let code = match &error {
        ConfigurationError::Conflict { .. } => ErrorCode::ConfigChanged,
        ConfigurationError::Invalid(_) | ConfigurationError::Parse(_) => ErrorCode::InvalidArgument,
        ConfigurationError::Publish {
            published: true, ..
        } => ErrorCode::OutcomeUncertain,
        _ => ErrorCode::Internal,
    };
    let failure = Failure::new(code, error.to_string());
    match code {
        ErrorCode::ConfigChanged | ErrorCode::OutcomeUncertain => failure.with_hint(
            "Read the daemon's room configuration and review its current values before another edit. The write was not retried.",
        ),
        _ => failure,
    }
}

impl Surface {
    /// Configuration remains readable without any online speakers. Warnings
    /// use only the already available live or cached inventory.
    pub fn room_configuration(&self, client: &Client) -> Result<RoomConfigurationDto, Failure> {
        self.guard(client).authorize("room_configuration", true)?;
        let path = self.room_configuration_path()?;
        let snapshot = configuration::read(path).map_err(config_failure)?;
        let households = self.room_configuration_inventory();
        Ok(RoomConfigurationDto::from_snapshot(
            snapshot,
            households.as_deref(),
        ))
    }

    /// Apply one owner-wide alias/default change under both configuration tool
    /// permission and ownership administration. It sends no speaker commands.
    pub fn configure_rooms(
        &self,
        client: &Client,
        request: &RoomConfigureRequest,
    ) -> Result<RoomConfiguredDto, Failure> {
        self.authorize_write(client, "configure_rooms")?;
        self.authorize_change(
            client,
            "configure_rooms",
            "the house room configuration",
            None,
        )?;
        request.validate()?;
        let path = self.room_configuration_path()?;
        let households = self.room_configuration_inventory();
        let _restoring = self.log.as_ref().map(|log| {
            log.restoring
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
        });
        let result = configuration::apply(path, &request.expected_revision, &request.change.edit())
            .map_err(config_failure)
            .map(|saved| RoomConfiguredDto {
                done: if saved.changed {
                    request.change.description()
                } else {
                    "room configuration unchanged".into()
                },
                changed: saved.changed,
                configuration: RoomConfigurationDto::from_snapshot(
                    saved.snapshot,
                    households.as_deref(),
                ),
            });
        let barrier = result.as_ref().map_or_else(
            |error| error.code == ErrorCode::OutcomeUncertain,
            |saved| saved.changed,
        );
        self.log_kept_write(
            client,
            "configure_rooms".into(),
            &result,
            |saved| saved.done.clone(),
            barrier.then_some("room configuration edit"),
        );
        result
    }

    fn room_configuration_path(&self) -> Result<&std::path::Path, Failure> {
        self.aliases_file.as_deref().ok_or_else(|| {
            Failure::new(
                ErrorCode::NotReady,
                "this surface has no room configuration file",
            )
            .with_hint("Use the running daemon or configure its data directory.")
        })
    }

    fn room_configuration_inventory(&self) -> Option<Vec<HouseholdState>> {
        self.live()
            .map(|live| live.households())
            .filter(|households| households.iter().any(|house| !house.rooms.is_empty()))
            .or_else(|| {
                self.cache
                    .lock()
                    .ok()?
                    .as_ref()
                    .map(|(_, households)| households.clone())
            })
    }
}

#[cfg(test)]
mod tests;
