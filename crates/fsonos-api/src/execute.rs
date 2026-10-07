//! Carrying out a planned [`Command`] on the speakers through the core's
//! control orchestration, and saying what happened.
//!
//! Planning (see [`crate::plan`]) already chose the player each command
//! addresses; this is the one place a surface turns a command into SOAP
//! actions, so the CLI, the HTTP API and the MCP tools report alike.

use fsonos_core::{HouseholdState, control};
use fsonos_proto::Transport;
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};

use crate::failure::{ErrorCode, Failure};
use crate::plan::{Command, DjAction, TransportAction, VolumeScope};
use crate::request::VolumeChange;

/// The result of a command, for every surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutcomeDto {
    /// What was done, or why nothing needed doing, in a sentence.
    pub done: String,
    /// Whether anything was sent to a speaker.
    pub changed: bool,
    /// The resulting volume, for volume commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
}

impl OutcomeDto {
    fn sent(done: String) -> Self {
        Self {
            done,
            changed: true,
            volume: None,
        }
    }
}

/// Carry out `command` against `households` over `transport`.
///
/// Playback takes renderer URIs as given and sends no DIDL metadata yet, so
/// a `title` is not shown on the speaker. `spotify:` URIs and the DJ answer
/// [`ErrorCode::NotImplemented`] until Spotify rendering and the DJ are wired.
pub fn execute<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    command: &Command,
) -> Result<OutcomeDto, Failure> {
    let room = |id: &PlayerId| {
        control::locate(households, id).map_or_else(|_| id.0.clone(), |p| p.room_name.clone())
    };
    let outcome = match command {
        Command::Play {
            coordinator,
            source_uri,
            ..
        } => {
            if source_uri.starts_with("spotify:") {
                return Err(Failure::new(
                    ErrorCode::NotImplemented,
                    format!("rendering {source_uri} on Sonos is not wired yet"),
                )
                .with_hint("Play a Sonos favorite or a radio/HTTP stream URI for now."));
            }
            control::play_uri(transport, households, coordinator, source_uri, "")?;
            OutcomeDto::sent(format!(
                "playing {source_uri} in {}'s group",
                room(coordinator)
            ))
        }
        Command::Transport {
            coordinator,
            action,
        } => {
            let verb = run_transport(transport, households, coordinator, *action)?;
            OutcomeDto::sent(format!("{verb} {}'s group", room(coordinator)))
        }
        Command::Volume {
            target,
            scope,
            change,
        } => {
            let level = run_volume(transport, households, target, *scope, *change)?;
            let whose = match scope {
                VolumeScope::Room => room(target),
                VolumeScope::Group => format!("{}'s group", room(target)),
            };
            OutcomeDto {
                volume: Some(level),
                ..OutcomeDto::sent(format!("{whose} volume is {level}"))
            }
        }
        Command::Mute { target, mute } => {
            control::set_mute(transport, households, target, *mute)?;
            let state = if *mute { "muted" } else { "unmuted" };
            OutcomeDto::sent(format!("{} is {state}", room(target)))
        }
        Command::Join {
            member,
            coordinator,
        } => {
            control::join(transport, households, member, coordinator)?;
            OutcomeDto::sent(format!(
                "{} joined {}'s group",
                room(member),
                room(coordinator)
            ))
        }
        Command::Leave { member } => {
            control::leave(transport, households, member)?;
            OutcomeDto::sent(format!("{} plays on its own", room(member)))
        }
        Command::Dj { action, .. } => {
            let verb = match action {
                DjAction::Start => "start",
                DjAction::Skip => "skip",
                DjAction::Stop => "stop",
            };
            return Err(Failure::new(
                ErrorCode::NotImplemented,
                format!("dj {verb}: the DJ is not wired to the speakers yet"),
            )
            .with_hint("Play a Sonos favorite or a radio/HTTP stream URI for now."));
        }
        Command::Nothing { reason } => OutcomeDto {
            done: reason.clone(),
            changed: false,
            volume: None,
        },
    };
    Ok(outcome)
}

