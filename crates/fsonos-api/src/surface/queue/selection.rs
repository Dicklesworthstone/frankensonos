//! Send exact live-queue occurrences to independent groups in their household.
//! Source and destination proofs stay distinct, including when both are one group.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use fsonos_core::{HouseholdState, control, playlists};
use fsonos_proto::Transport;
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};

use super::{QueueAddOutcome, QueueAddedSetDto, Surface, confirm_destination_revision};
use crate::guard::Note;
use crate::plan::{Command, NativePlaylistSelection, NativeTrackSelection, resolve_set};
use crate::surface::sonos_playlists::{SonosPlaylistTrackMode, SonosPlaylistTracksRequest};
use crate::surface::source_control::{confirm_revision, validate_revision};
use crate::surface::{room_view, set_runs};
use crate::{Failure, NoteCode};

/// Select source occurrences from get_queue or search_queue, never raw metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueSelectionRequest {
    /// Original room/group whose queue supplied the positions and revision.
    pub zone: String,
    /// Destination room, alias or all@household; defaults to the source group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Optional physical audience revision from resolve_rooms for the target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_target_revision: Option<String>,
    /// One to ten one-based occurrences, keeping the requested order and repeats.
    pub positions: Vec<u32>,
    /// Source queue revision, independent of the destination queue revision.
    pub expected_revision: String,
    /// Play replaces and starts; next/append keep current playback unchanged.
    pub mode: SonosPlaylistTrackMode,
    /// Optional destination queue revision, only for one destination group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_queue_revision: Option<String>,
}

impl QueueSelectionRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        crate::request::zone_name("zone", &self.zone)?;
        if let Some(target) = &self.target {
            crate::request::zone_name("target", target)?;
        }
        if let Some(revision) = &self.expected_target_revision {
            validate_revision(revision)?;
        }
        if self.positions.is_empty()
            || self.positions.len() > control::MAX_TRACK_BATCH
            || self.positions.iter().any(|position| {
                *position == 0 || *position > fsonos_proto::content::MAX_BROWSE_OBJECTS
            })
        {
            return Err(Failure::invalid(
                "select between 1 and 10 bounded one-based queue occurrences",
            ));
        }
        for revision in
            std::iter::once(&self.expected_revision).chain(self.expected_queue_revision.iter())
        {
            if revision.trim().is_empty()
                || revision.len() > 1024
                || revision.chars().any(char::is_control)
            {
                return Err(Failure::invalid(
                    "queue revisions must be nonempty and at most 1024 bytes without control characters",
                ));
            }
        }
        Ok(())
    }

    fn input(&self) -> NativeSelectionInput<'_> {
        NativeSelectionInput {
            zone: self.zone.trim(),
            target: self.target.as_deref().unwrap_or(&self.zone).trim(),
            expected_target_revision: self.expected_target_revision.as_deref(),
            expected_queue_revision: self.expected_queue_revision.as_deref(),
            playlist: None,
            positions: &self.positions,
            expected_revision: &self.expected_revision,
            mode: self.mode,
        }
    }
}

/// Shared input to the existing native insertion planner. A queue source uses
/// retained replay metadata; a saved playlist keeps its native catalog proof.
pub(in crate::surface) struct NativeSelectionInput<'a> {
    pub zone: &'a str,
    pub target: &'a str,
    pub expected_target_revision: Option<&'a str>,
    pub expected_queue_revision: Option<&'a str>,
    pub playlist: Option<&'a str>,
    pub positions: &'a [u32],
    pub expected_revision: &'a str,
    pub mode: SonosPlaylistTrackMode,
}

impl<'a> From<&'a SonosPlaylistTracksRequest> for NativeSelectionInput<'a> {
    fn from(request: &'a SonosPlaylistTracksRequest) -> Self {
        Self {
            zone: request.zone.trim(),
            target: request.target(),
            expected_target_revision: request.expected_target_revision.as_deref(),
            expected_queue_revision: request.expected_queue_revision.as_deref(),
            playlist: Some(request.playlist.trim()),
            positions: &request.positions,
            expected_revision: &request.expected_revision,
            mode: request.mode,
        }
    }
}

impl NativeSelectionInput<'_> {
    pub(super) fn select<T: Transport + ?Sized>(
        &self,
        transport: &T,
        households: &[HouseholdState],
        source: PlayerId,
    ) -> Result<NativeTrackSelection, Failure> {
        match self.playlist {
            Some(playlist) => Ok(NativePlaylistSelection {
                tracks: playlists::select_tracks(
                    transport,
                    households,
                    &source,
                    playlist,
                    self.positions,
                    self.expected_revision,
                )?,
                source_coordinator: source,
                playlist: playlist.into(),
                positions: self.positions.to_vec(),
                expected_revision: self.expected_revision.into(),
            }
            .into()),
            None => Ok(NativeTrackSelection::Queue(Box::new(
                playlists::PreparedQueueSelection::read(
                    transport,
                    households,
                    &source,
                    self.positions,
                    self.expected_revision,
                )?,
            ))),
        }
    }
}

/// Confirmed delivery and per-group receipts; renderer credentials stay local.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueueSelectionDto {
    pub done: String,
    pub source_zone: String,
    pub zone: String,
    pub positions: Vec<u32>,
    pub titles: Vec<String>,
    pub mode: SonosPlaylistTrackMode,
    pub changed: bool,
    #[serde(default)]
    pub notes: Vec<Note>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub first: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub count: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playing_queue: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<crate::execute::ControlBatchDto>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_batch: Option<QueueAddedSetDto>,
}

