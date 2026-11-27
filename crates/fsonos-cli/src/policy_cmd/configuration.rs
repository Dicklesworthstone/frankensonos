//! Owner policy editing through one selected daemon or configuration-only surface.
//! A read, merge and compare-and-swap write always use that same target.

use anyhow::Result;
use clap::{ArgAction, ArgGroup, Args, Subcommand, ValueEnum};
use fsonos_api::surface::house_policy::{
    PolicyConfigurationChange, PolicyConfigurationDto, PolicyConfigureRequest, PolicyConfiguredDto,
};
use fsonos_api::{ErrorCode, Failure, Surface};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::{Client, McpProfile, Policy, PolicyView};
use fsonos_core::rooms::normalize_room;
use fsonos_proto::{ProtoError, Transport};
use std::net::IpAddr;

use crate::config::GlobalArgs;
use crate::remote::Daemon;

#[derive(Debug, Clone, Subcommand)]
pub enum Change {
    /// Change default limits, preserving fields that are not supplied.
    Defaults(Defaults),
    /// Set or clear the daily quiet-hours interval.
    QuietHours {
        #[command(subcommand)]
        change: QuietHoursChange,
    },
    /// Set or remove a room's limit overrides.
    Room {
        #[command(subcommand)]
        change: RoomChange,
    },
    /// Set or remove a client's permissions and limits.
    Client {
        #[command(subcommand)]
        change: ClientChange,
    },
}

#[derive(Debug, Clone, Args)]
pub struct Revision {
    /// Require this reviewed configuration revision instead of the freshly read revision.
    #[arg(long, value_name = "REVISION")]
    expected_revision: Option<String>,
}

#[derive(Debug, Clone, Args)]
#[command(group(
    ArgGroup::new("default_fields")
        .required(true)
        .multiple(true)
        .args(["max_volume", "max_step", "fade_secs"])
))]
pub struct Defaults {
    #[command(flatten)]
    revision: Revision,
    /// Default maximum volume (0-100).
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    max_volume: Option<u8>,
    /// Default maximum relative volume step (0-100).
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    max_step: Option<u8>,
    /// Default fade duration in seconds.
    #[arg(long)]
    fade_secs: Option<u32>,
}

#[derive(Debug, Clone, Subcommand)]
pub enum QuietHoursChange {
    /// Replace the quiet-hours interval and its volume limit.
    Set(QuietHoursSet),
    /// Remove the quiet-hours override.
    Clear(Revision),
}

#[derive(Debug, Clone, Args)]
pub struct QuietHoursSet {
    #[command(flatten)]
    revision: Revision,
    /// Start time in HH:MM format.
    #[arg(long, value_name = "HH:MM")]
    start: String,
    /// End time in HH:MM format; an earlier time spans midnight.
    #[arg(long, value_name = "HH:MM")]
    end: String,
    /// Maximum volume during quiet hours (0-100).
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    max_volume: u8,
}

#[derive(Debug, Clone, Subcommand)]
pub enum RoomChange {
    /// Change selected room limits, preserving other overrides.
    Set(RoomSet),
    /// Remove all overrides for this room.
    #[command(alias = "remove")]
    Rm(RoomName),
}

#[derive(Debug, Clone, Args)]
pub struct RoomName {
    #[arg(value_name = "ROOM")]
    room: String,
    #[command(flatten)]
    revision: Revision,
}

#[derive(Debug, Clone, Args)]
#[command(group(
    ArgGroup::new("room_fields")
        .required(true)
        .multiple(true)
        .args(["max_volume", "inherit_max_volume", "max_step", "inherit_max_step"])
))]
pub struct RoomSet {
    #[arg(value_name = "ROOM")]
    room: String,
    #[command(flatten)]
    revision: Revision,
    /// Set this room's maximum volume (0-100).
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    max_volume: Option<u8>,
    /// Remove this room's volume override.
    #[arg(long, conflicts_with = "max_volume")]
    inherit_max_volume: bool,
    /// Set this room's maximum relative volume step (0-100).
    #[arg(long, value_parser = clap::value_parser!(u8).range(0..=100))]
    max_step: Option<u8>,
    /// Remove this room's relative-step override.
    #[arg(long, conflicts_with = "max_step")]
    inherit_max_step: bool,
}

