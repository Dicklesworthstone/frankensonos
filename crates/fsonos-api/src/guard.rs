//! House policy at the surfaces: who may call which tool, and how far a
//! volume change may go (`fsonos_core::policy`).
//!
//! Every surface runs a call through a [`Guard`] before [`crate::execute`]:
//! [`Guard::authorize`] for the tool, then [`Guard::bound`] for the planned
//! command. A clamped volume still happens, at the allowed level, and the
//! response carries a [`Note`] saying so (`VOLUME_CLAMPED`); a denial is a
//! `POLICY_DENIED` failure. Music a capped caller starts is held to the same
//! caps ([`Guard::playback_caps`]): the rooms it will sound in are lowered to
//! them first, however it starts there (a play, a resume, a DJ start or
//! skip, a join, an unmute).

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::clock::Clock;
use fsonos_core::policy::{
    Client, Decision, Policy, RoomLevel, ToolCall, VolumeChange as PolicyChange, VolumeIntent,
};
use fsonos_core::{HouseholdState, Room, control};
use fsonos_proto::Transport;
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::failure::{ErrorCode, Failure, NoteCode};
use crate::plan::{Command, DjAction, TransportAction, VolumeScope};
use crate::request::VolumeChange;

/// A note on a successful response: how the request was carried out.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Note {
    pub code: NoteCode,
    pub detail: String,
}

/// The policy, the caller, and the time, for one call. The policy is the
/// one in force when the call started, so a reload mid-call cannot change
/// its rules halfway.
pub struct Guard<'a> {
    pub policy: Arc<Policy>,
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

    /// May the caller change or delete `what` (a scene, a schedule), which
    /// `saver` saved ([`Policy::may_change`])? Asked after [`Self::authorize`]
    /// allowed `tool`.
    pub fn authorize_change(
        &self,
        tool: &str,
        what: &str,
        saver: Option<&str>,
    ) -> Result<(), Failure> {
        match self.policy.may_change(self.client, saver) {
            Decision::Allow | Decision::Clamp { .. } => Ok(()),
            Decision::Deny { reason } => Err(denied(format!(
                "{} may not {tool} {what}: {reason}",
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

impl Guard<'_> {
    /// Each room `command` starts music in, with the level a capped caller is
    /// held to there now and why: every room of the group a play, a resume, a
    /// DJ start or a DJ skip sounds in, the room that joins a group or is
    /// unmuted, the rooms the music moves to, a whole household for a party.
    /// Empty for an uncapped caller.
    #[must_use]
    pub fn playback_caps<'h>(
        &self,
        households: &'h [HouseholdState],
        command: &Command,
    ) -> Vec<(&'h Room, u8, String)> {
        let now = self.clock.now();
        sounding_rooms(households, command)
            .into_iter()
            .filter_map(|room| {
                let (cap, why) = self.policy.cap_for(&room.name, self.client, now)?;
                Some((room, cap, why))
            })
            .collect()
    }
}

/// The rooms `command` starts music in (see [`Guard::playback_caps`]). Every
/// command is named: a new one is a compile error here until someone decides
/// whether it can make a room sound.
fn sounding_rooms<'h>(households: &'h [HouseholdState], command: &Command) -> Vec<&'h Room> {
    let rooms = || households.iter().flat_map(|h| h.rooms.iter());
    let group = |coordinator: &PlayerId| -> Vec<&'h Room> {
        rooms().filter(|r| r.coordinator == *coordinator).collect()
    };
    match command {
        Command::Play { coordinator, .. }
        | Command::PlayFavorite { coordinator, .. }
        | Command::Transport {
            coordinator,
            action: TransportAction::Resume,
        }
        | Command::Dj {
            coordinator,
            action: DjAction::Start | DjAction::Skip,
        }
        | Command::DjSteer {
            coordinator,
            start: true,
            ..
        } => group(coordinator),
        // An unmuted room sounds at its own level whenever its group plays.
        Command::Mute {
            target: member,
            mute: false,
        }
        | Command::Join { member, .. } => rooms().filter(|r| r.players.contains(member)).collect(),
        Command::Move { to, .. } => rooms()
            .find(|r| r.players.contains(to))
            .map_or_else(Vec::new, |r| group(&r.coordinator)),
        Command::Party { member, .. } => households
            .iter()
            .find(|h| h.player(member).is_some())
            .map_or_else(Vec::new, |h| h.rooms.iter().collect()),
        // None of these starts music: a pause or a mute quiets it, a step or
        // a seek moves within what the group plays (a paused group stays
        // paused), a level is bounded by `Guard::bound`, a room that leaves
        // its group stands alone with nothing to play, and stopping or only
        // steering the DJ plays nothing new.
        Command::Transport {
            action:
                TransportAction::Pause
                | TransportAction::Next
                | TransportAction::Previous
                | TransportAction::Seek(_),
            ..
        }
        | Command::Mute { mute: true, .. }
        | Command::Volume { .. }
        | Command::Leave { .. }
        | Command::Dj {
            action: DjAction::Stop,
            ..
        }
        | Command::DjSteer { start: false, .. }
        | Command::Nothing { .. } => Vec::new(),
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
            policy: Arc::new(policy.clone()),
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

    /// The rooms each command can start sounding, and so what a capped
    /// caller's command lowers first: an unmute and a DJ skip too (p7vq.13);
    /// never a mute, a pause, a step or a DJ stop.
    #[test]
    fn unmutes_and_dj_skips_start_music_too() {
        let policy = Policy::default();
        let g = guard(&policy, &Client::McpStdio);
        let h = households();
        let names = |cmd: &Command| -> Vec<String> {
            let mut names: Vec<String> = g
                .playback_caps(&h, cmd)
                .into_iter()
                .map(|(room, ..)| room.name.clone())
                .collect();
            names.sort();
            names
        };
        let unmute = Command::Mute {
            target: id("RINCON_KIT1"),
            mute: false,
        };
        assert_eq!(names(&unmute), ["Kitchen"]);
        let skip = Command::Dj {
            coordinator: id("RINCON_DEN"),
            action: DjAction::Skip,
        };
        assert_eq!(names(&skip), ["Den", "Kitchen"]);
        for quiet in [
            Command::Mute {
                target: id("RINCON_KIT1"),
                mute: true,
            },
            Command::Dj {
                coordinator: id("RINCON_DEN"),
                action: DjAction::Stop,
            },
            Command::Transport {
                coordinator: id("RINCON_DEN"),
                action: TransportAction::Pause,
            },
            Command::Transport {
                coordinator: id("RINCON_DEN"),
                action: TransportAction::Next,
            },
        ] {
            assert!(names(&quiet).is_empty(), "{quiet:?}");
        }
        // The owner's CLI runs uncapped: nothing is lowered for it.
        let cli = guard(&policy, &Client::Cli);
        assert!(cli.playback_caps(&h, &unmute).is_empty());
    }
}