impl QueueSelectionDto {
    #[must_use]
    pub fn text(&self) -> String {
        if let Some(batch) = &self.queue_batch {
            return QueueAddOutcome::Groups(batch.clone()).text();
        }
        let mut text = format!("{}\n", self.done);
        for note in &self.notes {
            text.push_str(&note.detail);
            text.push('\n');
        }
        text
    }

    #[must_use]
    pub fn complete(&self) -> bool {
        !self.notes.iter().any(|note| note.code == NoteCode::Skipped)
            && self.batch.as_ref().is_none_or(|batch| {
                batch.complete
                    && !batch.results.is_empty()
                    && batch.results.iter().all(|target| target.result.complete())
            })
            && self.queue_batch.as_ref().is_none_or(|batch| {
                batch.complete && batch.failed.is_empty() && !batch.added.is_empty()
            })
    }
}

impl Surface {
    /// Copy exact live-queue occurrences through the shared native executors.
    /// Source playback stays intact unless its group was explicitly targeted.
    pub fn queue_selection(
        &self,
        client: &Client,
        req: &QueueSelectionRequest,
    ) -> Result<QueueSelectionDto, Failure> {
        req.validate()?;
        let input = req.input();
        if req.mode != SonosPlaylistTrackMode::Play {
            let (batch, titles) = self.queue_native_selection_targets(client, &input)?;
            let single = match (batch.added.as_slice(), batch.failed.is_empty()) {
                ([added], true) => Some(&added.queue),
                _ => None,
            };
            return Ok(QueueSelectionDto {
                done: batch.done.clone(),
                source_zone: input.zone.into(),
                zone: single.map_or_else(|| input.target.into(), |queue| queue.zone.clone()),
                positions: req.positions.clone(),
                titles,
                mode: req.mode,
                changed: !batch.added.is_empty()
                    || batch
                        .failed
                        .iter()
                        .any(|failed| failed.error.code == crate::ErrorCode::OutcomeUncertain),
                notes: Vec::new(),
                first: single.map(|queue| queue.first),
                count: single.map(|queue| queue.count),
                playing_queue: single.map(|queue| queue.playing_queue),
                batch: None,
                queue_batch: Some(batch),
            });
        }

        self.authorize_logged(client, "play_favorite")?;
        let households = self.households()?;
        let aliases = self.aliases();
        let rooms = room_view(&households, aliases.as_ref(), client);
        if let Some(revision) = input.expected_target_revision {
            confirm_revision(&rooms, input.target, revision)?;
        }
        let mut targets: Vec<fsonos_core::ControlTarget<'_>> = Vec::new();
        for target in resolve_set(&rooms, input.target)? {
            if !targets
                .iter()
                .any(|known| known.coordinator.id == target.coordinator.id)
            {
                targets.push(target);
            }
        }
        if targets.len() > 1 && input.expected_queue_revision.is_some() {
            return Err(Failure::invalid(
                "expected_queue_revision requires one destination group; use expected_target_revision to bind a room set",
            ));
        }
        let coordinators = targets
            .iter()
            .map(|target| target.coordinator.id.clone())
            .collect::<Vec<_>>();
        let source = NativePlaylistSelection::source_for(&rooms, input.zone, &coordinators)?;
        // Read every destination before source/renderer I/O, and put a selected
        // source group last so its mutation cannot invalidate later copies.
        targets.sort_by_key(|target| target.coordinator.id == source);
        let mut destinations = Vec::with_capacity(targets.len());
        for target in &targets {
            let coordinator = &target.coordinator.id;
            self.confirm_reviewed_group(&households, coordinator)?;
            let destination = control::TrackBatchDestination::read(
                &*self.transport,
                target.coordinator.ip,
                coordinator,
            )?;
            confirm_destination_revision(
                &*self.transport,
                target.coordinator.ip,
                coordinator,
                input.expected_queue_revision,
            )?;
            destinations.push(destination);
        }
        let selection = input.select(&*self.transport, &households, source)?;
        let titles = selection
            .tracks()
            .iter()
            .map(|track| track.title.clone())
            .collect();
        let zone = match targets.as_slice() {
            [target] => target.coordinator.room_name.clone(),
            _ => input.target.into(),
        };
        let commands = targets
            .into_iter()
            .zip(destinations)
            .map(|(target, destination)| Command::PlayNativeSelection {
                coordinator: target.coordinator.id.clone(),
                selection: selection.clone(),
                destination,
            })
            .collect();
        let commands = self.prepare_sources(&households, commands)?;
        for coordinator in &coordinators {
            self.confirm_reviewed_group(&households, coordinator)?;
        }
        let outcome = self.control_prepared_set(
            client,
            "play_favorite",
            &households,
            commands,
            set_runs::Execution::ReviewedSource,
        )?;
        Ok(QueueSelectionDto {
            done: outcome.done,
            source_zone: input.zone.into(),
            zone,
            positions: req.positions.clone(),
            titles,
            mode: req.mode,
            changed: outcome.changed,
            notes: outcome.notes,
            first: None,
            count: None,
            playing_queue: None,
            batch: outcome.batch,
            queue_batch: None,
        })
    }
}

#[cfg(test)]
mod tests;
