//! Planning: from a validated request to the [`Command`] the daemon carries
//! out, addressed to concrete players.
//!
//! This is where coordinator addressing happens for the surfaces. Transport,
//! playback and the DJ go to the coordinator of the room's group; room volume
//! goes to the room's own primary player, group volume to the coordinator;
//! joining targets the coordinator of the destination group. A request that is
//! already satisfied plans to [`Command::Nothing`] so retries are harmless.

use fsonos_core::favorites::{self, Favorite};
use fsonos_core::rooms::{Aliases, ResolveContext, resolve_one};
use fsonos_core::{ControlTarget, HouseholdState};
use fsonos_types::PlayerId;

use crate::dj::DjSteer;
use crate::failure::{ErrorCode, Failure};
use crate::request::zone_name;
use crate::request::{
    DjStartRequest, DjSteerRequest, GroupRequest, MoveRequest, MuteRequest, PartyRequest,
    PlayFavoriteRequest, PlayRequest, VolumeChange, VolumeRequest, ZoneRequest,
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
    /// Play a Sonos favorite (a track, a station, or a container that
    /// replaces the queue) in the group `coordinator` leads.
    PlayFavorite {
        coordinator: PlayerId,
        favorite: Favorite,
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
    /// Move the music room `from` plays to room `to` (both primaries),
    /// handing the group over, regrouping, or with `copy` replaying it there.
    Move {
        from: PlayerId,
        to: PlayerId,
        copy: bool,
    },
    /// Group every room of the household `member` belongs to, under `lead`'s
    /// group (`None`: the group playing now, else the first room).
    Party {
        member: PlayerId,
        lead: Option<PlayerId>,
    },
    Dj {
        coordinator: PlayerId,
        action: DjAction,
    },
    /// Replace or clear the DJ steering of the group `coordinator` leads,
    /// then, with `start`, start the DJ there.
    DjSteer {
        coordinator: PlayerId,
        steer: DjSteer,
        start: bool,
    },
    /// Already in the requested state; nothing to send.
    Nothing { reason: String },
}

impl Command {
    /// Whether sending this again is harmless even if the first request ran
    /// and only its reply was lost: it sets a state (play this, pause, this
    /// volume, mute, join, leave) rather than stepping (next, previous, a
    /// relative volume) or adding. Only these are retried at a player's new
    /// address (`fsonos_core::heal`).
    #[must_use]
    pub fn repeat_safe(&self) -> bool {
        match self {
            Self::Play { .. }
            | Self::PlayFavorite { .. }
            | Self::Mute { .. }
            | Self::Join { .. }
            | Self::Leave { .. }
            | Self::Party { .. } => true,
            Self::Transport { action, .. } => {
                matches!(action, TransportAction::Pause | TransportAction::Resume)
            }
            Self::Volume { change, .. } => matches!(change, VolumeChange::Set(_)),
            // Steering sets the stored session; starting queues more works.
            Self::DjSteer { start, .. } => !start,
            // A move that ran once has nothing left to move.
            Self::Move { .. } | Self::Dj { .. } | Self::Nothing { .. } => false,
        }
    }

    /// The player the command's request is sent to.
    #[must_use]
    pub fn addressed(&self) -> Option<&PlayerId> {
        match self {
            Self::Play { coordinator, .. }
            | Self::PlayFavorite { coordinator, .. }
            | Self::Transport { coordinator, .. }
            | Self::Dj { coordinator, .. }
            | Self::DjSteer { coordinator, .. } => Some(coordinator),
            Self::Volume { target, .. } | Self::Mute { target, .. } => Some(target),
            Self::Join { member, .. } | Self::Leave { member } => Some(member),
            Self::Move { from, .. } => Some(from),
            Self::Party { .. } | Self::Nothing { .. } => None,
        }
    }

    /// The coordinator a group command is addressed to (`None` for room
    /// commands and grouping).
    #[must_use]
    pub fn group_coordinator(&self) -> Option<&PlayerId> {
        match self {
            Self::Play { coordinator, .. }
            | Self::PlayFavorite { coordinator, .. }
            | Self::Transport { coordinator, .. }
            | Self::Dj { coordinator, .. }
            | Self::DjSteer { coordinator, .. } => Some(coordinator),
            Self::Volume {
                target,
                scope: VolumeScope::Group,
                ..
            } => Some(target),
            _ => None,
        }
    }

    /// The same group command, sent to `to` (the group's new coordinator).
    #[must_use]
    pub fn on_coordinator(mut self, to: &PlayerId) -> Self {
        match &mut self {
            Self::Play { coordinator, .. }
            | Self::PlayFavorite { coordinator, .. }
            | Self::Transport { coordinator, .. }
            | Self::Dj { coordinator, .. }
            | Self::DjSteer { coordinator, .. } => coordinator.clone_from(to),
            Self::Volume {
                target,
                scope: VolumeScope::Group,
                ..
            } => target.clone_from(to),
            _ => {}
        }
        self
    }
}

/// The households a request's rooms resolve in, with the owner's aliases
/// (`aliases.toml`) and the caller, whose default room `here` names. Plain
/// households (`&[HouseholdState]`, `&Vec<_>`) are rooms without aliases.
#[derive(Debug, Clone, Copy)]
pub struct Rooms<'a> {
    pub households: &'a [HouseholdState],
    pub aliases: Option<&'a Aliases>,
    /// The caller's policy key (`cli`, a tailnet login, ...), for `here`.
    pub client: Option<&'a str>,
}

