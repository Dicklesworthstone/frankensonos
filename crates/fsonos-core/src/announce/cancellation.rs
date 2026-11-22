//! Process-local cancellation and progress for one announcement invocation.

use super::{AnnounceError, HouseholdAnnouncement};
use crate::{CoreError, HouseholdState, Room};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{HouseholdId, PlayerId};
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, PoisonError};

/// Local cancellation checks do not add speaker polling.
pub(super) use super::turns::CANCEL_POLL;
const CANCELLED: &str = "announcement cancellation requested";

/// Where one household is in an announcement. `Complete` means its
/// restoration and completion callback have returned; consult the final
/// report for success, cancellation or a partial restoration.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AnnouncementPhase {
    Queued,
    Preparing,
    Playing,
    Restoring,
    Complete,
}

/// A household's current announcement phase. No speech text, clip URL or
/// uploaded audio is retained in this observation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnnouncementProgress {
    pub household: Option<HouseholdId>,
    pub lead: PlayerId,
    pub rooms: Vec<String>,
    pub phase: AnnouncementPhase,
}

#[derive(Debug, Default)]
struct Progress {
    started: bool,
    complete: bool,
    households: Vec<AnnouncementProgress>,
}

#[derive(Debug, Default)]
struct Shared {
    cancelled: AtomicBool,
    progress: Mutex<Progress>,
}

/// A cancellation request and bounded progress for exactly one invocation.
/// The surface owns its job id, caller authorization and final receipt.
/// Cancellation requests are cooperative: an in-flight network operation
/// completes before the announcer can inspect the flag and restore safely.
#[derive(Debug, Clone, Default)]
pub struct AnnouncementControl(Arc<Shared>);

impl AnnouncementControl {
    /// Request cancellation once. `true` means this is the first request
    /// before completion, not that playback has stopped or been restored.
    #[must_use]
    pub fn cancel(&self) -> bool {
        let progress = self
            .0
            .progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        !progress.complete && !self.0.cancelled.swap(true, Ordering::AcqRel)
    }

    #[must_use]
    pub fn is_cancelled(&self) -> bool {
        self.0.cancelled.load(Ordering::Acquire)
    }

    pub(super) fn cancellation_flag(&self) -> &AtomicBool {
        &self.0.cancelled
    }

    /// The latest phase of each admitted household, without speaker I/O.
    #[must_use]
    pub fn progress(&self) -> Vec<AnnouncementProgress> {
        self.0
            .progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .households
            .clone()
    }

    pub(super) fn begin(
        &self,
        targets: &[(&HouseholdState, Vec<&Room>)],
    ) -> Result<(), AnnounceError> {
        let mut progress = self
            .0
            .progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if progress.started {
            return Err(AnnounceError::ControlReused);
        }
        progress.started = true;
        progress.households = targets
            .iter()
            .map(|(household, rooms)| AnnouncementProgress {
                household: household.id.clone(),
                lead: rooms[0].primary.clone(),
                rooms: rooms.iter().map(|room| room.name.clone()).collect(),
                phase: AnnouncementPhase::Queued,
            })
            .collect();
        Ok(())
    }
}

#[derive(Clone, Copy)]
pub(super) struct Observer<'a> {
    pub control: &'a AnnouncementControl,
    pub index: usize,
}

impl Observer<'_> {
    pub fn phase(self, phase: AnnouncementPhase) {
        let mut progress = self
            .control
            .0
            .progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(household) = progress.households.get_mut(self.index) {
            household.phase = phase;
        }
        progress.complete = progress
            .households
            .iter()
            .all(|household| household.phase == AnnouncementPhase::Complete);
    }

    pub fn target(self, report: &HouseholdAnnouncement) {
        let mut progress = self
            .control
            .0
            .progress
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        if let Some(household) = progress.households.get_mut(self.index) {
            household.lead.clone_from(&report.lead);
            household.rooms.clone_from(&report.rooms);
        }
    }
}

pub(super) fn check(control: Option<&AnnouncementControl>) -> Result<(), CoreError> {
    if control.is_some_and(AnnouncementControl::is_cancelled) {
        Err(ProtoError::NotWired(CANCELLED).into())
    } else {
        Ok(())
    }
}

