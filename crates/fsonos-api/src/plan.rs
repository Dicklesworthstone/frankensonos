//! Planning: from a validated request to the [`Command`] the daemon carries
//! out, addressed to concrete players.
//!
//! This is where coordinator addressing happens for the surfaces. Transport,
//! playback and the DJ go to the coordinator of the room's group; room volume
//! goes to the room's own primary player, group volume to the coordinator;
//! joining targets the coordinator of the destination group. A request that is
//! already satisfied plans to [`Command::Nothing`] so retries are harmless.

use fsonos_core::{ControlTarget, HouseholdState, resolve_room};
use fsonos_types::PlayerId;

use crate::failure::{ErrorCode, Failure};
use crate::request::{
    GroupRequest, MuteRequest, PlayRequest, VolumeChange, VolumeRequest, ZoneRequest,
};

/// Pause, resume or skip on a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TransportAction {
    Pause,
    Resume,
    Next,
    Previous,
}

/// Classical DJ controls for a group.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DjAction {
    Start,
    Skip,
    Stop,
}

/// Whose volume a [`Command::Volume`] changes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeScope {
    /// One room (RenderingControl on the room's primary player).
    Room,
    /// The whole group (GroupRenderingControl on the coordinator).
    Group,
}

/// What a request asks the daemon to do, addressed to concrete players.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// Render `source_uri` on the group `coordinator` leads.
    Play {
        coordinator: PlayerId,
        source_uri: String,
        title: Option<String>,
    },
    Transport {
        coordinator: PlayerId,
        action: TransportAction,
    },
    Volume {
        target: PlayerId,
        scope: VolumeScope,
        change: VolumeChange,
    },
    /// Mute or unmute one room (RenderingControl on its primary player).
    Mute { target: PlayerId, mute: bool },
    /// Move `member` into the group `coordinator` leads.
    Join {
        member: PlayerId,
        coordinator: PlayerId,
    },
    /// Take `member` out of its group into a group of its own.
    Leave { member: PlayerId },
    Dj {
        coordinator: PlayerId,
        action: DjAction,
    },
    /// Already in the requested state; nothing to send.
    Nothing { reason: String },
}

/// Resolve a room the way every surface does: `503` while nothing has been
/// discovered (retrying helps), else the core's resolution with its
/// retry-able `404`/`409` details.
pub fn resolve<'a>(
    households: &'a [HouseholdState],
    room: &str,
) -> Result<ControlTarget<'a>, Failure> {
    if households.iter().all(|h| h.rooms.is_empty()) {
        return Err(Failure::new(
            ErrorCode::NotReady,
            "no rooms discovered yet; discovery may still be running, retry in a few seconds",
        ));
    }
    resolve_room(households, room).map_err(Failure::from)
}

/// `POST /play` / the `play` tool.
pub fn plan_play(households: &[HouseholdState], req: &PlayRequest) -> Result<Command, Failure> {
    let req = req.normalized()?;
    let target = resolve(households, &req.zone)?;
    Ok(Command::Play {
        coordinator: target.coordinator.id.clone(),
        source_uri: req.source_uri,
        title: req.title,
    })
}

/// `POST /pause|resume|next|previous` / the matching tools.
pub fn plan_transport(
    households: &[HouseholdState],
    req: &ZoneRequest,
    action: TransportAction,
) -> Result<Command, Failure> {
    let target = resolve(households, req.zone()?)?;
    Ok(Command::Transport {
        coordinator: target.coordinator.id.clone(),
        action,
    })
}

/// `POST /volume` / the `set_volume` tool.
pub fn plan_volume(households: &[HouseholdState], req: &VolumeRequest) -> Result<Command, Failure> {
    let change = req.change()?;
    let target = resolve(households, req.zone()?)?;
    let (target, scope) = if req.group {
        (target.coordinator, VolumeScope::Group)
    } else {
        (target.player, VolumeScope::Room)
    };
    Ok(Command::Volume {
        target: target.id.clone(),
        scope,
        change,
    })
}

