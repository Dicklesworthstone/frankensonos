//! One reviewed native occurrence selection, one command and one undo scope.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::actions::{self, QueuedBefore};
use fsonos_core::policy::Client;
use fsonos_core::{CoreError, control, playlists};
use fsonos_proto::ProtoError;
use serde::{Deserialize, Serialize};

use super::{SonosPlaylistTrackMode, SonosPlaylistTrackRequest, Surface};
use crate::guard::Note;
use crate::heal::Observed;
use crate::plan::{Command, NativePlaylistSelection};
use crate::surface::queue::{ADD, QueueAddOutcome, QueueAddedSetDto, confirm_destination_revision};
use crate::{ErrorCode, Failure, NoteCode};

mod targets;

/// Select exact saved occurrences without accepting client renderer metadata.
/// `play` replaces the queue; `next` and `append` leave playback alone.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SonosPlaylistTracksRequest {
    /// The original room/group used to read the playlist and its revision.
    pub zone: String,
    /// Optional destination room, alias or all/everywhere in the source
    /// household. Omitted means the source zone's own group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
    /// Optional audience revision from /rooms/resolve for target (or zone).
    /// This is independent of both playlist and destination queue revisions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_target_revision: Option<String>,
    /// Exact household-local SQ: id from the observed saved playlist.
    pub playlist: String,
    /// Between 1 and 10 one-based occurrences, in the requested playback order.
    /// Repeated positions and repeated resources remain separate occurrences.
    pub positions: Vec<u32>,
    /// The saved playlist contents revision, not the destination queue's token.
    pub expected_revision: String,
    pub mode: SonosPlaylistTrackMode,
    /// Optional token from `get_queue`, valid only for one destination group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_queue_revision: Option<String>,
}

impl SonosPlaylistTracksRequest {
    pub fn validate(&self) -> Result<(), Failure> {
        if let Some(target) = &self.target {
            crate::request::zone_name("target", target)?;
        }
        if let Some(revision) = &self.expected_target_revision {
            crate::surface::source_control::validate_revision(revision)?;
        }
        if self.positions.is_empty() || self.positions.len() > control::MAX_TRACK_BATCH {
            return Err(Failure::invalid(
                "select between 1 and 10 saved playlist occurrences",
            ));
        }
        for &position in &self.positions {
            SonosPlaylistTrackRequest {
                zone: self.zone.clone(),
                playlist: self.playlist.clone(),
                position,
                expected_revision: self.expected_revision.clone(),
                mode: self.mode,
            }
            .validate()?;
        }
        if self
            .expected_queue_revision
            .as_ref()
            .is_some_and(|revision| {
                revision.trim().is_empty()
                    || revision.len() > 1024
                    || revision.chars().any(char::is_control)
            })
        {
            return Err(Failure::invalid(
                "expected_queue_revision must be nonempty, free of control characters and at most 1024 bytes",
            ));
        }
        Ok(())
    }

    pub(in crate::surface) fn target(&self) -> &str {
        self.target.as_deref().unwrap_or(&self.zone).trim()
    }
}

/// The native selection and confirmed effect; no renderer credentials are returned.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SonosPlaylistTracksDto {
    pub done: String,
    pub zone: String,
    pub playlist: String,
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
    /// Actual playback results when more than one group was addressed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub batch: Option<crate::execute::ControlBatchDto>,
    /// Per-group insertion ranges and failures for an explicit queue target.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub queue_batch: Option<QueueAddedSetDto>,
}

impl SonosPlaylistTracksDto {
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

