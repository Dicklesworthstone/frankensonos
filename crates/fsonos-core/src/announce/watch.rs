//! The clip's deadline never grants authority over a newer source.

mod events;

use super::{AnnouncementControl, Clip, Ending, GroupPlan, cancellation, recovery, refreshed};
use crate::live::LiveEvent;
use crate::{CoreError, HouseholdState, control, heal};
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_types::TransportState;
use std::net::IpAddr;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

const START_GRACE: Duration = Duration::from_secs(2);

/// Why watching ended before the clip did.
#[derive(Debug)]
pub(super) enum Interrupted {
    /// Newer playback or grouping, a pause, or a refusal: what the house is
    /// doing now is left alone.
    LeaveAlone(String),
    /// The players stopped answering (a Wi-Fi drop). The put-back checks for
    /// itself what the announcement still owns once they answer again.
    Unanswered(String),
}

impl From<CoreError> for Interrupted {
    fn from(e: CoreError) -> Self {
        if heal::is_unreachable(&e) {
            Self::Unanswered(e.to_string())
        } else {
            Self::LeaveAlone(e.to_string())
        }
    }
}

impl From<ProtoError> for Interrupted {
    fn from(e: ProtoError) -> Self {
        CoreError::from(e).into()
    }
}

impl From<String> for Interrupted {
    fn from(why: String) -> Self {
        Self::LeaveAlone(why)
    }
}

/// One deadline and ownership check shared by event and direct watchers.
struct Watch<'a, T: Transport + ?Sized> {
    transport: &'a T,
    household: &'a HouseholdState,
    plan: &'a GroupPlan,
    clip: &'a Clip,
    host: IpAddr,
    started: Instant,
    deadline: Duration,
    began: bool,
    /// Only a successful state read followed by an exact source read can rearm events.
    observed: Option<TransportState>,
    control: Option<&'a AnnouncementControl>,
}

impl<T: Transport + ?Sized> Watch<'_, T> {
    fn remaining(&self) -> Duration {
        self.deadline.saturating_sub(self.started.elapsed())
    }

    fn cancelled(&self) -> bool {
        self.control.is_some_and(AnnouncementControl::is_cancelled)
    }

    fn delay(&self, delay: Duration) -> Duration {
        if self.control.is_some() {
            delay.min(cancellation::CANCEL_POLL)
        } else {
            delay
        }
    }

    fn owns_source(&self) -> Result<(), Interrupted> {
        let media = soap::get_media_info(self.transport, self.host)?;
        if media.uri == self.clip.url {
            Ok(())
        } else {
            Err(Interrupted::LeaveAlone(
                "announcement superseded by another source; newer playback was left alone".into(),
            ))
        }
    }

    fn owns_group(&self) -> Result<(), Interrupted> {
        let now = refreshed(self.transport, self.household)?;
        recovery::check_group(&now, self.household, self.plan)?;
        Ok(())
    }

    /// A notification can request inspection, never authorize a stale restore.
    fn inspect(&mut self) -> Result<Option<Ending>, Interrupted> {
        self.observed = None;
        // Read identity after state, so a source change during that read wins.
        let read = soap::get_transport_info(self.transport, self.host)
            .map_err(Interrupted::from)
            .and_then(|info| self.owns_source().map(|()| info.state));
        let state = match read {
            Ok(state) => state,
            // A player may finish the clip while temporarily unreachable.
            Err(Interrupted::Unanswered(_)) if !self.remaining().is_zero() => return Ok(None),
            Err(interrupted) => return Err(interrupted),
        };
        self.observed = Some(state);
        // A manual pause retains its existing supersession semantics even
        // when a cancellation request happens at the same time.
        if self.cancelled() && state != TransportState::Paused {
            return Ok(Some(Ending::Cancelled));
        }
        match state {
            TransportState::Playing | TransportState::Transitioning => self.began = true,
            TransportState::Paused => {
                return Err(Interrupted::LeaveAlone(
                    "announcement was paused; previous playback was not resumed".into(),
                ));
            }
            TransportState::Stopped if self.began => return Ok(Some(Ending::Finished)),
            TransportState::Stopped if self.started.elapsed() >= START_GRACE => {
                return Ok(Some(Ending::NeverStarted));
            }
            _ => {}
        }
        if self.remaining().is_zero() {
            return Ok(Some(Ending::Deadline));
        }
        Ok(None)
    }

    fn cancellation_state(&self) -> Result<TransportState, Interrupted> {
        self.owns_group()?;
        let state = soap::get_transport_info(self.transport, self.host)?.state;
        self.owns_source()?;
        match state {
            TransportState::Playing | TransportState::Transitioning | TransportState::Stopped => {
                Ok(state)
            }
            TransportState::Paused => Err(Interrupted::LeaveAlone(
                "announcement was paused; cancellation left the newer intent alone".into(),
            )),
            _ => Err(Interrupted::LeaveAlone(
                "announcement transport is unknown; cancellation left playback alone".into(),
            )),
        }
    }

    /// Stop exactly once after fresh ownership reads. A lost reply can be
    /// proved by reads, but never causes another Stop or source replay.
    fn stop_cancelled(&self) -> Result<(), Interrupted> {
        if self.cancellation_state()? == TransportState::Stopped {
            return Ok(());
        }
        let sent = soap::stop(self.transport, self.host);
        if let Err(error) = &sent
            && !matches!(error, ProtoError::Network { .. } | ProtoError::Malformed(_))
        {
            return Err(Interrupted::LeaveAlone(error.to_string()));
        }
        match self.cancellation_state() {
            Ok(TransportState::Stopped) => Ok(()),
            Ok(_) => Err(Interrupted::LeaveAlone(
                "announcement cancellation did not establish a stopped clip; playback was left alone"
                    .into(),
            )),
            Err(Interrupted::Unanswered(why)) => Err(Interrupted::Unanswered(
                sent.err().map_or(why, |error| error.to_string()),
            )),
            Err(interrupted) => Err(interrupted),
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn wait<T: Transport + ?Sized>(
    transport: &T,
    household: &HouseholdState,
    plan: &GroupPlan,
    clip: &Clip,
    deadline: Duration,
    poll: Duration,
    incoming: Option<&Receiver<LiveEvent>>,
    control: Option<&AnnouncementControl>,
) -> Result<Ending, Interrupted> {
    let host = control::locate(std::slice::from_ref(household), &plan.lead)?.ip;
    let mut watch = Watch {
        transport,
        household,
        plan,
        clip,
        host,
        started: Instant::now(),
        deadline,
        began: false,
        observed: None,
        control,
    };
    if let Some(incoming) = incoming {
        return events::wait(&mut watch, incoming);
    }
    loop {
        if let Some(ending) = watch.inspect()? {
            if ending == Ending::Deadline {
                watch.owns_group()?;
                watch.owns_source()?;
                soap::stop(watch.transport, watch.host)?;
            } else if ending == Ending::Cancelled {
                watch.stop_cancelled()?;
            }
            return Ok(ending);
        }
        std::thread::sleep(watch.delay(poll.min(watch.remaining())));
    }
}
