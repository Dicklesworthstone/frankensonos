//! The clip's deadline never grants authority over a newer source.

use super::{Clip, Ending, GroupPlan, recovery, refreshed};
use crate::{HouseholdState, control};
use fsonos_proto::{Transport, control as soap};
use fsonos_types::TransportState;
use std::time::{Duration, Instant};

const START_GRACE: Duration = Duration::from_secs(2);

pub(super) fn wait<T: Transport + ?Sized>(
    t: &T,
    household: &HouseholdState,
    plan: &GroupPlan,
    clip: &Clip,
    deadline: Duration,
    poll: Duration,
) -> Result<Ending, String> {
    let host = control::locate(std::slice::from_ref(household), &plan.lead)
        .map_err(|e| e.to_string())?
        .ip;
    let owns_source = || -> Result<(), String> {
        let media = soap::get_media_info(t, host).map_err(|e| e.to_string())?;
        if media.uri == clip.url {
            Ok(())
        } else {
            Err("announcement superseded by another source; newer playback was left alone".into())
        }
    };
    let started = Instant::now();
    let mut began = false;
    loop {
        let state = soap::get_transport_info(t, host)
            .map_err(|e| e.to_string())?
            .state;
        // Read identity after state, so a source change during that read wins.
        owns_source()?;
        match state {
            TransportState::Playing | TransportState::Transitioning => began = true,
            TransportState::Paused => {
                return Err("announcement was paused; previous playback was not resumed".into());
            }
            TransportState::Stopped if began => return Ok(Ending::Finished),
            TransportState::Stopped if started.elapsed() >= START_GRACE => {
                return Ok(Ending::NeverStarted);
            }
            _ => {}
        }
        let left = deadline.saturating_sub(started.elapsed());
        if left.is_zero() {
            let now = refreshed(t, household).map_err(|e| e.to_string())?;
            recovery::check_group(&now, household, plan)?;
            // Recheck immediately before Stop, not just on the preceding poll.
            owns_source()?;
            soap::stop(t, host).map_err(|e| e.to_string())?;
            return Ok(Ending::Deadline);
        }
        std::thread::sleep(poll.min(left));
    }
}
