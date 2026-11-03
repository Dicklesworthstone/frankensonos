//! A direct source relocation keeps its destination through Stop, load, seek
//! and Play. Every mutation is sent once; ambiguous replies advance only when
//! the exact transition is observed. Live radio needs no queue-style clock.

use super::{Item, MoveError, Prepared, Source, invalid, verify};
use crate::moving::JOIN_DEADLINE;
use crate::queue_source::destination::Destination;
use fsonos_proto::content::{self, BrowseFlag, QUEUE};
use fsonos_proto::control::{self as soap, PositionInfo};
use fsonos_proto::play_mode::get_play_mode;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayMode, PlayerId, TransportState};
use std::net::IpAddr;
use std::time::{Duration, Instant};

#[derive(PartialEq, Eq)]
struct Queue {
    revision: u32,
    count: u32,
    mode: Option<PlayMode>,
}

struct Playback {
    destination: Destination,
    queue: Option<Queue>,
    position: PositionInfo,
    state: TransportState,
    observed: Instant,
    positioned: bool,
}

#[derive(Clone, Copy)]
enum Step<'a> {
    Stop,
    Load(&'a Item, bool),
    Seek(u32),
    Play,
}

fn stable(state: TransportState) -> bool {
    matches!(
        state,
        TransportState::Playing | TransportState::Paused | TransportState::Stopped
    )
}

fn changed() -> MoveError {
    invalid("the relocation group, boot, source or playback changed; no further writes were sent")
}

impl Playback {
    fn read<T: Transport + ?Sized>(
        t: &T,
        host: IpAddr,
        target: &PlayerId,
        mode_known: bool,
    ) -> Result<Self, MoveError> {
        let observed = Instant::now();
        let destination = Destination::read(t, host, target)?;
        let position = soap::get_position_info(t, host)?;
        let state = soap::get_transport_info(t, host)?.state;
        let queue = if destination.is_queue(target) {
            let page = content::browse(t, host, QUEUE, BrowseFlag::DirectChildren, 0, 1)?;
            Some(Queue {
                revision: page.update_id,
                count: page.total_matches,
                mode: mode_known.then(|| get_play_mode(t, host)).transpose()?,
            })
        } else {
            None
        };
        destination.confirm(t, host, target)?;
        let positioned = position.duration_secs.is_some_and(|secs| secs > 0);
        Ok(Self {
            destination,
            queue,
            position,
            state,
            observed,
            positioned,
        })
    }

    /// Track URI can differ from CurrentURI, particularly for live radio.
    /// Only a queue or a duration-bearing item provides selection evidence.
    fn has_selection(&self) -> bool {
        self.queue.is_some() || self.positioned
    }

    fn selection_agrees(&self, now: &Self) -> bool {
        !self.has_selection()
            || (self.position.track == now.position.track
                && verify::identity(&self.position.uri) == verify::identity(&now.position.uri))
    }

    fn time_agrees(&self, now: &Self, advancing: bool) -> bool {
        if !self.has_selection() {
            return true;
        }
        let forward = if advancing {
            u32::try_from(self.observed.elapsed().as_secs())
                .unwrap_or(u32::MAX)
                .saturating_add(1)
        } else {
            0
        };
        match (self.position.position_secs, now.position.position_secs) {
            (Some(before), Some(after)) => {
                after >= before && after <= before.saturating_add(forward)
            }
            (None, None) => true,
            _ => false,
        }
    }

    fn known_mode(&self) -> bool {
        self.queue
            .as_ref()
            .is_some_and(|queue| queue.mode.is_some())
    }

    fn same_source(&self, now: &Self) -> bool {
        self.destination == now.destination && self.queue == now.queue
    }

