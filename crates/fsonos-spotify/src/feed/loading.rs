//! Prepare a whole-work set silently, and retain that intent across restarts.

use fsonos_core::{CoreError, playback::PlayerPlayback};
use fsonos_proto::control::{get_transport_info, set_normal_play_mode};
use fsonos_proto::didl::spotify_uri_from_renderer_uri;
use fsonos_proto::{ProtoError, Transport};
use fsonos_types::TransportState;

use super::stop::{self, Selected, Selection};
use super::{FeedError, Planning, QueueFeed, QueuedWork, Speakers, Store, host_of};

impl QueueFeed {
    /// Prepare without Play. Existing owned occurrences keep their elapsed
    /// position; a new set selects its first movement in NORMAL order. The old
    /// source is paused before any music is appended. A later failure does not
    /// permit automatic playback to undo the requested silence.
    ///
    /// A daemon should persist [`Self::suspend`] before calling this on a feed
    /// that is already saved. Preparing a fresh feed has no earlier automatic
    /// playback intent to retire.
    pub fn load<T: Transport + ?Sized, S: Store + ?Sized>(
        &mut self,
        at: &Speakers<'_, T>,
        plan: Planning<'_>,
        store: &mut S,
    ) -> Result<&[QueuedWork], FeedError> {
        self.load_ready(at, plan, store)?;
        self.suspend();
        if let Err(error) = self.begin(at, plan, store, false) {
            if self.queued.is_empty() && self.played.is_empty() {
                self.release();
            }
            return Err(error);
        }
        Ok(&self.queued)
    }

    /// Retire automatic playback intent without touching the speaker. Save
    /// this state before silent scene preparation so an interruption cannot
    /// restore an exhausted-queue retry that would start the music.
    pub fn suspend(&mut self) {
        self.suspended = true;
        self.dry_end = None;
        self.accepted_playback = false;
    }

    /// A silent preparation is waiting for the listener to start playback.
    #[must_use]
    pub fn is_suspended(&self) -> bool {
        self.suspended
    }
}

fn inactive<T: Transport + ?Sized>(at: &Speakers<'_, T>) -> Result<bool, FeedError> {
    Ok(matches!(
        get_transport_info(at.transport, host_of(at)?)?.state,
        TransportState::Paused | TransportState::Stopped
    ))
}

/// Pausing before queue work retained this occurrence and its elapsed time.
/// A mode change must not reselect or rewind it, even after a concurrent edit.
pub(super) fn hold<T: Transport + ?Sized>(
    feed: &QueueFeed,
    at: &Speakers<'_, T>,
    expected: &Selected,
) -> Result<(), FeedError> {
    let confirm = || -> Result<(), FeedError> {
        if !matches!(
            stop::selected(feed, at)?,
            Selection::Owned(selected) if &selected == expected
        ) || !inactive(at)?
        {
            return Err(ProtoError::Malformed(
                "the DJ occurrence changed or resumed during silent preparation".into(),
            )
            .into());
        }
        Ok(())
    };
    confirm()?;
    if let Err(error) = set_normal_play_mode(at.transport, host_of(at)?) {
        if matches!(
            &error,
            ProtoError::Network { .. } | ProtoError::Malformed(_)
        ) {
            return Err(uncertain("SetPlayMode", error));
        }
        return Err(error.into());
    }
    confirm().map_err(|error| uncertain("confirmation after SetPlayMode", error))
}

fn uncertain(stage: &str, error: impl std::fmt::Display) -> FeedError {
    CoreError::Uncertain(format!(
        "silent DJ preparation stopped during {stage}; inspect the speaker before retrying: {error}"
    ))
    .into()
}

/// Buffered Playing or renderer-error events must not lift a scene's silence.
/// Only a fresh read of the event's exact owned, playing occurrence may resume
/// maintenance. Resync stays provisional until every observation agrees.
pub(super) fn observed_resume<T: Transport + ?Sized>(
    feed: &mut QueueFeed,
    at: &Speakers<'_, T>,
    event: &PlayerPlayback,
) -> Result<bool, FeedError> {
    if event.transport != Some(TransportState::Playing)
        || event.transport_status.as_deref() == Some("ERROR_OCCURRED")
    {
        return Ok(false);
    }
    let Some(position) = event.queue_position.filter(|&position| position > 0) else {
        return Ok(false);
    };
    let Some(source) = event
        .track_uri
        .as_deref()
        .and_then(spotify_uri_from_renderer_uri)
    else {
        return Ok(false);
    };
    let playing = || -> Result<bool, FeedError> {
        let live = get_transport_info(at.transport, host_of(at)?)?;
        Ok(live.state == TransportState::Playing && live.status != "ERROR_OCCURRED")
    };
    if !playing()? {
        return Ok(false);
    }
    let mut resumed = feed.clone();
    resumed.resync(at)?;
    if !resumed.is_active() {
        return Ok(false);
    }
    let selected = match stop::selected(&resumed, at)? {
        Selection::Owned(selected) => selected,
        Selection::DifferentSource | Selection::Unowned | Selection::Changed => return Ok(false),
    };
    if selected.position != position
        || !resumed
            .played
            .iter()
            .chain(&resumed.queued)
            .any(|work| work.track_at(position) == Some(source.as_str()))
        || !playing()?
    {
        return Ok(false);
    }
    *feed = resumed;
    Ok(true)
}