#[derive(Debug, Clone, Subcommand)]
pub enum ClientChange {
    /// Change selected client fields, preserving other permissions.
    Set(ClientSet),
    /// Remove the complete rule for this client.
    #[command(alias = "remove")]
    Rm(ClientName),
}

#[derive(Debug, Clone, Args)]
pub struct ClientName {
    #[arg(value_name = "CLIENT")]
    client: String,
    #[command(flatten)]
    revision: Revision,
}

#[derive(Debug, Clone, Copy, ValueEnum)]
pub enum Profile {
    Essentials,
    Full,
}

impl From<Profile> for McpProfile {
    fn from(value: Profile) -> Self {
        match value {
            Profile::Essentials => Self::Essentials,
            Profile::Full => Self::Full,
        }
    }
}

#[derive(Debug, Clone, Args)]
#[command(group(
    ArgGroup::new("client_fields")
        .required(true)
        .multiple(true)
        .args([
            "allow", "allow_none", "inherit_allow", "deny", "clear_deny",
            "grant", "clear_grant", "capped", "inherit_capped",
            "manage_others", "mcp_profile", "inherit_mcp_profile",
        ])
))]
#[allow(clippy::struct_excessive_bools)] // CLI switches distinguish clearing from inheritance.
pub struct ClientSet {
    #[arg(value_name = "CLIENT")]
    client: String,
    #[command(flatten)]
    revision: Revision,
    /// Replace the allow list; repeat this flag or separate entries with commas.
    #[arg(
        long,
        value_name = "TOOL_OR_CLASS",
        value_delimiter = ',',
        action = ArgAction::Append,
        conflicts_with_all = ["allow_none", "inherit_allow"]
    )]
    allow: Option<Vec<String>>,
    /// Set an explicit empty allow list.
    #[arg(long, conflicts_with = "inherit_allow")]
    allow_none: bool,
    /// Remove the explicit allow list.
    #[arg(long)]
    inherit_allow: bool,
    /// Replace the deny list; repeat this flag or separate entries with commas.
    #[arg(long, value_name = "TOOL_OR_CLASS", value_delimiter = ',', action = ArgAction::Append)]
    deny: Option<Vec<String>>,
    /// Clear the deny list.
    #[arg(long, conflicts_with = "deny")]
    clear_deny: bool,
    /// Replace grants; repeat this flag or separate entries with commas.
    #[arg(long, value_name = "TOOL_OR_CLASS", value_delimiter = ',', action = ArgAction::Append)]
    grant: Option<Vec<String>>,
    /// Clear all grants.
    #[arg(long, conflicts_with = "grant")]
    clear_grant: bool,
    /// Set whether this client's volume is capped (true or false).
    #[arg(long, value_name = "BOOL", action = ArgAction::Set, num_args = 1)]
    capped: Option<bool>,
    /// Remove the explicit capped override.
    #[arg(long, conflicts_with = "capped")]
    inherit_capped: bool,
    /// Set whether this client may manage other clients' work (true or false).
    #[arg(long, value_name = "BOOL", action = ArgAction::Set, num_args = 1)]
    manage_others: Option<bool>,
    /// Select the MCP tool profile.
    #[arg(long, value_enum)]
    mcp_profile: Option<Profile>,
    /// Remove the explicit MCP profile override.
    #[arg(long, conflicts_with = "mcp_profile")]
    inherit_mcp_profile: bool,
}

impl Change {
    fn validate(&self) -> Result<(), Failure> {
        let change = self.configuration(&Policy::default().view());
        match &self.revision().expected_revision {
            Some(expected) => PolicyConfigureRequest {
                expected_revision: expected.clone(),
                change,
            }
            .validate(),
            None => change.validate(),
        }
    }

    fn revision(&self) -> &Revision {
        match self {
            Self::Defaults(fields) => &fields.revision,
            Self::QuietHours { change } => match change {
                QuietHoursChange::Set(fields) => &fields.revision,
                QuietHoursChange::Clear(revision) => revision,
            },
            Self::Room { change } => match change {
                RoomChange::Set(fields) => &fields.revision,
                RoomChange::Rm(fields) => &fields.revision,
            },
            Self::Client { change } => match change {
                ClientChange::Set(fields) => &fields.revision,
                ClientChange::Rm(fields) => &fields.revision,
            },
        }
    }

