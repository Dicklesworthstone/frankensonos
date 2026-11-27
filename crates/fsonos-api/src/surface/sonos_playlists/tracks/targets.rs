//! A saved-playlist revision stays with its original reader while independent
//! destination groups retain their own queue and physical-audience proofs.

use fsonos_core::control;
use fsonos_core::playlists;
use fsonos_core::policy::Client;

use super::{
    Command, Failure, NativePlaylistSelection, SonosPlaylistTrackMode, SonosPlaylistTracksDto,
    SonosPlaylistTracksRequest, Surface, confirm_destination_revision,
};
use crate::plan::resolve_set;
use crate::surface::source_control::confirm_revision;
use crate::surface::{room_view, set_runs};

impl Surface {
    pub(super) fn sonos_playlist_tracks_targets(
        &self,
        client: &Client,
        req: &SonosPlaylistTracksRequest,
    ) -> Result<SonosPlaylistTracksDto, Failure> {
        if req.mode != SonosPlaylistTrackMode::Play {
            let (batch, titles) = self.queue_sonos_playlist_targets(client, req)?;
            let single = match (batch.added.as_slice(), batch.failed.is_empty()) {
                ([added], true) => Some(&added.queue),
                _ => None,
            };
            return Ok(SonosPlaylistTracksDto {
                done: batch.done.clone(),
                zone: single.map_or_else(|| req.target().into(), |queue| queue.zone.clone()),
                playlist: req.playlist.trim().into(),
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
        if let Some(revision) = &req.expected_target_revision {
            confirm_revision(&rooms, req.target(), revision)?;
        }
        let mut targets: Vec<fsonos_core::ControlTarget<'_>> = Vec::new();
        for target in resolve_set(&rooms, req.target())? {
            if !targets
                .iter()
                .any(|known| known.coordinator.id == target.coordinator.id)
            {
                targets.push(target);
            }
        }
        if targets.len() > 1 && req.expected_queue_revision.is_some() {
            return Err(Failure::invalid(
                "expected_queue_revision requires one destination group; use expected_target_revision to bind a room set",
            ));
        }
        let coordinators = targets
            .iter()
            .map(|target| target.coordinator.id.clone())
            .collect::<Vec<_>>();
        let source = NativePlaylistSelection::source_for(&rooms, req.zone.trim(), &coordinators)?;
        // Retain every destination before the source catalog is read. A slow
        // lookup cannot adopt playback that appeared halfway through planning.
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
                req.expected_queue_revision.as_deref(),
            )?;
            destinations.push(destination);
        }
        let selection = NativePlaylistSelection {
            tracks: playlists::select_tracks(
                &*self.transport,
                &households,
                &source,
                req.playlist.trim(),
                &req.positions,
                &req.expected_revision,
            )?,
            source_coordinator: source,
            playlist: req.playlist.trim().into(),
            positions: req.positions.clone(),
            expected_revision: req.expected_revision.clone(),
        };
        let titles = selection
            .tracks
            .iter()
            .map(|track| track.title.clone())
            .collect();
        let zone = match targets.as_slice() {
            [target] => target.coordinator.room_name.clone(),
            _ => req.target().into(),
        };
        let commands = targets
            .into_iter()
            .zip(destinations)
            .map(|(target, destination)| Command::PlayNativeSelection {
                coordinator: target.coordinator.id.clone(),
                selection: selection.clone().into(),
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
}
