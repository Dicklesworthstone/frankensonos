//! Keep staged playback addressed to the group and source that were reviewed.

use super::{Item, verify};
use crate::queue_source::destination::Destination;
use fsonos_proto::control::{self as soap, PositionInfo};
use fsonos_proto::play_mode::get_play_mode;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::net::IpAddr;
use std::time::Instant;

pub(super) struct Owner {
    destination: Destination,
    position: PositionInfo,
    state: TransportState,
    mode: Option<PlayMode>,
    observed: Instant,
}

#[derive(Clone, Copy)]
pub(super) enum Action<'a> {
    Load(&'a Item),
    Mode(PlayMode),
    Track(u32, &'a Item),
    Position(u32),
    Play,
}

impl Action<'_> {
    pub(super) fn name(self) -> &'static str {
        match self {
            Self::Load(_) => "SetAVTransportURI",
            Self::Mode(_) => "SetPlayMode",
            Self::Track(_, _) | Self::Position(_) => "Seek",
            Self::Play => "Play",
        }
    }
}

fn changed() -> ProtoError {
    ProtoError::Malformed(
        "the destination group, boot, source, selection, time, transport or play mode changed while staging the move".into(),
    )
}

impl Owner {
    pub(super) fn read<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        target: &PlayerId,
        mode_known: bool,
    ) -> Result<Self, ProtoError> {
        let destination = Destination::read(t, host, target)?;
        let observed = Instant::now();
        let position = soap::get_position_info(t, host)?;
        let state = soap::get_transport_info(t, host)?.state;
        if !matches!(
            state,
            TransportState::Playing | TransportState::Paused | TransportState::Stopped
        ) {
            return Err(changed());
        }
        let mode = mode_known.then(|| get_play_mode(t, host)).transpose()?;
        destination.confirm(t, host, target)?;
        Ok(Self {
            destination,
            position,
            state,
            mode,
            observed,
        })
    }

    fn selection_agrees(&self, other: &Self) -> bool {
        self.position.track == other.position.track
            && verify::identity(&self.position.uri) == verify::identity(&other.position.uri)
    }

    fn time_agrees(&self, other: &Self) -> bool {
        let forward = if self.state == TransportState::Playing {
            u32::try_from(self.observed.elapsed().as_secs())
                .unwrap_or(u32::MAX)
                .saturating_add(1)
        } else {
            0
        };
        match (self.position.position_secs, other.position.position_secs) {
            (Some(before), Some(after)) => {
                after >= before && after <= before.saturating_add(forward)
            }
            (None, None) => true,
            _ => false,
        }
    }

    pub(super) fn confirm<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        target: &PlayerId,
    ) -> Result<(), ProtoError> {
        let now = Self::read(t, host, target, self.mode.is_some())?;
        if self.destination != now.destination
            || self.mode != now.mode
            || self.state != now.state
            || !self.selection_agrees(&now)
            || !self.time_agrees(&now)
        {
            return Err(changed());
        }
        Ok(())
    }

    /// Adopt only the exact transition we dispatched. A helper that performed
    /// no write must preserve every field, even when another controller happens
    /// to select the same setting that the transfer wanted.
    pub(super) fn after_action<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        target: &PlayerId,
        action: Option<Action<'_>>,
        started: Instant,
    ) -> Result<Self, ProtoError> {
        let now = Self::read(t, host, target, self.mode.is_some())?;
        let source_matches = if matches!(action, Some(Action::Load(_))) {
            now.destination == self.destination.for_queue(target)
        } else {
            self.destination == now.destination
        };
        if !source_matches || !now.destination.is_queue(target) {
            return Err(changed());
        }
        let mode = match action {
            Some(Action::Mode(mode)) => Some(mode),
            _ => self.mode,
        };
        let selected = |slot, item: &Item| {
            now.position.track == slot
                && verify::identity(&now.position.uri) == verify::identity(&item.uri)
        };
        let playback = match action {
            Some(Action::Load(first)) => {
                now.state == TransportState::Stopped
                    && selected(1, first)
                    && now.position.position_secs == Some(0)
            }
            Some(Action::Track(slot, item)) => {
                now.state == self.state
                    && selected(slot, item)
                    && now.position.position_secs == Some(0)
            }
            Some(Action::Position(position)) => {
                now.state == self.state
                    && self.selection_agrees(&now)
                    && now.position.position_secs == Some(position)
            }
            Some(Action::Play) => {
                let forward = u32::try_from(started.elapsed().as_secs())
                    .unwrap_or(u32::MAX)
                    .saturating_add(1);
                now.state == TransportState::Playing
                    && self.selection_agrees(&now)
                    && self
                        .position
                        .position_secs
                        .zip(now.position.position_secs)
                        .is_some_and(|(before, after)| {
                            after >= before && after <= before.saturating_add(forward)
                        })
            }
            Some(Action::Mode(_)) | None => {
                now.state == self.state && self.selection_agrees(&now) && self.time_agrees(&now)
            }
        };
        if now.mode != mode || !playback {
            return Err(changed());
        }
        Ok(now)
    }

    /// Stop may reset the current clock. It cannot select another source or
    /// occurrence, change the group, or authorize an unrelated seek.
    pub(super) fn after_stop<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        target: &PlayerId,
    ) -> Result<Self, ProtoError> {
        let now = Self::read(t, host, target, self.mode.is_some())?;
        if self.destination != now.destination
            || self.mode != now.mode
            || now.state != TransportState::Stopped
            || !self.selection_agrees(&now)
            || !(now.position.position_secs == Some(0) || self.time_agrees(&now))
        {
            return Err(changed());
        }
        Ok(now)
    }

    /// Native prefix deletion can shift a selected queue occurrence to the
    /// first survivor. An inactive queue cannot change its source's position.
    pub(super) fn after_removal<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        target: &PlayerId,
        removed: usize,
        items: &[Item],
    ) -> Result<Self, ProtoError> {
        let now = Self::read(t, host, target, self.mode.is_some())?;
        if self.destination != now.destination
            || self.mode != now.mode
            || now.state != TransportState::Stopped
        {
            return Err(changed());
        }
        if removed == 0 || !self.destination.is_queue(target) {
            if !self.selection_agrees(&now) || !self.time_agrees(&now) {
                return Err(changed());
            }
        } else {
            let before = usize::try_from(self.position.track).map_err(|_| changed())?;
            let selected = if before == 0 {
                0
            } else {
                before.saturating_sub(removed).max(1)
            };
            let matches_item = selected
                .checked_sub(1)
                .and_then(|index| items.get(index))
                .is_some_and(|item| {
                    verify::identity(&item.uri) == verify::identity(&now.position.uri)
                });
            if usize::try_from(now.position.track).ok() != Some(selected)
                || (selected > 0 && !matches_item)
                || (selected == 0 && !now.position.uri.is_empty())
                || !(self.time_agrees(&now)
                    || (before > 0 && before <= removed && now.position.position_secs == Some(0)))
            {
                return Err(changed());
            }
        }
        Ok(now)
    }
}
