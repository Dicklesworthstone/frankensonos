//! Carrying out a planned [`Command`] on the speakers through the core's
//! control orchestration, and saying what happened.
//!
//! Planning (see [`crate::plan`]) already chose the player each command
//! addresses; this is the one place a surface turns a command into SOAP
//! actions, so the CLI, the HTTP API and the MCP tools report alike.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::moving::{self, MoveMethod};
use fsonos_core::{CoreError, HouseholdState, control, resolve_room};
use fsonos_proto::Transport;
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use crate::failure::{ErrorCode, Failure};
use crate::guard::{Guard, Note};
use crate::plan::{Command, DjAction, TransportAction, VolumeScope};
use crate::request::VolumeChange;

/// The result of a command, for every surface.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct OutcomeDto {
    /// What was done, or why nothing needed doing, in a sentence.
    pub done: String,
    /// Whether anything was sent to a speaker.
    pub changed: bool,
    /// The resulting volume, for volume commands.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub volume: Option<u8>,
    /// How the request was carried out, e.g. a volume clamped by the house
    /// policy (`VOLUME_CLAMPED`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<Note>,
}

impl OutcomeDto {
    fn sent(done: String) -> Self {
        Self {
            done,
            changed: true,
            volume: None,
            notes: Vec::new(),
        }
    }
}

/// [`execute`] under the house policy: bound `command` by `guard` (a capped
/// caller's volume may be lowered), carry it out, and attach the notes. The
/// caller authorizes the tool with [`Guard::authorize`] before planning.
pub fn execute_guarded<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    guard: &Guard<'_>,
    command: Command,
) -> Result<OutcomeDto, Failure> {
    let (command, notes) = guard.bound(transport, households, command)?;
    let mut outcome = execute(transport, households, &command)?;
    outcome.notes = notes;
    Ok(outcome)
}

/// Carry out `command` against `households` over `transport`.
///
/// `spotify:track:` URIs render with the household's learned Spotify
/// parameters ([`ErrorCode::RenderParamsMissing`] when it has no Spotify
/// favorite to learn from). Other renderer URIs play as given, without DIDL
/// metadata. Spotify albums and playlists, and the DJ, answer
/// [`ErrorCode::NotImplemented`] until they are wired.
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
            title,
        } => {
            play(
                transport,
                households,
                coordinator,
                source_uri,
                title.as_deref(),
            )?;
            OutcomeDto::sent(format!(
                "playing {source_uri} in {}'s group",
                room(coordinator)
            ))
        }
        Command::PlayFavorite {
            coordinator,
            favorite,
        } => {
            fsonos_core::favorites::play(transport, households, coordinator, favorite)?;
            OutcomeDto::sent(format!(
                "playing the favorite {:?} in {}'s group",
                favorite.title,
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
        Command::Move { from, to, copy } => move_music(transport, households, from, to, *copy)?,
        Command::Party { member, lead } => party(transport, households, member, lead.as_ref())?,
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
            changed: false,
            ..OutcomeDto::sent(reason.clone())
        },
    };
    Ok(outcome)
}

/// Move (or with `copy`, replay) the music room `from` plays to room `to`.
fn move_music<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    from: &PlayerId,
    to: &PlayerId,
    copy: bool,
) -> Result<OutcomeDto, Failure> {
    let source = resolve_room(households, &from.0)?;
    let dest = resolve_room(households, &to.0)?;
    let moved = if copy {
        moving::copy_playback(transport, households, &source, &dest)
    } else {
        moving::move_playback(transport, households, &source, &dest)
    }
    .map_err(move_failure)?;
    let how = match moved.method {
        MoveMethod::Nothing => "nothing to move",
        MoveMethod::Regrouped => "regrouped",
        MoveMethod::Delegated => "handed the group over",
        MoveMethod::Copied => "replayed there",
    };
    Ok(OutcomeDto::sent(format!(
        "moved the music from {} to {} ({how})",
        source.room.name, dest.room.name
    )))
}

/// Group every room of `member`'s household under `lead`'s group.
fn party<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    member: &PlayerId,
    lead: Option<&PlayerId>,
) -> Result<OutcomeDto, Failure> {
    let household = households
        .iter()
        .find(|h| h.player(member).is_some())
        .ok_or_else(|| CoreError::UnknownPlayer(member.0.clone()))?;
    let lead = lead.map(|l| resolve_room(households, &l.0)).transpose()?;
    let outcome = moving::party(transport, households, household, lead.as_ref())?;
    let mut done = format!("{} joined the party", rooms_list(&outcome.moved));
    if !outcome.failed.is_empty() {
        let failed: Vec<String> = outcome
            .failed
            .iter()
            .map(|(room, why)| format!("{room} ({why})"))
            .collect();
        let _ = write!(done, "; failed: {}", failed.join(", "));
    }
    Ok(OutcomeDto {
        changed: !outcome.moved.is_empty(),
        ..OutcomeDto::sent(done)
    })
}