    fn configuration(&self, current: &PolicyView) -> PolicyConfigurationChange {
        match self {
            Self::Defaults(fields) => PolicyConfigurationChange::SetDefaults {
                max_volume: fields.max_volume.unwrap_or(current.defaults.max_volume),
                max_step: fields.max_step.unwrap_or(current.defaults.max_step),
                fade_secs: fields.fade_secs.unwrap_or(current.defaults.fade_secs),
            },
            Self::QuietHours { change } => match change {
                QuietHoursChange::Set(fields) => PolicyConfigurationChange::SetQuietHours {
                    start: fields.start.clone(),
                    end: fields.end.clone(),
                    max_volume: fields.max_volume,
                },
                QuietHoursChange::Clear(_) => PolicyConfigurationChange::ClearQuietHours {},
            },
            Self::Room { change } => match change {
                RoomChange::Set(fields) => fields.configuration(current),
                RoomChange::Rm(fields) => PolicyConfigurationChange::RemoveRoom {
                    room: fields.room.clone(),
                },
            },
            Self::Client { change } => match change {
                ClientChange::Set(fields) => fields.configuration(current),
                ClientChange::Rm(fields) => PolicyConfigurationChange::RemoveClient {
                    client: fields.client.clone(),
                },
            },
        }
    }

    fn request(&self, current: &PolicyConfigurationDto) -> Result<PolicyConfigureRequest> {
        if let Some(expected) = &self.revision().expected_revision
            && expected != &current.revision
        {
            return Err(Failure::new(
                ErrorCode::ConfigChanged,
                "policy configuration changed since the supplied revision was reviewed",
            )
            .with_hint(
                "Read policy configuration again and review its current values before editing.",
            )
            .into());
        }
        let request = PolicyConfigureRequest {
            expected_revision: current.revision.clone(),
            change: self.configuration(&current.policy),
        };
        request.validate()?;
        Ok(request)
    }
}

impl RoomSet {
    fn configuration(&self, current: &PolicyView) -> PolicyConfigurationChange {
        let identity = normalize_room(&self.room);
        let prior = current
            .rooms
            .iter()
            .find(|rule| normalize_room(&rule.room) == identity);
        PolicyConfigurationChange::SetRoom {
            room: prior.map_or_else(|| self.room.clone(), |rule| rule.room.clone()),
            max_volume: if self.inherit_max_volume {
                None
            } else {
                self.max_volume.or(prior.and_then(|rule| rule.max_volume))
            },
            max_step: if self.inherit_max_step {
                None
            } else {
                self.max_step.or(prior.and_then(|rule| rule.max_step))
            },
        }
    }
}

impl ClientSet {
    fn configuration(&self, current: &PolicyView) -> PolicyConfigurationChange {
        let identity = self.client.to_lowercase();
        let prior = current
            .clients
            .iter()
            .find(|rule| rule.client.to_lowercase() == identity);
        PolicyConfigurationChange::SetClient {
            client: prior.map_or_else(|| self.client.clone(), |rule| rule.client.clone()),
            allow: if self.allow_none {
                Some(Vec::new())
            } else if self.inherit_allow {
                None
            } else {
                self.allow
                    .clone()
                    .or_else(|| prior.and_then(|rule| rule.allow.clone()))
            },
            deny: if self.clear_deny {
                Vec::new()
            } else {
                self.deny
                    .clone()
                    .unwrap_or_else(|| prior.map_or_else(Vec::new, |rule| rule.deny.clone()))
            },
            grant: if self.clear_grant {
                Vec::new()
            } else {
                self.grant
                    .clone()
                    .unwrap_or_else(|| prior.map_or_else(Vec::new, |rule| rule.grant.clone()))
            },
            capped: if self.inherit_capped {
                None
            } else {
                self.capped.or(prior.and_then(|rule| rule.capped))
            },
            manage_others: self
                .manage_others
                .unwrap_or(prior.is_some_and(|rule| rule.manage_others)),
            mcp_profile: if self.inherit_mcp_profile {
                None
            } else {
                self.mcp_profile
                    .map(McpProfile::from)
                    .or(prior.and_then(|rule| rule.mcp_profile))
            },
        }
    }
}