/// `POST /mute` / the `mute` tool.
pub fn plan_mute(households: &[HouseholdState], req: &MuteRequest) -> Result<Command, Failure> {
    let target = resolve(households, req.zone()?)?;
    Ok(Command::Mute {
        target: target.player.id.clone(),
        mute: req.mute,
    })
}

/// `POST /group` / the `group` tool: move `zone` into `to`'s group.
pub fn plan_group(households: &[HouseholdState], req: &GroupRequest) -> Result<Command, Failure> {
    let (zone, to) = req.zones()?;
    let mover = resolve(households, zone)?;
    let dest = resolve(households, to)?;
    if !std::ptr::eq(mover.household, dest.household) {
        return Err(Failure::new(
            ErrorCode::CrossHouseholdGroup,
            format!(
                "{} and {} are in different households; only rooms in the same household can be grouped",
                mover.room.name, dest.room.name
            ),
        ));
    }
    if mover.room.coordinator == dest.room.coordinator {
        return Ok(Command::Nothing {
            reason: format!(
                "{} already plays in {}'s group",
                mover.room.name, dest.room.name
            ),
        });
    }
    Ok(Command::Join {
        member: mover.player.id.clone(),
        coordinator: dest.coordinator.id.clone(),
    })
}

/// `POST /ungroup` / the `ungroup` tool.
pub fn plan_ungroup(households: &[HouseholdState], req: &ZoneRequest) -> Result<Command, Failure> {
    let target = resolve(households, req.zone()?)?;
    let shares_group = target
        .household
        .rooms
        .iter()
        .any(|r| r.coordinator == target.room.coordinator && !std::ptr::eq(r, target.room));
    if !shares_group {
        return Ok(Command::Nothing {
            reason: format!("{} already plays on its own", target.room.name),
        });
    }
    Ok(Command::Leave {
        member: target.player.id.clone(),
    })
}

