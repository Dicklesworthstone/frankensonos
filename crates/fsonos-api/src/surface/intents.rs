//! The action log's intent for planned commands: what was asked, in room
//! terms (`set_volume: Kitchen to 30`), not Rust debug output full of player
//! ids. The tool name still leads, so a filter on it keeps working.

use fsonos_core::HouseholdState;
use fsonos_core::control::Seek;
use fsonos_types::PlayerId;

use crate::dj::DjSteer;
use crate::plan::{Command, DjAction, TransportAction, VolumeScope};
use crate::request::VolumeChange;

/// `tool: what`, the commands of a set joined with `; `.
pub(super) fn of(tool: &str, households: &[HouseholdState], commands: &[Command]) -> String {
    let what: Vec<String> = commands.iter().map(|c| describe(households, c)).collect();
    format!("{tool}: {}", what.join("; "))
}

/// A player's room name, or its id when no household knows it.
fn room(households: &[HouseholdState], id: &PlayerId) -> String {
    households
        .iter()
        .find_map(|h| h.player(id))
        .map_or_else(|| id.0.clone(), |p| p.room_name.clone())
}

fn describe(households: &[HouseholdState], command: &Command) -> String {
    let room = |id: &PlayerId| room(households, id);
    match command {
        Command::Play {
            coordinator,
            source_uri,
            title,
        } => format!(
            "play {} in {}",
            title.as_deref().unwrap_or(source_uri),
            room(coordinator)
        ),
        Command::PlayFavorite {
            coordinator,
            favorite,
        } => format!("play favorite {} in {}", favorite.title, room(coordinator)),
        Command::Transport {
            coordinator,
            action,
        } => {
            let r = room(coordinator);
            match action {
                TransportAction::Pause => format!("pause {r}"),
                TransportAction::Resume => format!("resume {r}"),
                TransportAction::Next => format!("next track in {r}"),
                TransportAction::Previous => format!("previous track in {r}"),
                TransportAction::Seek(Seek::At(secs)) => format!("seek {r} to {secs} s"),
                TransportAction::Seek(Seek::By(secs)) => format!("seek {r} by {secs:+} s"),
            }
        }
        Command::Volume {
            target,
            scope,
            change,
        } => {
            let whose = match scope {
                VolumeScope::Room => room(target),
                VolumeScope::Group => format!("{}'s group", room(target)),
            };
            match change {
                VolumeChange::Set(level) => format!("{whose} to {level}"),
                VolumeChange::Adjust(by) => format!("{whose} by {by:+}"),
            }
        }
        Command::Mute { target, mute } => {
            let verb = if *mute { "mute" } else { "unmute" };
            format!("{verb} {}", room(target))
        }
        Command::Join {
            member,
            coordinator,
        } => format!("{} joins {}", room(member), room(coordinator)),
        Command::Leave { member } => format!("{} leaves its group", room(member)),
        Command::Move { from, to, copy } => {
            let verb = if *copy { "copy" } else { "move" };
            format!("{verb} the music from {} to {}", room(from), room(to))
        }
        Command::Party { member, lead } => match lead {
            Some(lead) => format!("{}'s household joins {}", room(member), room(lead)),
            None => format!("{}'s household in one group", room(member)),
        },
        Command::Dj {
            coordinator,
            action,
        } => {
            let verb = match action {
                DjAction::Start => "start",
                DjAction::Skip => "skip",
                DjAction::Stop => "stop",
            };
            format!("{verb} the DJ in {}", room(coordinator))
        }
        Command::DjSteer {
            coordinator,
            steer,
            start,
        } => {
            let r = room(coordinator);
            let what = match steer.as_ref() {
                DjSteer::Set {
                    mood: Some(mood), ..
                } => format!("steer the DJ in {r} to {mood}"),
                DjSteer::Set { mood: None, .. } => format!("steer the DJ in {r}"),
                DjSteer::Clear => format!("clear the DJ's steering in {r}"),
            };
            if *start {
                format!("{what}, then start it")
            } else {
                what
            }
        }
        Command::Nothing { reason } => reason.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_types::{Generation, Player};

    fn house() -> Vec<HouseholdState> {
        let player = |id: &str, room: &str, last: u8| Player {
            id: PlayerId(id.into()),
            room_name: room.into(),
            ip: std::net::IpAddr::from([192, 0, 2, last]),
            model: "Sonos One".into(),
            generation: Generation::S2,
        };
        vec![HouseholdState {
            players: vec![
                player("RINCON_A", "Kitchen", 1),
                player("RINCON_B", "Office", 2),
            ],
            ..HouseholdState::default()
        }]
    }

    fn id(s: &str) -> PlayerId {
        PlayerId(s.into())
    }

    #[test]
    fn commands_read_in_room_terms_with_the_tool_first() {
        let h = house();
        let set = [
            Command::Volume {
                target: id("RINCON_A"),
                scope: VolumeScope::Room,
                change: VolumeChange::Set(30),
            },
            Command::Volume {
                target: id("RINCON_B"),
                scope: VolumeScope::Group,
                change: VolumeChange::Adjust(-5),
            },
        ];
        assert_eq!(
            of("set_volume", &h, &set),
            "set_volume: Kitchen to 30; Office's group by -5"
        );
        let join = Command::Join {
            member: id("RINCON_B"),
            coordinator: id("RINCON_A"),
        };
        assert_eq!(of("group", &h, &[join]), "group: Office joins Kitchen");
        let seek = Command::Transport {
            coordinator: id("RINCON_A"),
            action: TransportAction::Seek(Seek::By(-15)),
        };
        assert_eq!(of("seek", &h, &[seek]), "seek: seek Kitchen by -15 s");
    }

    #[test]
    fn a_player_no_household_knows_is_named_by_its_id() {
        let leave = Command::Leave {
            member: id("RINCON_GONE"),
        };
        assert_eq!(
            of("ungroup", &house(), &[leave]),
            "ungroup: RINCON_GONE leaves its group"
        );
    }
}