    fn confirm<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        target: &PlayerId,
    ) -> Result<(), MoveError> {
        let now = Self::read(t, host, target, self.known_mode())?;
        if !stable(self.state)
            || !self.same_source(&now)
            || self.state != now.state
            || !self.selection_agrees(&now)
            || !self.time_agrees(&now, self.state == TransportState::Playing)
        {
            return Err(changed());
        }
        Ok(())
    }

    fn accepts(&self, now: &Self, step: Step<'_>) -> bool {
        match step {
            Step::Load(item, positioned) => {
                self.destination.same_group(&now.destination)
                    && now.destination.has_source(&item.uri, &item.metadata)
                    && now.state == TransportState::Stopped
                    // A seekable source starts at zero; an unrelated seek
                    // after loading must not become this transfer's baseline.
                    // Clockless radio need not expose either track 1 or time 0.
                    && (!positioned
                        || (now.position.position_secs == Some(0)
                            && verify::identity(&now.position.uri) == verify::identity(&item.uri)))
            }
            Step::Stop => {
                self.same_source(now)
                    && self.selection_agrees(now)
                    && now.state == TransportState::Stopped
                    && (now.position.position_secs == Some(0)
                        || self.time_agrees(now, self.state == TransportState::Playing))
            }
            Step::Seek(position) => {
                self.same_source(now)
                    && self.selection_agrees(now)
                    && now.state == self.state
                    && now.position.position_secs == Some(position)
            }
            Step::Play => {
                self.same_source(now)
                    && self.selection_agrees(now)
                    && self.time_agrees(now, true)
                    && matches!(
                        now.state,
                        TransportState::Playing | TransportState::Transitioning
                    )
            }
        }
    }

    fn after<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        target: &PlayerId,
        step: Step<'_>,
        started: Instant,
    ) -> Result<Self, MoveError> {
        loop {
            let mut now = Self::read(t, host, target, self.known_mode())?;
            if !self.accepts(&now, step) {
                return Err(changed());
            }
            if !matches!(step, Step::Play) || now.state == TransportState::Playing {
                now.positioned |= match step {
                    Step::Load(_, positioned) => positioned,
                    _ => self.positioned,
                };
                return Ok(now);
            }
            // Only TRANSITIONING warrants waiting. A newer pause or stop is
            // not an invitation to wait for someone else to start playback.
            let remaining = JOIN_DEADLINE.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                return Err(invalid("the relocated source did not finish starting"));
            }
            std::thread::sleep(Duration::from_millis(100).min(remaining));
        }
    }

    fn step<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        target: &PlayerId,
        step: Step<'_>,
    ) -> Result<Self, MoveError> {
        self.confirm(t, host, target)?;
        let started = Instant::now();
        let sent = match step {
            Step::Stop => soap::stop(t, host),
            Step::Load(item, _) => soap::set_av_transport_uri(t, host, &item.uri, &item.metadata),
            Step::Seek(position) => soap::seek_position(t, host, position),
            Step::Play => soap::play(t, host),
        };
        let uncertain = match sent {
            Ok(()) => None,
            Err(error @ (ProtoError::Network { .. } | ProtoError::Malformed(_))) => Some(error),
            Err(error) => return Err(error.into()),
        };
        match self.after(t, host, target, step, started) {
            Ok(now) => Ok(now),
            Err(error) => Err(uncertain.map_or(error, MoveError::from)),
        }
    }
}

pub(super) fn apply<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    target: &PlayerId,
    prepared: &Prepared,
) -> Result<(), MoveError> {
    let Source::Uri { item, position } = &prepared.source else {
        return Err(invalid("direct relocation requires a prepared URI"));
    };
    let mut playback = Playback::read(t, host, target, false)?;
    playback = playback.step(t, host, target, Step::Stop)?;
    playback = playback.step(t, host, target, Step::Load(item, position.is_some()))?;
    if let Some(position) = position.filter(|position| *position > 0) {
        playback = playback.step(t, host, target, Step::Seek(position))?;
    }
    if prepared.transport == TransportState::Playing {
        playback = playback.step(t, host, target, Step::Play)?;
    }
    playback.confirm(t, host, target)
}

/// The original prepared source must still be present before its release.
/// Keep that proof across the final Stop too, including queued origins; its
/// revision/mode/selection cannot be adopted from a newer controller.
pub(super) fn release<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    origin: &PlayerId,
    positioned: bool,
    mode_known: bool,
    verify_origin: impl FnOnce() -> Result<(), MoveError>,
) -> Result<(), MoveError> {
    let mut playback = Playback::read(t, host, origin, mode_known)?;
    playback.positioned |= positioned;
    verify_origin()?;
    playback.step(t, host, origin, Step::Stop).map(drop)
}