/// Send a transport action; returns the verb for the summary.
fn run_transport<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    action: TransportAction,
) -> Result<&'static str, Failure> {
    Ok(match action {
        TransportAction::Pause => {
            control::pause(transport, households, coordinator)?;
            "paused"
        }
        TransportAction::Resume => {
            control::resume(transport, households, coordinator)?;
            "resumed"
        }
        TransportAction::Next => {
            control::next(transport, households, coordinator)?;
            "skipped to the next track in"
        }
        TransportAction::Previous => {
            control::previous(transport, households, coordinator)?;
            "went back a track in"
        }
    })
}

/// Apply a volume change; returns the resulting level.
fn run_volume<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    target: &PlayerId,
    scope: VolumeScope,
    change: VolumeChange,
) -> Result<u8, Failure> {
    Ok(match (scope, change) {
        (VolumeScope::Room, VolumeChange::Set(v)) => {
            control::set_volume(transport, households, target, v)?
        }
        (VolumeScope::Room, VolumeChange::Adjust(d)) => {
            control::adjust_volume(transport, households, target, i32::from(d))?
        }
        (VolumeScope::Group, VolumeChange::Set(v)) => {
            control::set_group_volume(transport, households, target, v)?
        }
        (VolumeScope::Group, VolumeChange::Adjust(d)) => {
            control::adjust_group_volume(transport, households, target, i32::from(d))?
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zones::fixtures::{households, id};
    use fsonos_proto::ProtoError;
    use std::cell::RefCell;
    use std::net::IpAddr;

    /// A transport that records each SOAP action and answers success with
    /// `out_args`, like the proto crate's own canned transport.
    struct Canned {
        sent: RefCell<Vec<(IpAddr, String)>>,
        out_args: &'static str,
        fail: Option<fn() -> ProtoError>,
    }

    impl Canned {
        fn ok(out_args: &'static str) -> Self {
            Self {
                sent: RefCell::new(Vec::new()),
                out_args,
                fail: None,
            }
        }

        fn actions(&self) -> Vec<String> {
            self.sent.borrow().iter().map(|(_, a)| a.clone()).collect()
        }
    }

    impl Transport for Canned {
        fn soap_post(
            &self,
            host: IpAddr,
            _control_path: &str,
            soap_action: &str,
            _body: &str,
        ) -> Result<String, ProtoError> {
            let action = soap_action
                .trim_matches('"')
                .rsplit('#')
                .next()
                .unwrap_or_default()
                .to_string();
            self.sent.borrow_mut().push((host, action.clone()));
            if let Some(fail) = self.fail {
                return Err(fail());
            }
            Ok(format!(
                "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body>\
                 <u:{action}Response xmlns:u=\"urn:x\">{}</u:{action}Response></s:Body></s:Envelope>",
                self.out_args
            ))
        }
    }

    #[test]
    fn transport_commands_send_their_action_to_the_coordinator() {
        let houses = households();
        for (action, soap, verb) in [
            (TransportAction::Pause, "Pause", "paused Den's group"),
            (TransportAction::Resume, "Play", "resumed Den's group"),
            (
                TransportAction::Next,
                "Next",
                "skipped to the next track in Den's group",
            ),
            (
                TransportAction::Previous,
                "Previous",
                "went back a track in Den's group",
            ),
        ] {
            let t = Canned::ok("");
            let cmd = Command::Transport {
                coordinator: id("RINCON_DEN"),
                action,
            };
            let out = execute(&t, &houses, &cmd).unwrap();
            assert_eq!(t.actions(), [soap]);
            assert_eq!(out, OutcomeDto::sent(verb.to_string()));
        }
    }

    #[test]
    fn volume_reports_the_new_level() {
        let houses = households();
        let t = Canned::ok("<NewVolume>27</NewVolume>");
        let cmd = Command::Volume {
            target: id("RINCON_KIT1"),
            scope: VolumeScope::Room,
            change: VolumeChange::Adjust(-3),
        };
        let out = execute(&t, &houses, &cmd).unwrap();
        assert_eq!(t.actions(), ["SetRelativeVolume"]);
        assert_eq!(
            (out.volume, out.done.as_str()),
            (Some(27), "Kitchen volume is 27")
        );

        let t = Canned::ok("");
        let cmd = Command::Volume {
            target: id("RINCON_DEN"),
            scope: VolumeScope::Group,
            change: VolumeChange::Set(40),
        };
        let out = execute(&t, &houses, &cmd).unwrap();
        assert_eq!(t.actions(), ["SnapshotGroupVolume", "SetGroupVolume"]);
        assert_eq!(out.done, "Den's group volume is 40");
    }

    #[test]
    fn grouping_mute_and_play_reach_the_right_players() {
        let houses = households();
        let t = Canned::ok("");
        let join = Command::Join {
            member: id("RINCON_STU_L"),
            coordinator: id("RINCON_DEN"),
        };
        assert_eq!(
            execute(&t, &houses, &join).unwrap().done,
            "Ada\u{2019}s Studio joined Den's group"
        );
        let leave = Command::Leave {
            member: id("RINCON_KIT1"),
        };
        execute(&t, &houses, &leave).unwrap();
        let mute = Command::Mute {
            target: id("RINCON_KIT1"),
            mute: true,
        };
        assert_eq!(
            execute(&t, &houses, &mute).unwrap().done,
            "Kitchen is muted"
        );
        let play = Command::Play {
            coordinator: id("RINCON_DEN"),
            source_uri: "x-rincon-mp3radio://stream.example.org/live.mp3".into(),
            title: None,
        };
        execute(&t, &houses, &play).unwrap();
        assert_eq!(
            t.actions(),
            [
                "SetAVTransportURI",
                "BecomeCoordinatorOfStandaloneGroup",
                "SetMute",
                "SetAVTransportURI",
                "Play"
            ]
        );
    }

    #[test]
    fn nothing_sends_nothing() {
        let t = Canned::ok("");
        let cmd = Command::Nothing {
            reason: "Patio already plays on its own".into(),
        };
        let out = execute(&t, &households(), &cmd).unwrap();
        assert!(!out.changed && t.actions().is_empty());
        assert_eq!(out.done, "Patio already plays on its own");
    }

    #[test]
    fn unwired_paths_say_so_without_touching_speakers() {
        let t = Canned::ok("");
        let spotify = Command::Play {
            coordinator: id("RINCON_DEN"),
            source_uri: "spotify:track:0123456789ABCDEFabcdef".into(),
            title: None,
        };
        let err = execute(&t, &households(), &spotify).unwrap_err();
        assert_eq!((err.code, err.status()), (ErrorCode::NotImplemented, 501));
        let dj = Command::Dj {
            coordinator: id("RINCON_DEN"),
            action: DjAction::Start,
        };
        assert_eq!(
            execute(&t, &households(), &dj).unwrap_err().code,
            ErrorCode::NotImplemented
        );
        assert_eq!(t.actions(), Vec::<String>::new());
    }

    #[test]
    fn speaker_failures_become_coded_failures() {
        let t = Canned {
            fail: Some(|| ProtoError::Network {
                target: "192.0.2.10:1400".into(),
                detail: "connection refused".into(),
            }),
            ..Canned::ok("")
        };
        let cmd = Command::Transport {
            coordinator: id("RINCON_DEN"),
            action: TransportAction::Pause,
        };
        let err = execute(&t, &households(), &cmd).unwrap_err();
        assert_eq!(err.code, ErrorCode::PlayerUnreachable);
        assert!(err.retryable());

        let gone = Command::Leave {
            member: id("RINCON_NOWHERE"),
        };
        let err = execute(&Canned::ok(""), &households(), &gone).unwrap_err();
        assert_eq!(err.code, ErrorCode::PlayerUnreachable);
    }
}