/// A move that could not go ahead, coded.
fn move_failure(err: moving::MoveError) -> Failure {
    let detail = err.to_string();
    match err {
        moving::MoveError::CrossHousehold { .. } => {
            Failure::new(ErrorCode::CrossHouseholdGroup, detail)
                .with_hint("Copy the music there instead: copy true (fsonos move --copy).")
        }
        moving::MoveError::NoRenderParams(_) => {
            Failure::new(ErrorCode::RenderParamsMissing, detail)
        }
        moving::MoveError::JoinTimedOut { .. } => {
            Failure::new(ErrorCode::PlayerUnreachable, detail)
        }
        moving::MoveError::Core(e) => Failure::from(e),
    }
}

/// "Kitchen, Office and Den" ("no room" when empty).
fn rooms_list(rooms: &[String]) -> String {
    match rooms {
        [] => "no room".to_string(),
        [one] => one.clone(),
        [init @ .., last] => format!("{} and {last}", init.join(", ")),
    }
}

/// Start `source_uri` on the group `coordinator` leads.
fn play<T: Transport + ?Sized>(
    transport: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    source_uri: &str,
    title: Option<&str>,
) -> Result<(), Failure> {
    if source_uri.starts_with("spotify:track:") {
        let title = title.unwrap_or(source_uri);
        let Some((uri, didl)) =
            control::spotify_track_source(transport, households, coordinator, source_uri, title)?
        else {
            return Err(Failure::new(
                ErrorCode::RenderParamsMissing,
                "this household has no Spotify track among its favorites to learn its Spotify \
                 settings from",
            ));
        };
        control::play_uri(transport, households, coordinator, &uri, &didl)?;
        return Ok(());
    }
    if source_uri.starts_with("spotify:") {
        return Err(Failure::new(
            ErrorCode::NotImplemented,
            format!("only Spotify tracks play so far; {source_uri} is not a track"),
        )
        .with_hint("Play one of its tracks (spotify:track:...) for now."));
    }
    control::play_uri(transport, households, coordinator, source_uri, "")?;
    Ok(())
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
        /// The whole response to a ContentDirectory Browse.
        browse: &'static str,
        fail: Option<fn() -> ProtoError>,
    }

    const FAVORITES_S1: &str =
        include_str!("../../fsonos-proto/tests/fixtures/browse_favorites_s1.xml");
    const NO_FAVORITES: &str =
        include_str!("../../fsonos-proto/tests/fixtures/browse_queue_empty.xml");

    impl Canned {
        fn ok(out_args: &'static str) -> Self {
            Self {
                sent: RefCell::new(Vec::new()),
                out_args,
                browse: NO_FAVORITES,
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
            if action == "Browse" {
                return Ok(self.browse.to_string());
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
    fn guarded_volume_is_clamped_for_agents_and_says_so() {
        use crate::failure::NoteCode;
        use fsonos_core::clock::SystemClock;
        use fsonos_core::policy::{Client, Policy};

        let policy = Policy::default();
        let guard = Guard {
            policy: &policy,
            client: &Client::McpStdio,
            clock: &SystemClock,
        };
        let t = Canned::ok("<CurrentVolume>60</CurrentVolume>");
        let loud = Command::Volume {
            target: id("RINCON_KIT1"),
            scope: VolumeScope::Room,
            change: VolumeChange::Set(95),
        };
        let out = execute_guarded(&t, &households(), &guard, loud).unwrap();
        assert_eq!(t.actions(), ["GetVolume", "SetVolume"]);
        assert_eq!(out.volume, Some(70));
        assert_eq!(out.notes.len(), 1);
        assert_eq!(out.notes[0].code, NoteCode::VolumeClamped);
        let wire = serde_json::to_value(&out).unwrap();
        assert_eq!(wire["notes"][0]["code"], "VOLUME_CLAMPED");
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

    fn play_spotify(uri: &str) -> Command {
        Command::Play {
            coordinator: id("RINCON_DEN"),
            source_uri: uri.into(),
            title: Some("Partita".into()),
        }
    }

    #[test]
    fn spotify_tracks_render_with_the_households_learned_params() {
        let t = Canned {
            browse: FAVORITES_S1,
            ..Canned::ok("")
        };
        let out = execute(
            &t,
            &households(),
            &play_spotify("spotify:track:0123456789ABCDEFabcdef"),
        )
        .unwrap();
        assert!(out.changed);
        assert_eq!(t.actions(), ["Browse", "SetAVTransportURI", "Play"]);
    }

    #[test]
    fn spotify_without_a_favorite_to_learn_from_says_what_to_do() {
        let t = Canned::ok("");
        let err = execute(
            &t,
            &households(),
            &play_spotify("spotify:track:0123456789ABCDEFabcdef"),
        )
        .unwrap_err();
        assert_eq!(
            (err.code, err.status()),
            (ErrorCode::RenderParamsMissing, 409)
        );
        assert!(err.hint.contains("My Sonos"), "{}", err.hint);
        assert_eq!(t.actions(), ["Browse"]);
    }

    #[test]
    fn unwired_paths_say_so_without_touching_speakers() {
        let t = Canned::ok("");
        let album = play_spotify("spotify:album:0123456789ABCDEFabcdef");
        let err = execute(&t, &households(), &album).unwrap_err();
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