struct ConfigurationOnly;

impl Transport for ConfigurationOnly {
    fn soap_post(
        &self,
        _: IpAddr,
        _: &str,
        _: &str,
        _: &str,
    ) -> std::result::Result<String, ProtoError> {
        Err(ProtoError::NotWired(
            "policy configuration never controls speakers",
        ))
    }
}

enum Target {
    Daemon(Daemon),
    Local(Box<Surface>),
}

impl Target {
    fn select(global: &GlobalArgs) -> Result<Self, Failure> {
        if let Some(daemon) = Daemon::find(global, global.daemon)? {
            return Ok(Self::Daemon(daemon));
        }
        let dir = crate::daemon::data_dir(global)?;
        let surface = Surface::new(
            Box::new(ConfigurationOnly),
            Box::new(|_| Ok(Vec::new())),
            crate::daemon::policy(&dir)?,
            Box::new(SystemClock),
        );
        Ok(Self::Local(Box::new(crate::daemon::with_action_log(
            surface, &dir, "cli",
        ))))
    }

    fn read(&self) -> Result<PolicyConfigurationDto, Failure> {
        match self {
            Self::Daemon(daemon) => daemon.get("/policy/configuration"),
            Self::Local(surface) => surface.policy_configuration(&Client::Cli),
        }
    }

    fn apply(&self, request: &PolicyConfigureRequest) -> Result<PolicyConfiguredDto, Failure> {
        match self {
            Self::Daemon(daemon) => daemon.post("/policy/configuration", request),
            Self::Local(surface) => surface.configure_policy(&Client::Cli, request),
        }
    }
}

pub fn read(global: &GlobalArgs) -> Result<()> {
    let current = Target::select(global)?.read()?;
    let rendered = serde_json::to_string_pretty(&current)?;
    crate::emit(global.json, &current, |_| format!("{rendered}\n"))
}

