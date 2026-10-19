//! Ordered announcement observations, with one slow verification watchdog.

use super::{Ending, Interrupted, Watch, soap};
use crate::live::LiveEvent;
use crate::reconcile::Health;
use fsonos_proto::Transport;
use fsonos_types::{PlayerId, TransportState};
use std::sync::mpsc::{Receiver, RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

/// Silence, missing initial events, and a lost terminal event all recover
/// without returning to the direct watcher's four checks per second.
const VERIFY_AFTER: Duration = Duration::from_secs(2);
const EVENT_BURST: usize = 256;

#[allow(clippy::struct_excessive_bools)] // independent evidence and pending checks
#[derive(Default)]
struct Evidence {
    /// An ordered event or successful fresh source read named our exact clip.
    clip: bool,
    /// PLAYING/TRANSITIONING followed that clip, not a cached old source.
    began: bool,
    inspect: bool,
    topology: bool,
    /// Relevant evidence arrived after the current SOAP check began.
    changed: bool,
    disconnected: bool,
}

impl Evidence {
    fn lost(&mut self) {
        self.clip = false;
        self.began = false;
        self.inspect = true;
        self.changed = true;
    }

    /// A failed probe supplies no ownership. A successful probe restores it
    /// before processing the callbacks that arrived while the read ran.
    fn observed_source(&mut self, state: Option<TransportState>) {
        if let Some(state) = state {
            self.clip = true;
            self.began |= matches!(
                state,
                TransportState::Playing | TransportState::Transitioning
            );
        }
    }

    fn observe(
        &mut self,
        event: LiveEvent,
        player: &PlayerId,
        clip: &str,
    ) -> Result<(), Interrupted> {
        match event {
            LiveEvent::Playback {
                player: heard,
                changes,
            } if heard == *player => {
                let was_ours = self.clip;
                if let Some((_, uri)) = changes.track {
                    self.changed = true;
                    match uri.as_deref() {
                        Some(uri) if uri == clip => self.clip = true,
                        Some(_) if self.clip => {
                            return Err(Interrupted::LeaveAlone(
                                "announcement superseded by another source; newer playback was left alone"
                                    .into(),
                            ));
                        }
                        None if self.clip => {
                            // Some players clear the track at EOF. Verify the
                            // AVTransport source before treating that as an end;
                            // a blank track does not revoke its ownership.
                            self.inspect = true;
                        }
                        _ => {}
                    }
                }
                if let Some((before, state)) = changes.transport {
                    self.changed = true;
                    // A URI-only delta can inherit PLAYING. Its later edge
                    // describes our clip only when an earlier event named it.
                    if was_ours
                        && matches!(
                            before,
                            Some(TransportState::Playing | TransportState::Transitioning)
                        )
                    {
                        self.began = true;
                    }
                    if self.clip {
                        match state {
                            TransportState::Playing | TransportState::Transitioning => {
                                self.began = true;
                            }
                            TransportState::Paused if self.began => {
                                return Err(Interrupted::LeaveAlone(
                                    "announcement was paused; previous playback was not resumed"
                                        .into(),
                                ));
                            }
                            TransportState::Stopped if self.began => self.inspect = true,
                            _ => {}
                        }
                    } else if was_ours && self.began && state == TransportState::Stopped {
                        self.inspect = true;
                    }
                }
            }
            LiveEvent::Topology => {
                self.topology = true;
                self.changed = true;
            }
            LiveEvent::Health {
                player: heard,
                health,
            } if heard == *player && health != Health::Healthy => self.lost(),
            LiveEvent::Resync => self.lost(),
            _ => {}
        }
        Ok(())
    }

    /// Before adopting a SOAP completion, exhaust the known callback prefix.
    /// A bounded burst yields to the outer loop rather than skipping evidence.
    fn drain(
        &mut self,
        incoming: &Receiver<LiveEvent>,
        first: Option<LiveEvent>,
        player: &PlayerId,
        clip: &str,
    ) -> Result<bool, Interrupted> {
        let mut first = first;
        for _ in 0..EVENT_BURST {
            let event = match first.take().map_or_else(|| incoming.try_recv(), Ok) {
                Ok(event) => event,
                Err(TryRecvError::Empty) => return Ok(true),
                Err(TryRecvError::Disconnected) => {
                    if !self.disconnected {
                        self.lost();
                        self.disconnected = true;
                    }
                    return Ok(true);
                }
            };
            self.observe(event, player, clip)?;
        }
        Ok(false)
    }
}

pub(super) fn wait<T: Transport + ?Sized>(
    watch: &mut Watch<'_, T>,
    incoming: &Receiver<LiveEvent>,
) -> Result<Ending, Interrupted> {
    let mut evidence = Evidence::default();
    let mut first = None;
    let mut verify_at = Instant::now() + VERIFY_AFTER;
    loop {
        let settled = evidence.drain(incoming, first.take(), &watch.plan.lead, &watch.clip.url)?;
        if !settled {
            if watch.remaining().is_zero() {
                return Err(Interrupted::LeaveAlone(
                    "announcement callbacks did not settle before its deadline; newer playback was left alone"
                        .into(),
                ));
            }
            continue;
        }
        watch.began |= evidence.began;
        if evidence.topology {
            evidence.topology = false;
            match watch.owns_group() {
                Ok(()) => {}
                Err(Interrupted::Unanswered(_)) if !watch.remaining().is_zero() => {
                    evidence.inspect = true;
                }
                Err(interrupted) => return Err(interrupted),
            }
        }
        if evidence.inspect || Instant::now() >= verify_at || watch.remaining().is_zero() {
            evidence.inspect = false;
            evidence.changed = false;
            let ending = watch.inspect()?;
            evidence.observed_source(watch.observed);
            verify_at = Instant::now() + VERIFY_AFTER;
            if let Some(ending) = ending {
                // Current source and group still own the announcement. A
                // callback arriving during either read must get its turn too.
                watch.owns_group()?;
                watch.owns_source()?;
                let settled = evidence.drain(incoming, None, &watch.plan.lead, &watch.clip.url)?;
                if !settled || evidence.changed {
                    evidence.inspect = true;
                    continue;
                }
                if ending == Ending::Deadline {
                    soap::stop(watch.transport, watch.host)?;
                }
                return Ok(ending);
            }
        }
        let delay = verify_at
            .saturating_duration_since(Instant::now())
            .min(watch.remaining());
        if evidence.disconnected {
            std::thread::sleep(delay);
            continue;
        }
        match incoming.recv_timeout(delay) {
            Ok(event) => first = Some(event),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => {
                evidence.lost();
                evidence.disconnected = true;
            }
        }
    }
}

#[cfg(test)]
mod tests;
