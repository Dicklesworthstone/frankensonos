//! House policy at the surfaces: who may call which tool, and how far a
//! volume change may go (`fsonos_core::policy`).
//!
//! Every surface runs a call through a [`Guard`] before [`crate::execute`]:
//! [`Guard::authorize`] for the tool, then [`Guard::bound`] for the planned
//! command. A clamped volume still happens, at the allowed level, and the
//! response carries a [`Note`] saying so (`VOLUME_CLAMPED`); a denial is a
//! `POLICY_DENIED` failure.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::clock::Clock;
use fsonos_core::policy::{
    Client, Decision, Policy, RoomLevel, ToolCall, VolumeChange as PolicyChange, VolumeIntent,
};
use fsonos_core::{HouseholdState, control};
use fsonos_proto::Transport;
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};

use crate::failure::{ErrorCode, Failure, NoteCode};
use crate::plan::{Command, VolumeScope};
use crate::request::VolumeChange;

/// A note on a successful response: how the request was carried out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Note {
    pub code: NoteCode,
    pub detail: String,
}

/// The policy, the caller, and the time, for one call.
pub struct Guard<'a> {
    pub policy: &'a Policy,
    pub client: &'a Client,
    pub clock: &'a dyn Clock,
}

impl Guard<'_> {
    /// May the caller use `tool` at all?
    pub fn authorize(&self, tool: &str, read_only: bool) -> Result<(), Failure> {
        match self.policy.authorize(
            ToolCall {
                name: tool,
                read_only,
            },
            self.client,
        ) {
            Decision::Allow | Decision::Clamp { .. } => Ok(()),
            Decision::Deny { reason } => Err(denied(format!(
                "{} may not use {tool}: {reason}",
                self.client
            ))),
        }
    }

    /// Bound a planned command by the policy. Only volume changes for capped
    /// callers are affected; for those the current level of every room the
    /// change lands on is read first.
    pub fn bound<T: Transport + ?Sized>(
        &self,
        transport: &T,
        households: &[HouseholdState],
        command: Command,
    ) -> Result<(Command, Vec<Note>), Failure> {
        let Command::Volume {
            target,
            scope,
            change,
        } = &command
        else {
            return Ok((command, Vec::new()));
        };
        if !self.policy.is_capped(self.client) {
            return Ok((command, Vec::new()));
        }
        let intent = VolumeIntent {
            change: to_policy(*change),
            rooms: levels(transport, households, target, *scope)?,
        };
        match self.policy.evaluate(&intent, self.client, self.clock.now()) {
            Decision::Allow => Ok((command, Vec::new())),
            Decision::Clamp {
                requested,
                allowed,
                reason,
            } => {
                let note = Note {
                    code: NoteCode::VolumeClamped,
                    detail: format!(
                        "volume {} lowered to {}: {reason}",
                        describe(requested),
                        describe(allowed)
                    ),
                };
                let bounded = Command::Volume {
                    target: target.clone(),
                    scope: *scope,
                    change: from_policy(allowed),
                };
                Ok((bounded, vec![note]))
            }
            Decision::Deny { reason } => Err(denied(reason)),
        }
    }
}

fn denied(detail: String) -> Failure {
    Failure::new(ErrorCode::PolicyDenied, detail)
}

/// The current level of each room a volume change on `target` lands on: the
/// room itself, or every room in the group `target` coordinates.
fn levels<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    target: &PlayerId,
    scope: VolumeScope,
) -> Result<Vec<RoomLevel>, Failure> {
    let household = households
        .iter()
        .find(|h| h.player(target).is_some())
        .ok_or_else(|| Failure::from(fsonos_core::CoreError::UnknownPlayer(target.0.clone())))?;
    let rooms: Vec<(&str, &PlayerId)> = household
        .rooms
        .iter()
        .filter(|r| match scope {
            VolumeScope::Room => r.primary == *target,
            VolumeScope::Group => r.coordinator == *target,
        })
        .map(|r| (r.name.as_str(), &r.primary))
        .collect();
    rooms
        .into_iter()
        .map(|(room, player)| {
            Ok(RoomLevel {
                room: room.to_string(),
                volume: control::volume(transport, households, player)?,
            })
        })
        .collect()
}