    /// CLI/MCP keep every receipt while reporting incomplete delivery.
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
    /// Native selections share policy, staged replacement and exact-range undo
    /// with existing playback. No Spotify catalog or renderer learning is needed.
    pub fn sonos_playlist_tracks(
        &self,
        client: &Client,
        req: &SonosPlaylistTracksRequest,
    ) -> Result<SonosPlaylistTracksDto, Failure> {
        req.validate()?;
        if req.target.is_some() || req.expected_target_revision.is_some() {
            return self.sonos_playlist_tracks_targets(client, req);
        }
        if req.mode != SonosPlaylistTrackMode::Play {
            return self.queue_sonos_playlist_tracks(client, req);
        }
        let mut zone = String::new();
        let mut titles = Vec::new();
        let outcome = self.control_io(client, "play_favorite", |transport, rooms| {
            let target = super::resolve(rooms, req.zone.trim())?;
            let coordinator = &target.coordinator.id;
            let host = target.coordinator.ip;
            self.confirm_source_audience(rooms.households, coordinator)?;
            // Retain the destination before potentially slow native catalog reads.
            let destination = control::TrackBatchDestination::read(transport, host, coordinator)?;
            confirm_destination_revision(
                transport,
                host,
                coordinator,
                req.expected_queue_revision.as_deref(),
            )?;
            let tracks = playlists::select_tracks(
                transport,
                rooms.households,
                coordinator,
                req.playlist.trim(),
                &req.positions,
                &req.expected_revision,
            )?;
            titles = tracks.iter().map(|track| track.title.clone()).collect();
            zone.clone_from(&target.coordinator.room_name);
            Ok(Command::PlayNativeSelection {
                coordinator: coordinator.clone(),
                selection: NativePlaylistSelection {
                    source_coordinator: coordinator.clone(),
                    playlist: req.playlist.trim().into(),
                    positions: req.positions.clone(),
                    expected_revision: req.expected_revision.clone(),
                    tracks,
                },
                destination,
            })
        })?;
        Ok(SonosPlaylistTracksDto {
            done: outcome.done,
            zone,
            playlist: req.playlist.trim().into(),
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

    fn queue_sonos_playlist_tracks(
        &self,
        client: &Client,
        req: &SonosPlaylistTracksRequest,
    ) -> Result<SonosPlaylistTracksDto, Failure> {
        self.authorize_logged(client, ADD)?;
        let next = req.mode == SonosPlaylistTrackMode::Next;
        let intent = format!(
            "{ADD}: {} saved playlist {} occurrences {:?} next={next}",
            req.zone.trim(),
            req.playlist.trim(),
            req.positions,
        );
        let mut uncertain = false;
        let result = (|| {
            let (households, group) = self.queue_households(client, req.zone.trim())?;
            let coordinator = group.coordinator.clone();
            let player = control::locate(&households, &coordinator)?;
            let destination =
                control::TrackBatchDestination::read(&*self.transport, player.ip, &coordinator)?;
            confirm_destination_revision(
                &*self.transport,
                player.ip,
                &coordinator,
                req.expected_queue_revision.as_deref(),
            )?;
            let tracks = playlists::select_tracks(
                &*self.transport,
                &households,
                &coordinator,
                req.playlist.trim(),
                &req.positions,
                &req.expected_revision,
            )?;
            let selection = NativePlaylistSelection {
                source_coordinator: coordinator.clone(),
                playlist: req.playlist.trim().into(),
                positions: req.positions.clone(),
                expected_revision: req.expected_revision.clone(),
                tracks,
            };
            self.guard(client).authorize(ADD, false)?;
            self.queue_group_unchanged(player.ip, &group)?;
            selection.confirm(&*self.transport, &households, &coordinator)?;
            let watch = Observed::new(&*self.transport);
            let queued = control::enqueue_native_tracks_bound(
                &watch, &households, &coordinator, &selection.tracks, next, &destination,
            ).map_err(|error| {
                uncertain = watch.wrote() && matches!(
                    &error,
                    CoreError::Proto(ProtoError::Network { .. } | ProtoError::Malformed(_))
                        | CoreError::Uncertain(_)
                );
                Failure::from(error).with_hint(
                    "Read the destination queue before another add; the saved playlist selection was not replayed.",
                )
            })?;
            let count = u32::try_from(queued.uris.len()).expect("bounded native selection");
            let mut done = format!(
                "queued {count} selected saved playlist tracks in {}'s group at position {} in the supplied order",
                player.room_name, queued.first,
            );
            if !queued.playing_queue {
                done.push_str(" (the group is not playing its queue; playback was not changed)");
            }
            let before = actions::before_state_queued(&[QueuedBefore {
                coordinator,
                first: queued.first,
                uris: queued.uris,
                proof: queued.proof,
            }]);
            Ok((
                SonosPlaylistTracksDto {
                    done,
                    zone: player.room_name.clone(),
                    playlist: selection.playlist,
                    positions: selection.positions,
                    titles: selection
                        .tracks
                        .iter()
                        .map(|track| track.title.clone())
                        .collect(),
                    mode: req.mode,
                    changed: true,
                    notes: Vec::new(),
                    first: Some(queued.first),
                    count: Some(count),
                    playing_queue: Some(queued.playing_queue),
                    batch: None,
                    queue_batch: None,
                },
                before,
            ))
        })();
        match result {
            Ok((mut added, before)) => {
                if !self.record(client, intent, "allow".into(), added.done.clone(), before) {
                    added.notes.push(Note {
                        code: NoteCode::NotLogged,
                        detail: "carried out, but the action could not be logged, so it cannot be undone".into(),
                    });
                }
                Ok(added)
            }
            Err(failure) => {
                self.notice(&failure);
                let decision = if failure.code == ErrorCode::PolicyDenied {
                    format!("deny: {}", failure.detail)
                } else {
                    "allow".into()
                };
                self.record(
                    client,
                    intent,
                    decision,
                    format!("failed: {}", failure.detail),
                    uncertain.then(|| actions::before_state_not_undoable("native queue selection")),
                );
                Err(failure)
            }
        }
    }
}

#[cfg(test)]
mod tests;
