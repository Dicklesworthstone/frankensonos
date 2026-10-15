//! The clip's deadline never grants authority over a newer source.

use super::{Clip, Ending, GroupPlan, recovery, refreshed};
use crate::{CoreError, HouseholdState, control, heal};
use fsonos_proto::{ProtoError, Transport, control as soap};
use fsonos_types::TransportState;
use std::time::{Duration, Instant};

const START_GRACE: Duration = Duration::from_secs(2);

/// Why watching ended before the clip did.
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

pub(super) fn wait<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    plan: &GroupPlan,
    clip: &Clip,
    deadline: Duration,
    poll: Duration,
) -> Result<Ending, Interrupted> {
    let host = control::locate(std::slice::from_ref(household), &plan.lead)
        .map_err(Interrupted::from)?
        .ip;
    let owns_source = || -> Result<(), Interrupted> {
        let media = soap::get_media_info(t, host)?;
        if media.uri == clip.url {
            Ok(())
        } else {
            Err(Interrupted::LeaveAlone(
                "announcement superseded by another source; newer playback was left alone".into(),
            ))
        }
    };
    let started = Instant::now();
    let mut began = false;
    loop {
        // Read identity after state, so a source change during that read wins.
        let read = soap::get_transport_info(t, host)
            .map_err(Interrupted::from)
            .and_then(|info| owns_source().map(|()| info.state));
        let state = match read {
            Ok(state) => state,
            // A player that stops answering mid-clip is waited for until
            // the clip's deadline: the clip may still be playing.
            Err(Interrupted::Unanswered(_)) if started.elapsed() < deadline => {
                std::thread::sleep(poll.min(deadline.saturating_sub(started.elapsed())));
                continue;
            }
            Err(interrupted) => return Err(interrupted),
        };
        match state {
            TransportState::Playing | TransportState::Transitioning => began = true,
            TransportState::Paused => {
                return Err(Interrupted::LeaveAlone(
                    "announcement was paused; previous playback was not resumed".into(),
                ));
            }
            TransportState::Stopped if began => return Ok(Ending::Finished),
            TransportState::Stopped if started.elapsed() >= START_GRACE => {
                return Ok(Ending::NeverStarted);
            }
            _ => {}
        }
        let left = deadline.saturating_sub(started.elapsed());
        if left.is_zero() {
            let now = refreshed(t, household).map_err(Interrupted::from)?;
            recovery::check_group(&now, household, plan)?;
            // Recheck immediately before Stop, not just on the preceding poll.
            owns_source()?;
            soap::stop(t, host)?;
            return Ok(Ending::Deadline);
        }
        std::thread::sleep(poll.min(left));
    }
}