impl<'a> From<&'a [HouseholdState]> for Rooms<'a> {
    fn from(households: &'a [HouseholdState]) -> Self {
        Self {
            households,
            aliases: None,
            client: None,
        }
    }
}

impl<'a> From<&'a Vec<HouseholdState>> for Rooms<'a> {
    fn from(households: &'a Vec<HouseholdState>) -> Self {
        Self::from(households.as_slice())
    }
}

impl<'a, const N: usize> From<&'a [HouseholdState; N]> for Rooms<'a> {
    fn from(households: &'a [HouseholdState; N]) -> Self {
        Self::from(households.as_slice())
    }
}

impl<'a> From<&Rooms<'a>> for Rooms<'a> {
    fn from(rooms: &Rooms<'a>) -> Self {
        *rooms
    }
}

/// Resolve a room the way every surface does: `503` while nothing has been
/// discovered (retrying helps), else the core's resolution (names,
/// `Name@Label`, ids, the owner's aliases, `here`, unique prefixes) with
/// its retry-able `404`/`409` details. A request names one room; an alias
/// for several is ambiguous here.
pub fn resolve<'a>(rooms: impl Into<Rooms<'a>>, room: &str) -> Result<ControlTarget<'a>, Failure> {
    let rooms = rooms.into();
    if rooms.households.iter().all(|h| h.rooms.is_empty()) {
        return Err(Failure::new(
            ErrorCode::NotReady,
            "no rooms discovered yet; discovery may still be running, retry in a few seconds",
        ));
    }
    let ctx = ResolveContext {
        aliases: rooms.aliases,
        client: rooms.client,
    };
    resolve_one(rooms.households, room, ctx).map_err(Failure::from)
}

/// `POST /play` / the `play` tool.
pub fn plan_play<'a>(rooms: impl Into<Rooms<'a>>, req: &PlayRequest) -> Result<Command, Failure> {
    let req = req.normalized()?;
    let target = resolve(rooms, &req.zone)?;
    Ok(Command::Play {
        coordinator: target.coordinator.id.clone(),
        source_uri: req.source_uri,
        title: req.title,
    })
}

/// `POST /play/favorite` / the `play_favorite` tool, given the favorites of
/// the zone's household (`fsonos_core::favorites::list`).
pub fn plan_play_favorite<'a>(
    rooms: impl Into<Rooms<'a>>,
    req: &PlayFavoriteRequest,
    household_favorites: &[Favorite],
) -> Result<Command, Failure> {
    let query = req.favorite()?;
    let target = resolve(rooms, req.zone()?)?;
    // An `FV:2/<n>` id (what list_favorites and search_library hand out)
    // names its favorite exactly; anything else is a position or a title.
    let favorite = match household_favorites.iter().find(|f| f.id == query) {
        Some(f) => f,
        None => favorites::find(household_favorites, query)?,
    };
    Ok(Command::PlayFavorite {
        coordinator: target.coordinator.id.clone(),
        favorite: favorite.clone(),
    })
}