fn to_policy(change: VolumeChange) -> PolicyChange {
    match change {
        VolumeChange::Set(v) => PolicyChange::Set(v),
        VolumeChange::Adjust(d) => PolicyChange::Adjust(d),
    }
}

fn from_policy(change: PolicyChange) -> VolumeChange {
    match change {
        PolicyChange::Set(v) => VolumeChange::Set(v),
        PolicyChange::Adjust(d) => VolumeChange::Adjust(d),
    }
}

fn describe(change: PolicyChange) -> String {
    match change {
        PolicyChange::Set(v) => v.to_string(),
        PolicyChange::Adjust(d) => format!("{d:+}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zones::fixtures::{households, id};
    use fsonos_core::clock::SystemClock;
    use fsonos_proto::ProtoError;
    use std::net::IpAddr;

    /// Every player reports volume `level`.
    struct AtLevel(u8);

    impl Transport for AtLevel {
        fn soap_post(
            &self,
            _: IpAddr,
            _: &str,
            action: &str,
            _: &str,
        ) -> Result<String, ProtoError> {
            assert!(action.ends_with("#GetVolume\""), "only reads: {action}");
            Ok(format!(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                 <u:GetVolumeResponse xmlns:u=\"urn:x\"><CurrentVolume>{}</CurrentVolume>\
                 </u:GetVolumeResponse></s:Body></s:Envelope>",
                self.0
            ))
        }
    }

    fn set(target: &str, scope: VolumeScope, level: u8) -> Command {
        Command::Volume {
            target: id(target),
            scope,
            change: VolumeChange::Set(level),
        }
    }

    fn guard<'a>(policy: &'a Policy, client: &'a Client) -> Guard<'a> {
        Guard {
            policy,
            client,
            clock: &SystemClock,
        }
    }

    #[test]
    fn agents_are_clamped_with_a_note() {
        let policy = Policy::default();
        let (cmd, notes) = guard(&policy, &Client::McpStdio)
            .bound(
                &AtLevel(60),
                &households(),
                set("RINCON_KIT1", VolumeScope::Room, 95),
            )
            .unwrap();
        let Command::Volume {
            change: VolumeChange::Set(level),
            ..
        } = cmd
        else {
            panic!("expected a volume set, got {cmd:?}")
        };
        assert!(level < 95, "clamped below the request: {level}");
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].code, NoteCode::VolumeClamped);
        assert!(
            notes[0].detail.starts_with("volume 95 lowered to"),
            "{}",
            notes[0].detail
        );
    }

    #[test]
    fn the_cli_and_non_volume_commands_pass_untouched() {
        let policy = Policy::default();
        let loud = set("RINCON_KIT1", VolumeScope::Room, 95);
        let (cmd, notes) = guard(&policy, &Client::Cli)
            .bound(&AtLevel(60), &households(), loud.clone())
            .unwrap();
        assert_eq!((cmd, notes), (loud, Vec::new()));
        let pause = Command::Leave {
            member: id("RINCON_KIT1"),
        };
        let (cmd, notes) = guard(&policy, &Client::McpStdio)
            .bound(&AtLevel(60), &households(), pause.clone())
            .unwrap();
        assert_eq!((cmd, notes), (pause, Vec::new()));
    }

    #[test]
    fn group_volume_reads_every_member_room() {
        let policy = Policy::default();
        let quiet = set("RINCON_DEN", VolumeScope::Group, 30);
        let (cmd, notes) = guard(&policy, &Client::McpStdio)
            .bound(&AtLevel(20), &households(), quiet.clone())
            .unwrap();
        assert_eq!((cmd, notes), (quiet, Vec::new()));
    }

    #[test]
    fn unknown_callers_may_only_read() {
        let policy = Policy::default();
        let g = guard(&policy, &Client::Unknown);
        assert_eq!(g.authorize("list_zones", true), Ok(()));
        let err = g.authorize("pause", false).unwrap_err();
        assert_eq!((err.code, err.exit_code()), (ErrorCode::PolicyDenied, 5));
        assert!(err.detail.starts_with("unknown may not use pause"), "{err}");
        assert_eq!(
            guard(&policy, &Client::McpStdio).authorize("pause", false),
            Ok(())
        );
    }
}
