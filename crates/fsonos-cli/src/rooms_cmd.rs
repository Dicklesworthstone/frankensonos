//! Alias/default edits use one selected daemon or a configuration-only local
//! surface. Every write compares the observed revision; remote failures never
//! fall back to the local aliases file.

use fsonos_api::surface::room_config::{
    RoomConfigurationChange, RoomConfigurationDto, RoomConfigureRequest, RoomConfiguredDto,
};
use fsonos_api::{Failure, Surface};
use fsonos_core::clock::SystemClock;
use fsonos_core::policy::Client;
use fsonos_proto::{ProtoError, Transport};
use std::fmt::Write as _;
use std::net::IpAddr;

use crate::AliasAction;
use crate::config::GlobalArgs;
use crate::remote::Daemon;

struct ConfigurationOnly;
impl Transport for ConfigurationOnly {
    fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
        Err(ProtoError::NotWired(
            "room configuration never controls speakers",
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

    fn read(&self) -> Result<RoomConfigurationDto, Failure> {
        match self {
            Self::Daemon(daemon) => daemon.get("/rooms/configuration"),
            Self::Local(surface) => surface.room_configuration(&Client::Cli),
        }
    }

    fn apply(&self, request: &RoomConfigureRequest) -> Result<RoomConfiguredDto, Failure> {
        match self {
            Self::Daemon(daemon) => daemon.post("/rooms/configuration", request),
            Self::Local(surface) => surface.configure_rooms(&Client::Cli, request),
        }
    }
}

/// Read the selected house's complete aliases/defaults and current revision.
pub fn configuration(global: &GlobalArgs) -> anyhow::Result<()> {
    let current = Target::select(global)?.read()?;
    let text = serde_json::to_string_pretty(&current)?;
    crate::emit(global.json, &current, |_| format!("{text}\n"))
}

fn configure(
    global: &GlobalArgs,
    change: RoomConfigurationChange,
    expected_revision: Option<&str>,
) -> anyhow::Result<()> {
    change.validate()?;
    if let Some(revision) = expected_revision {
        RoomConfigureRequest {
            expected_revision: revision.to_owned(),
            change: change.clone(),
        }
        .validate()?;
    }
    let target = Target::select(global)?;
    let revision = match expected_revision {
        Some(revision) => revision.to_owned(),
        None => target.read()?.revision,
    };
    let request = RoomConfigureRequest {
        expected_revision: revision,
        change,
    };
    request.validate()?;
    let result = target.apply(&request)?;
    crate::emit(global.json, &result, |result| {
        let mut text = format!(
            "{}\nRevision: {}\n",
            result.done, result.configuration.revision
        );
        for warning in &result.configuration.warnings {
            let _ = writeln!(text, "warning: {}", warning.message);
        }
        text
    })
}

/// Add, replace or remove one alias, keeping unrelated definitions and comments.
pub fn alias(global: &GlobalArgs, action: &AliasAction) -> anyhow::Result<()> {
    let (change, revision) = match action {
        AliasAction::Add {
            alias,
            rooms,
            expected_revision,
        } => (
            RoomConfigurationChange::SetAlias {
                name: alias.clone(),
                rooms: rooms.clone(),
            },
            expected_revision,
        ),
        AliasAction::Rm {
            alias,
            expected_revision,
        } => (
            RoomConfigurationChange::RemoveAlias {
                name: alias.clone(),
            },
            expected_revision,
        ),
    };
    configure(global, change, revision.as_deref())
}

/// Set, clear or show the default that resolves `here`, globally or per client.
pub fn default_room(
    global: &GlobalArgs,
    room: Option<&str>,
    client: Option<&str>,
    clear: bool,
    expected_revision: Option<&str>,
) -> anyhow::Result<()> {
    let client = client.map(str::to_owned);
    let change = match (room, clear) {
        (Some(room), _) => RoomConfigurationChange::SetDefault {
            client,
            room: room.to_owned(),
        },
        (None, true) => RoomConfigurationChange::ClearDefault { client },
        (None, false) => {
            if expected_revision.is_some() {
                return Err(Failure::invalid("--expected-revision needs a room or --clear").into());
            }
            RoomConfigurationChange::ClearDefault {
                client: client.clone(),
            }
            .validate()?;
            let current = Target::select(global)?.read()?;
            let room = client
                .as_ref()
                .and_then(|key| current.client_defaults.get(key))
                .or(current.default_room.as_ref());
            let whose = client.as_deref().unwrap_or("everyone");
            let text = room.map_or_else(
                || format!("here means no room for {whose} yet: fsonos rooms default <room>\n"),
                |room| format!("here means {room} for {whose}\n"),
            );
            return crate::emit(global.json, &current, |_| text.clone());
        }
    };
    configure(global, change, expected_revision)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{Cli, Command, RoomsCommand};
    use clap::Parser;

    #[test]
    fn existing_room_commands_accept_explicit_revisions_without_losing_targets() {
        let cli = Cli::try_parse_from([
            "fsonos",
            "--daemon",
            "rooms",
            "alias",
            "add",
            "Upstairs",
            "Study@S2",
            "Hall@S2",
            "--expected-revision",
            "observed",
        ])
        .unwrap();
        let Command::Rooms {
            alias: Some(RoomsCommand::Alias { action }),
        } = cli.command
        else {
            panic!("expected alias command")
        };
        let AliasAction::Add {
            alias,
            rooms,
            expected_revision,
        } = action
        else {
            panic!("expected alias add")
        };
        assert_eq!(alias, "Upstairs");
        assert_eq!(rooms, ["Study@S2", "Hall@S2"]);
        assert_eq!(expected_revision.as_deref(), Some("observed"));
        for words in [
            vec!["rooms", "configuration"],
            vec![
                "rooms",
                "alias",
                "rm",
                "Upstairs",
                "--expected-revision",
                "observed",
            ],
            vec![
                "rooms",
                "default",
                "Study@S2",
                "--client",
                "tag:fixture",
                "--expected-revision",
                "observed",
            ],
            vec!["rooms", "default", "--clear", "--client", "tag:fixture"],
            vec!["rooms", "default"],
        ] {
            assert!(Cli::try_parse_from(std::iter::once("fsonos").chain(words)).is_ok());
        }
    }

    #[test]
    fn invalid_edits_are_refused_before_selecting_a_daemon_or_creating_files() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("uncreated");
        let cli = Cli::try_parse_from([
            "fsonos",
            "--daemon",
            "--data-dir",
            path.to_str().unwrap(),
            "rooms",
            "configuration",
        ])
        .unwrap();
        for change in [
            RoomConfigurationChange::SetAlias {
                name: "here".into(),
                rooms: vec!["Study".into()],
            },
            RoomConfigurationChange::SetAlias {
                name: "upstairs".into(),
                rooms: vec![],
            },
            RoomConfigurationChange::SetDefault {
                client: None,
                room: "everywhere".into(),
            },
            RoomConfigurationChange::ClearDefault {
                client: Some("\n".into()),
            },
        ] {
            let error = configure(&cli.global, change, None).unwrap_err();
            assert_eq!(
                error.downcast_ref::<Failure>().unwrap().code,
                fsonos_api::ErrorCode::InvalidArgument
            );
        }
        assert!(!path.exists());
    }
}