/// `POST /pause|resume|next|previous` / the matching tools.
pub fn plan_transport<'a>(
    rooms: impl Into<Rooms<'a>>,
    req: &ZoneRequest,
    action: TransportAction,
) -> Result<Command, Failure> {
    let target = resolve(rooms, req.zone()?)?;
    Ok(Command::Transport {
        coordinator: target.coordinator.id.clone(),
        action,
    })
}

/// `POST /volume` / the `set_volume` tool.
pub fn plan_volume<'a>(
    rooms: impl Into<Rooms<'a>>,
    req: &VolumeRequest,
) -> Result<Command, Failure> {
    let change = req.change()?;
    let target = resolve(rooms, req.zone()?)?;
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
pub fn plan_mute<'a>(rooms: impl Into<Rooms<'a>>, req: &MuteRequest) -> Result<Command, Failure> {
    let target = resolve(rooms, req.zone()?)?;
    Ok(Command::Mute {
        target: target.player.id.clone(),
        mute: req.mute,
    })
}

/// `POST /group` / the `group` tool: move `zone` into `to`'s group.
pub fn plan_group<'a>(rooms: impl Into<Rooms<'a>>, req: &GroupRequest) -> Result<Command, Failure> {
    let (zone, to) = req.zones()?;
    let rooms = rooms.into();
    let mover = resolve(rooms, zone)?;
    let dest = resolve(rooms, to)?;
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
pub fn plan_ungroup<'a>(
    rooms: impl Into<Rooms<'a>>,
    req: &ZoneRequest,
) -> Result<Command, Failure> {
    let target = resolve(rooms, req.zone()?)?;
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

/// `POST /move` / the `move_playback` tool. Rooms in different households
/// can never group, so a move between them must be a copy.
pub fn plan_move<'a>(rooms: impl Into<Rooms<'a>>, req: &MoveRequest) -> Result<Command, Failure> {
    let rooms = rooms.into();
    let (zone, to) = req.zones()?;
    let from = resolve(rooms, zone)?;
    let dest = resolve(rooms, to)?;
    if from.room.primary == dest.room.primary {
        return Ok(Command::Nothing {
            reason: format!("the music is already in {}", from.room.name),
        });
    }
    if !req.copy && !std::ptr::eq(from.household, dest.household) {
        return Err(Failure::new(
            ErrorCode::CrossHouseholdGroup,
            format!(
                "{} and {} are in different households, which can never be grouped",
                from.room.name, dest.room.name
            ),
        )
        .with_hint("Copy the music there instead: copy true (fsonos move --copy)."));
    }
    Ok(Command::Move {
        from: from.room.primary.clone(),
        to: dest.room.primary.clone(),
        copy: req.copy,
    })
}

/// `POST /party` / the `group_all` tool.
pub fn plan_party<'a>(rooms: impl Into<Rooms<'a>>, req: &PartyRequest) -> Result<Command, Failure> {
    let rooms = rooms.into();
    if let Some(zone) = req.zone.as_deref() {
        if req.household.is_some() {
            return Err(Failure::invalid(
                "give either `zone` (the room that leads) or `household`, not both",
            ));
        }
        let lead = resolve(rooms, zone_name("zone", zone)?)?;
        return Ok(Command::Party {
            member: lead.room.primary.clone(),
            lead: Some(lead.room.primary.clone()),
        });
    }
    let households = rooms.households;
    let labels = fsonos_core::rooms::household_labels(households);
    let index = match req.household.as_deref().map(str::trim) {
        Some(wanted) => labels
            .iter()
            .position(|l| l.eq_ignore_ascii_case(wanted))
            .ok_or_else(|| {
                Failure::new(
                    ErrorCode::UnknownHousehold,
                    format!("no household {wanted:?}"),
                )
                .with_suggestions(labels.clone())
            })?,
        None if households.len() == 1 => 0,
        None => {
            return Err(Failure::invalid(
                "name the room that leads, or the household (S1, S2), for the party",
            )
            .with_suggestions(labels));
        }
    };
    let member = households[index]
        .rooms
        .first()
        .map(|r| r.primary.clone())
        .ok_or_else(|| {
            Failure::new(
                ErrorCode::NotReady,
                "that household has no rooms discovered yet",
            )
        })?;
    Ok(Command::Party { member, lead: None })
}

