//! The DJ as the surfaces see it. A [`DjEngine`] runs it on the speakers
//! (the daemon's, over fsonos-spotify's queue feed; installed with
//! [`crate::Surface::with_dj`]), and [`crate::surface::follow`] feeds it each
//! coordinator's playback from the live model, which is what keeps a DJ queue
//! topped up. Without the live model a start queues the first works and
//! plays them, and nothing tops the queue up.

use fsonos_core::HouseholdState;
use fsonos_core::clock::Clock;
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::store::Store;
use fsonos_proto::Transport;
use fsonos_types::PlayerId;

use crate::execute::OutcomeDto;
use crate::failure::Failure;
use crate::plan::DjAction;

/// The speakers a DJ command acts on.
#[derive(Clone, Copy)]
pub struct DjSpeakers<'a> {
    pub transport: &'a dyn Transport,
    pub households: &'a [HouseholdState],
    /// The coordinator of the group the DJ feeds.
    pub coordinator: &'a PlayerId,
}

/// Runs the DJ; see the module docs.
pub trait DjEngine: Send + Sync {
    /// Start, skip or stop the DJ in the group `at.coordinator` leads.
    /// `clock` is the house's: its local day and time pick the time-of-day
    /// program and the energy target.
    fn act(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        action: DjAction,
        clock: &dyn Clock,
    ) -> Result<OutcomeDto, Failure>;

    /// Whether the DJ is feeding `coordinator`'s queue.
    fn feeds(&self, coordinator: &PlayerId) -> bool;

    /// Fold `playback`, the coordinator's latest state, into its feed: record
    /// what plays and top the queue up. Failures are the engine's to log; the
    /// next playback change retries.
    fn on_playback(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        playback: &PlayerPlayback,
        clock: &dyn Clock,
    );
}