pub fn run(global: &GlobalArgs, change: &Change) -> Result<()> {
    // Reject malformed authored values before selecting a target or creating
    // local state. Missing fields get valid placeholders only for validation.
    change.validate()?;
    let target = Target::select(global)?;
    let current = target.read()?;
    let request = change.request(&current)?;
    let saved = target.apply(&request)?;
    crate::emit(global.json, &saved, |result| {
        format!(
            "{}\nRevision: {}\n",
            result.done, result.configuration.revision
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        Change, Failure, McpProfile, Policy, PolicyConfigurationChange, PolicyConfigurationDto,
    };
    use clap::Parser;
    use serde_json::json;

    #[derive(Debug, Parser)]
    struct Parse {
        #[command(subcommand)]
        change: Change,
    }

    fn parse(arguments: &[&str]) -> Change {
        Parse::try_parse_from(std::iter::once("configure").chain(arguments.iter().copied()))
            .unwrap()
            .change
    }

    fn current() -> PolicyConfigurationDto {
        let mut policy = Policy::default().view();
        policy.defaults.max_volume = 67;
        policy.defaults.max_step = 9;
        policy.defaults.fade_secs = 4;
        policy.rooms = serde_json::from_value(json!([{
            "room": "Living Room",
            "max_volume": 31,
            "max_step": 3
        }]))
        .unwrap();
        policy.clients = serde_json::from_value(json!([{
            "client": "guest",
            "allow": ["read"],
            "deny": ["volume"],
            "grant": ["auth"],
            "capped": false,
            "manage_others": true,
            "mcp_profile": McpProfile::Full
        }]))
        .unwrap();
        PolicyConfigurationDto {
            revision: "a".repeat(64),
            policy,
        }
    }

    #[test]
    fn top_level_cli_dispatches_configuration_reads_and_typed_edits() {
        let shown = crate::Cli::try_parse_from(["fsonos", "policy", "configuration"]).unwrap();
        assert!(matches!(
            shown.command,
            crate::Command::Policy {
                action: crate::policy_cmd::PolicyAction::Configuration,
            }
        ));
        let edited = crate::Cli::try_parse_from([
            "fsonos",
            "--daemon",
            "policy",
            "configure",
            "defaults",
            "--max-volume",
            "45",
        ])
        .unwrap();
        let crate::Command::Policy {
            action: crate::policy_cmd::PolicyAction::Configure { change },
        } = edited.command
        else {
            panic!("expected policy configuration edit");
        };
        assert_eq!(
            change.request(&current()).unwrap().change,
            PolicyConfigurationChange::SetDefaults {
                max_volume: 45,
                max_step: 9,
                fade_secs: 4,
            }
        );
    }

    #[test]
    fn typed_commands_cover_every_configuration_edit() {
        let commands: &[&[&str]] = &[
            &["defaults", "--max-volume", "55"],
            &[
                "quiet-hours",
                "set",
                "--start",
                "22:30",
                "--end",
                "07:15",
                "--max-volume",
                "20",
            ],
            &["quiet-hours", "clear"],
            &["room", "set", "Living Room", "--max-step", "4"],
            &["room", "rm", "Living Room"],
            &[
                "client",
                "set",
                "guest",
                "--allow",
                "read,playback",
                "--grant",
                "auth",
                "--capped",
                "true",
                "--mcp-profile",
                "essentials",
            ],
            &["client", "rm", "guest"],
        ];
        for command in commands {
            parse(command)
                .configuration(&Policy::default().view())
                .validate()
                .unwrap();
        }
    }

    #[test]
    fn empty_or_conflicting_edits_fail_during_parsing() {
        let invalid: &[&[&str]] = &[
            &["defaults"],
            &["defaults", "--expected-revision", "unchanged"],
            &["defaults", "--max-volume", "101"],
            &["room", "set", "Living Room"],
            &[
                "room",
                "set",
                "Living Room",
                "--max-volume",
                "20",
                "--inherit-max-volume",
            ],
            &["client", "set", "guest"],
            &["client", "set", "guest", "--allow-none", "--inherit-allow"],
            &["client", "set", "guest", "--allow", "read", "--allow-none"],
            &[
                "client",
                "set",
                "guest",
                "--capped",
                "false",
                "--inherit-capped",
            ],
            &["client", "set", "guest", "--capped", "maybe"],
            &["client", "set", "guest", "--mcp-profile", "unknown"],
        ];
        for arguments in invalid {
            assert!(
                Parse::try_parse_from(
                    std::iter::once("configure").chain(arguments.iter().copied())
                )
                .is_err(),
                "{arguments:?}"
            );
        }
    }

    #[test]
    fn partial_defaults_keep_unmentioned_current_limits() {
        let request = parse(&["defaults", "--max-volume", "60"])
            .request(&current())
            .unwrap();
        assert_eq!(request.expected_revision, "a".repeat(64));
        assert_eq!(
            request.change,
            PolicyConfigurationChange::SetDefaults {
                max_volume: 60,
                max_step: 9,
                fade_secs: 4,
            }
        );
    }

    #[test]
    fn normalized_room_edits_preserve_spelling_and_other_overrides() {
        let current = current();
        let request = parse(&["room", "set", "living room", "--max-volume", "25"])
            .request(&current)
            .unwrap();
        assert_eq!(
            request.change,
            PolicyConfigurationChange::SetRoom {
                room: "Living Room".into(),
                max_volume: Some(25),
                max_step: Some(3),
            }
        );
        let request = parse(&["room", "set", "LIVING ROOM", "--inherit-max-volume"])
            .request(&current)
            .unwrap();
        assert_eq!(
            request.change,
            PolicyConfigurationChange::SetRoom {
                room: "Living Room".into(),
                max_volume: None,
                max_step: Some(3),
            }
        );
    }

    #[test]
    fn partial_client_edits_preserve_permissions_and_profile() {
        let request = parse(&["client", "set", "GUEST", "--capped", "true"])
            .request(&current())
            .unwrap();
        assert_eq!(
            request.change,
            PolicyConfigurationChange::SetClient {
                client: "guest".into(),
                allow: Some(vec!["read".into()]),
                deny: vec!["volume".into()],
                grant: vec!["auth".into()],
                capped: Some(true),
                manage_others: true,
                mcp_profile: Some(McpProfile::Full),
            }
        );
    }

    #[test]
    fn client_identity_uses_unicode_lowercase_like_core_configuration() {
        let mut current = current();
        current.policy.clients[0].client = "ος".into();
        let change =
            parse(&["client", "set", "ΟΣ", "--capped", "true"]).configuration(&current.policy);
        let PolicyConfigurationChange::SetClient {
            client,
            grant,
            manage_others,
            ..
        } = change
        else {
            panic!("wrong edit variant");
        };
        assert_eq!(client, "ος");
        assert_eq!(grant, ["auth"]);
        assert!(manage_others);
    }

    #[test]
    fn explicit_empty_allow_and_inherited_allow_remain_distinct() {
        for (flag, expected) in [
            ("--allow-none", Some(Vec::<String>::new())),
            ("--inherit-allow", None),
        ] {
            let request = parse(&["client", "set", "guest", flag])
                .request(&current())
                .unwrap();
            let PolicyConfigurationChange::SetClient {
                allow,
                deny,
                grant,
                capped,
                manage_others,
                mcp_profile,
                ..
            } = request.change
            else {
                panic!("wrong edit variant");
            };
            assert_eq!(allow, expected);
            assert_eq!(deny, ["volume"]);
            assert_eq!(grant, ["auth"]);
            assert_eq!(capped, Some(false));
            assert!(manage_others);
            assert_eq!(mcp_profile, Some(McpProfile::Full));
        }
    }

    #[test]
    fn explicit_clear_and_false_values_remove_only_selected_client_fields() {
        let request = parse(&[
            "client",
            "set",
            "guest",
            "--clear-deny",
            "--clear-grant",
            "--inherit-capped",
            "--manage-others",
            "false",
            "--inherit-mcp-profile",
        ])
        .request(&current())
        .unwrap();
        assert_eq!(
            request.change,
            PolicyConfigurationChange::SetClient {
                client: "guest".into(),
                allow: Some(vec!["read".into()]),
                deny: Vec::new(),
                grant: Vec::new(),
                capped: None,
                manage_others: false,
                mcp_profile: None,
            }
        );
    }

    #[test]
    fn list_flags_replace_lists_and_support_repeated_and_comma_values() {
        let request = parse(&[
            "client",
            "set",
            "guest",
            "--allow",
            "read,playback",
            "--allow",
            "configure_policy",
            "--deny",
            "next",
            "--grant",
            "spotify_sign_in",
        ])
        .request(&current())
        .unwrap();
        let PolicyConfigurationChange::SetClient {
            allow, deny, grant, ..
        } = request.change
        else {
            panic!("wrong edit variant");
        };
        assert_eq!(
            allow,
            Some(vec![
                "read".into(),
                "playback".into(),
                "configure_policy".into(),
            ])
        );
        assert_eq!(deny, ["next"]);
        assert_eq!(grant, ["spotify_sign_in"]);
    }

    #[test]
    fn explicit_revision_must_match_before_partial_fields_are_merged() {
        let current = current();
        let stale = "b".repeat(64);
        let change = parse(&[
            "defaults",
            "--max-volume",
            "50",
            "--expected-revision",
            &stale,
        ]);
        let error = change.request(&current).unwrap_err();
        assert!(error.downcast_ref::<Failure>().is_some());
        assert!(error.to_string().contains("supplied revision"));
        let reviewed = parse(&[
            "defaults",
            "--max-volume",
            "50",
            "--expected-revision",
            &current.revision,
        ])
        .request(&current)
        .unwrap();
        assert_eq!(reviewed.expected_revision, current.revision);
        assert_eq!(
            reviewed.change,
            PolicyConfigurationChange::SetDefaults {
                max_volume: 50,
                max_step: 9,
                fade_secs: 4,
            }
        );
    }

    #[test]
    fn malformed_authored_policy_values_fail_before_target_selection() {
        let commands: &[&[&str]] = &[
            &[
                "quiet-hours",
                "set",
                "--start",
                "25:00",
                "--end",
                "07:00",
                "--max-volume",
                "20",
            ],
            &["room", "set", "", "--max-volume", "20"],
            &["client", "set", "guest", "--allow", "invalid tool!"],
            &["defaults", "--max-volume", "50", "--expected-revision", " "],
        ];
        for command in commands {
            assert!(parse(command).validate().is_err(), "{command:?}");
        }
    }
}