/// `POST /dj/{start|skip|stop}` / the `dj_*` tools.
pub fn plan_dj<'a>(
    rooms: impl Into<Rooms<'a>>,
    req: &ZoneRequest,
    action: DjAction,
) -> Result<Command, Failure> {
    let target = resolve(rooms, req.zone()?)?;
    Ok(Command::Dj {
        coordinator: target.coordinator.id.clone(),
        action,
    })
}

/// `POST /dj/start` / the `dj_start` tool: with a mood, steer the group and
/// start the DJ in one go.
pub fn plan_dj_start<'a>(
    rooms: impl Into<Rooms<'a>>,
    req: &DjStartRequest,
) -> Result<Command, Failure> {
    let (zone, steer) = (req.zone()?, req.steer()?);
    let coordinator = resolve(rooms, zone)?.coordinator.id.clone();
    Ok(match steer {
        Some(steer) => Command::DjSteer {
            coordinator,
            steer,
            start: true,
        },
        None => Command::Dj {
            coordinator,
            action: DjAction::Start,
        },
    })
}

/// `POST /dj/steer` / the `dj_steer` tool. Steering is the group's: it is
/// stored under the group's coordinator.
pub fn plan_dj_steer<'a>(
    rooms: impl Into<Rooms<'a>>,
    req: &DjSteerRequest,
) -> Result<Command, Failure> {
    let (zone, steer) = (req.zone()?, req.steer()?);
    Ok(Command::DjSteer {
        coordinator: resolve(rooms, zone)?.coordinator.id.clone(),
        steer,
        start: false,
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
    fn a_favorite_is_found_by_id_position_or_title() {
        let houses = households();
        let fav = |id: &str, title: &str| Favorite {
            id: id.into(),
            title: title.into(),
            kind: fsonos_core::favorites::FavoriteKind::Stream,
            uri: Some(format!("x-rincon-mp3radio://{id}")),
            metadata: String::new(),
            description: None,
            art_uri: None,
        };
        let list = [fav("FV:2/4", "Sim Radio"), fav("FV:2/1", "Symphonies")];
        let pick = |query: &str| {
            let req = PlayFavoriteRequest {
                zone: "Den".into(),
                favorite: query.into(),
            };
            match plan_play_favorite(&houses, &req, &list).unwrap() {
                Command::PlayFavorite { favorite, .. } => favorite.id,
                other => panic!("{other:?}"),
            }
        };
        assert_eq!(pick("FV:2/1"), "FV:2/1");
        assert_eq!(pick("FV:2/4"), "FV:2/4");
        assert_eq!(pick("2"), "FV:2/1");
        assert_eq!(pick("sim radio"), "FV:2/4");
    }

    #[test]
    fn moves_need_two_rooms_and_a_copy_across_households() {
        let houses = households();
        let mv = |zone: &str, to: &str, copy: bool| {
            plan_move(
                &houses,
                &MoveRequest {
                    zone: zone.into(),
                    to: to.into(),
                    copy,
                },
            )
        };
        assert_eq!(
            mv("Den", "Kitchen@S1", false).unwrap(),
            Command::Move {
                from: id("RINCON_DEN"),
                to: id("RINCON_KIT1"),
                copy: false
            }
        );
        assert!(matches!(
            mv("Den", "den", false).unwrap(),
            Command::Nothing { .. }
        ));
        let across = mv("Den", "Patio", false).unwrap_err();
        assert_eq!(across.code, ErrorCode::CrossHouseholdGroup);
        assert!(across.hint.contains("copy"));
        assert!(matches!(
            mv("Den", "Patio", true).unwrap(),
            Command::Move { copy: true, .. }
        ));
    }

    #[test]
    fn a_party_is_led_by_a_room_or_names_its_household() {
        let houses = households();
        let party = |zone: Option<&str>, household: Option<&str>| {
            plan_party(
                &houses,
                &PartyRequest {
                    zone: zone.map(str::to_string),
                    household: household.map(str::to_string),
                },
            )
        };
        assert_eq!(
            party(Some("Patio"), None).unwrap(),
            Command::Party {
                member: id("RINCON_PATIO"),
                lead: Some(id("RINCON_PATIO"))
            }
        );
        assert!(matches!(
            party(None, Some("s1")).unwrap(),
            Command::Party { lead: None, .. }
        ));
        let which = party(None, None).unwrap_err();
        assert_eq!(which.code, ErrorCode::InvalidArgument);
        assert_eq!(which.suggestions, ["S1", "S2"]);
        assert_eq!(
            party(None, Some("S9")).unwrap_err().code,
            ErrorCode::UnknownHousehold
        );
    }

    #[test]
    fn only_state_setting_commands_are_repeat_safe() {
        let c = || id("RINCON_DEN");
        let transport = |action| Command::Transport {
            coordinator: c(),
            action,
        };
        let volume = |change| Command::Volume {
            target: c(),
            scope: VolumeScope::Group,
            change,
        };
        assert!(transport(TransportAction::Pause).repeat_safe());
        assert!(transport(TransportAction::Resume).repeat_safe());
        assert!(!transport(TransportAction::Next).repeat_safe());
        assert!(!transport(TransportAction::Previous).repeat_safe());
        assert!(volume(VolumeChange::Set(20)).repeat_safe());
        assert!(!volume(VolumeChange::Adjust(5)).repeat_safe());
        assert!(Command::Leave { member: c() }.repeat_safe());
        assert!(
            !Command::Dj {
                coordinator: c(),
                action: DjAction::Skip
            }
            .repeat_safe()
        );
        let steer = |start| Command::DjSteer {
            coordinator: c(),
            steer: DjSteer::Clear,
            start,
        };
        assert!(steer(false).repeat_safe());
        assert!(!steer(true).repeat_safe());
        assert_eq!(
            steer(false).on_coordinator(&id("RINCON_KIT1")).addressed(),
            Some(&id("RINCON_KIT1"))
        );
        // A group command can be moved to the new coordinator; a room
        // command cannot.
        let moved = volume(VolumeChange::Set(20)).on_coordinator(&id("RINCON_KIT1"));
        assert_eq!(moved.group_coordinator(), Some(&id("RINCON_KIT1")));
        let room = Command::Mute {
            target: c(),
            mute: true,
        };
        assert_eq!(room.group_coordinator(), None);
        assert_eq!(room.addressed(), Some(&c()));
    }

    #[test]
    fn dj_steering_is_the_groups() {
        let houses = households();
        // Kitchen@S1 plays in Den's group: its steering is stored under Den.
        let req = DjSteerRequest {
            zone: "kitchen@s1".into(),
            mood: Some("Focus".into()),
            ..DjSteerRequest::default()
        };
        assert_eq!(
            plan_dj_steer(&houses, &req).unwrap(),
            Command::DjSteer {
                coordinator: id("RINCON_DEN"),
                steer: DjSteer::Set {
                    mood: Some("focus".into()),
                    constraints: crate::dj::SteerConstraints::default(),
                    for_secs: None,
                },
                start: false,
            }
        );
        // The request is checked before the room is looked for.
        let bad = DjSteerRequest {
            zone: "Nowhere".into(),
            ..DjSteerRequest::default()
        };
        assert_eq!(
            plan_dj_steer(&houses, &bad).unwrap_err().code,
            ErrorCode::InvalidArgument
        );
        let start = |mood: Option<&str>| DjStartRequest {
            zone: "Den".into(),
            mood: mood.map(str::to_owned),
            for_secs: None,
        };
        assert_eq!(
            plan_dj_start(&houses, &start(None)).unwrap(),
            Command::Dj {
                coordinator: id("RINCON_DEN"),
                action: DjAction::Start
            }
        );
        assert!(matches!(
            plan_dj_start(&houses, &start(Some("calm"))).unwrap(),
            Command::DjSteer { start: true, .. }
        ));
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