/// `POST /dj/{start|skip|stop}` / the `dj_*` tools.
pub fn plan_dj(
    households: &[HouseholdState],
    req: &ZoneRequest,
    action: DjAction,
) -> Result<Command, Failure> {
    let target = resolve(households, req.zone()?)?;
    Ok(Command::Dj {
        coordinator: target.coordinator.id.clone(),
        action,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::zones::fixtures::{households, id};

    fn zone(name: &str) -> ZoneRequest {
        ZoneRequest { zone: name.into() }
    }

    #[test]
    fn group_commands_address_the_coordinator() {
        let houses = households();
        // Kitchen@S1 is a member of Den's group.
        assert_eq!(
            plan_transport(&houses, &zone("kitchen@S1"), TransportAction::Pause).unwrap(),
            Command::Transport {
                coordinator: id("RINCON_DEN"),
                action: TransportAction::Pause
            }
        );
        assert_eq!(
            plan_dj(&houses, &zone("Kitchen@S1"), DjAction::Start).unwrap(),
            Command::Dj {
                coordinator: id("RINCON_DEN"),
                action: DjAction::Start
            }
        );
        let play = PlayRequest {
            zone: "kitchen@s1".into(),
            source_uri: "https://open.spotify.com/album/0123456789ABCDEFabcdef?si=q".into(),
            title: Some(" Partita No. 2 ".into()),
        };
        assert_eq!(
            plan_play(&houses, &play).unwrap(),
            Command::Play {
                coordinator: id("RINCON_DEN"),
                source_uri: "spotify:album:0123456789ABCDEFabcdef".into(),
                title: Some("Partita No. 2".into()),
            }
        );
    }

    #[test]
    fn room_volume_addresses_the_room_group_volume_the_coordinator() {
        let houses = households();
        let mut req = VolumeRequest {
            zone: "Kitchen@S1".into(),
            volume: Some(25),
            delta: None,
            group: false,
        };
        assert_eq!(
            plan_volume(&houses, &req).unwrap(),
            Command::Volume {
                target: id("RINCON_KIT1"),
                scope: VolumeScope::Room,
                change: VolumeChange::Set(25)
            }
        );
        req.group = true;
        req.volume = None;
        req.delta = Some(-10);
        assert_eq!(
            plan_volume(&houses, &req).unwrap(),
            Command::Volume {
                target: id("RINCON_DEN"),
                scope: VolumeScope::Group,
                change: VolumeChange::Adjust(-10)
            }
        );
    }

    #[test]
    fn mute_addresses_the_room() {
        let req = MuteRequest {
            zone: "kitchen@s1".into(),
            mute: false,
        };
        assert_eq!(
            plan_mute(&households(), &req).unwrap(),
            Command::Mute {
                target: id("RINCON_KIT1"),
                mute: false
            }
        );
    }

    #[test]
    fn validation_runs_before_resolution() {
        // A bad volume is reported even though the room is also unknown.
        let req = VolumeRequest {
            zone: "Garage".into(),
            volume: Some(400),
            delta: None,
            group: false,
        };
        assert!(
            plan_volume(&households(), &req)
                .unwrap_err()
                .detail
                .contains("0 to 100")
        );
    }

    #[test]
    fn grouping_joins_the_destination_coordinator() {
        let houses = households();
        let req = GroupRequest {
            zone: "Ada's Studio".into(),
            to: "kitchen@s1".into(),
        };
        assert_eq!(
            plan_group(&houses, &req).unwrap(),
            Command::Join {
                member: id("RINCON_STU_L"),
                coordinator: id("RINCON_DEN")
            }
        );
    }

    #[test]
    fn grouping_is_idempotent_and_household_bound() {
        let houses = households();
        let already = GroupRequest {
            zone: "Kitchen@S1".into(),
            to: "Den".into(),
        };
        assert!(matches!(
            plan_group(&houses, &already).unwrap(),
            Command::Nothing { reason } if reason.contains("already plays in Den's group")
        ));
        let across = GroupRequest {
            zone: "Patio".into(),
            to: "Den".into(),
        };
        let err = plan_group(&houses, &across).unwrap_err();
        assert_eq!(
            (err.code, err.status()),
            (ErrorCode::CrossHouseholdGroup, 422)
        );
        assert!(err.detail.contains("different households"), "{err}");
    }

    #[test]
    fn ungroup_leaves_only_when_grouped() {
        let houses = households();
        assert_eq!(
            plan_ungroup(&houses, &zone("Kitchen@S1")).unwrap(),
            Command::Leave {
                member: id("RINCON_KIT1")
            }
        );
        assert!(matches!(
            plan_ungroup(&houses, &zone("Patio")).unwrap(),
            Command::Nothing { reason } if reason.contains("on its own")
        ));
    }

    #[test]
    fn nothing_discovered_is_a_retryable_503() {
        let err = plan_transport(&[], &zone("Den"), TransportAction::Resume).unwrap_err();
        assert_eq!((err.code, err.status()), (ErrorCode::NotReady, 503));
        assert!(err.retryable());
        let err = resolve(&[HouseholdState::default()], "Den").unwrap_err();
        assert!(err.detail.contains("retry"), "{err}");
    }

    #[test]
    fn resolution_failures_keep_core_details() {
        let houses = households();
        let err = plan_dj(&houses, &zone("Kitchen"), DjAction::Skip).unwrap_err();
        assert_eq!((err.code, err.status()), (ErrorCode::AmbiguousRoom, 409));
        assert_eq!(err.suggestions, ["Kitchen@S1", "Kitchen@S2"]);
        let err = plan_dj(&houses, &zone("  "), DjAction::Skip).unwrap_err();
        assert_eq!(err.status(), 422);
    }
}
