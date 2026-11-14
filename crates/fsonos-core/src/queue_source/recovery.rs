//! A rejected initial enqueue permits one new template, never a new owner.
//! Keep its original destination, queue revision and playback intent while
//! learning refreshed parameters; post-commit failures never enter recovery.

use super::{
    Behavior, Destination, Owner, Queue, QueueReplacement, ReceiptOwner, read, replace_once,
    uncertain,
};
use crate::{CoreError, HouseholdState};
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::{PlayerId, TransportState};
use std::net::IpAddr;

#[derive(Default)]
pub(super) struct State {
    pub(super) boundary: Option<Recovery>,
    pub(super) refused: Option<Recovery>,
}

pub(super) struct Recovery {
    pub(super) destination: Destination,
    pub(super) queue: Queue,
    pub(super) owner: Owner,
    original_owner: Owner,
}

impl Recovery {
    pub(super) fn confirm<T: Transport + ?Sized>(
        &self,
        t: &T,
        host: IpAddr,
        coordinator: &PlayerId,
    ) -> Result<(), CoreError> {
        if !self.destination.has_boot_identity()
            || !self.destination.is_at(host)
            || self.owner.mode.is_none()
            || self.owner.playback.position.position_secs.is_none()
            || !matches!(
                self.owner.playback.state,
                TransportState::Playing | TransportState::Paused | TransportState::Stopped
            )
        {
            return Err(uncertain(
                "renderer recovery lacks complete original playback and player evidence",
            ));
        }
        self.destination
            .confirm(t, host, coordinator)
            .map_err(uncertain)?;
        self.owner.confirm(t, host, coordinator).map_err(uncertain)?;
        if read(t, host).map_err(uncertain)? != self.queue {
            return Err(uncertain("the queue changed while relearning its renderer"));
        }
        self.destination
            .confirm(t, host, coordinator)
            .map_err(uncertain)
    }

    fn finish(self, mut receipt: QueueReplacement) -> QueueReplacement {
        receipt.before = self.queue;
        receipt.owner = Some(self.original_owner);
        receipt
    }
}

pub(super) fn after_enqueue_refusal<T: Transport + ?Sized>(
    t: &T,
    host: IpAddr,
    coordinator: &PlayerId,
    destination: Destination,
    queue: Queue,
    owner: Option<ReceiptOwner>,
) -> Result<Recovery, CoreError> {
    let owner = owner.ok_or_else(|| {
        uncertain("the refused enqueue has no stable original playback observation")
    })?;
    let playback = owner.execution.unwrap_or_else(|| owner.original.clone());
    let proof = Recovery {
        destination,
        queue,
        owner: playback,
        original_owner: owner.original,
    };
    proof.confirm(t, host, coordinator)?;
    Ok(proof)
}

pub(crate) fn replace<T: Transport + ?Sized>(
    t: &T,
    households: &[HouseholdState],
    coordinator: &PlayerId,
    autoplay: bool,
    receipt: bool,
    learn: impl Fn(Option<&(String, String)>) -> Result<(String, String), CoreError>,
) -> Result<QueueReplacement, CoreError> {
    let initial = learn(None)?;
    let behavior = Behavior { autoplay, receipt };
    let mut state = State::default();
    let error = match replace_once(
        t,
        households,
        coordinator,
        &initial.0,
        &initial.1,
        behavior,
        Some(&mut state),
    ) {
        Ok(completed) => return Ok(completed),
        Err(error) => error,
    };
    if !matches!(
        error,
        CoreError::Proto(ProtoError::SoapFault { code: 800, .. })
    ) {
        return Err(error);
    }
    let Some(boundary) = state.refused.take() else {
        return Err(error);
    };
    let refreshed = learn(Some(&initial)).map_err(uncertain)?;
    // replace_once confirms this retained proof again across its preparation
    // reads. It cannot read a listener's newer state and adopt it as its own.
    state.boundary = Some(boundary);
    match replace_once(
        t,
        households,
        coordinator,
        &refreshed.0,
        &refreshed.1,
        behavior,
        Some(&mut state),
    ) {
        Ok(completed) => Ok(state.boundary.expect("retained refusal").finish(completed)),
        Err(CoreError::Proto(ProtoError::SoapFault { code: 800, .. })) => {
            Err(CoreError::RenderParamsStale)
        }
        Err(error) => Err(uncertain(error)),
    }
}