pub(super) fn requested(error: &CoreError) -> bool {
    matches!(error, CoreError::Proto(ProtoError::NotWired(reason)) if *reason == CANCELLED)
}

/// Starting a clip has two writes. A request arriving during SetURI must
/// prevent the subsequent Play; this local refusal is deliberately definite,
/// so source-start reconciliation cannot mistake it for a lost reply.
pub(super) struct Starting<'a, T: Transport + ?Sized> {
    transport: &'a T,
    control: Option<&'a AnnouncementControl>,
    refused: AtomicBool,
}

impl<'a, T: Transport + ?Sized> Starting<'a, T> {
    pub fn new(transport: &'a T, control: Option<&'a AnnouncementControl>) -> Self {
        Self {
            transport,
            control,
            refused: AtomicBool::new(false),
        }
    }

    /// Source-start wraps failures after SetURI to prevent replaying that
    /// completed mutation. Retain the announcer's typed cancellation only
    /// when this wrapper actually refused a write before sending it. A late
    /// cancellation flag alone cannot turn an ambiguous network failure into
    /// a confirmed cancellation.
    pub fn finish(&self, result: Result<(), CoreError>) -> Result<(), CoreError> {
        if self.refused.load(Ordering::Acquire) {
            Err(ProtoError::NotWired(CANCELLED).into())
        } else {
            result
        }
    }
}

impl<T: Transport + ?Sized> Transport for Starting<'_, T> {
    fn soap_post(
        &self,
        host: IpAddr,
        path: &str,
        action: &str,
        body: &str,
    ) -> Result<String, ProtoError> {
        if self.control.is_some_and(AnnouncementControl::is_cancelled)
            && (action.contains("#SetAVTransportURI") || action.contains("#Play\""))
        {
            self.refused.store(true, Ordering::Release);
            return Err(ProtoError::NotWired(CANCELLED));
        }
        self.transport.soap_post(host, path, action, body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cancellation_is_shared_and_requested_once() {
        let control = AnnouncementControl::default();
        let observer = control.clone();
        assert!(!observer.is_cancelled());
        assert!(control.cancel());
        assert!(observer.is_cancelled());
        assert!(!observer.cancel());
        assert!(control.progress().is_empty());
    }

    #[test]
    fn completed_observations_do_not_accept_a_late_cancel() {
        let control = AnnouncementControl::default();
        control.begin(&[]).unwrap();
        Observer {
            control: &control,
            index: 0,
        }
        .phase(AnnouncementPhase::Complete);
        assert!(!control.cancel());
        assert!(!control.is_cancelled());
        assert!(matches!(
            control.begin(&[]),
            Err(AnnounceError::ControlReused)
        ));
    }

    struct LostReply;

    impl Transport for LostReply {
        fn soap_post(&self, _: IpAddr, _: &str, _: &str, _: &str) -> Result<String, ProtoError> {
            Err(ProtoError::Network {
                target: "synthetic speaker".into(),
                detail: "reply was lost".into(),
            })
        }
    }

    #[test]
    fn a_locally_refused_play_retains_cancellation_after_source_start_wraps_it() {
        let control = AnnouncementControl::default();
        let starting = Starting::new(&LostReply, Some(&control));
        assert!(control.cancel());
        let refused =
            fsonos_proto::control::play(&starting, "127.0.0.1".parse().unwrap()).unwrap_err();
        assert!(matches!(refused, ProtoError::NotWired(reason) if reason == CANCELLED));
        let wrapped = CoreError::Uncertain(format!("source selection completed: {refused}"));
        assert!(requested(&starting.finish(Err(wrapped)).unwrap_err()));
    }

    #[test]
    fn a_late_cancel_does_not_hide_an_unconfirmed_write_or_other_failure() {
        let control = AnnouncementControl::default();
        let starting = Starting::new(&LostReply, Some(&control));
        let lost =
            fsonos_proto::control::play(&starting, "127.0.0.1".parse().unwrap()).unwrap_err();
        assert!(matches!(lost, ProtoError::Network { .. }));
        assert!(control.cancel());
        let uncertain = CoreError::Uncertain(format!("source selection completed: {lost}"));
        let result = starting.finish(Err(uncertain)).unwrap_err();
        assert!(!requested(&result));
        assert!(matches!(result, CoreError::Uncertain(_)));
    }
}
