//! Queue or play an explicit ordered selection through the existing bound
//! insertion and replacement engines, with one action and undo boundary.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::actions::{self, QueuedBefore};
use fsonos_core::policy::Client;
use fsonos_core::{CoreError, control};
use fsonos_proto::{ProtoError, Transport};
use serde::{Deserialize, Serialize};

use super::{ADD, QueueAddedDto, Surface, confirm_destination_revision};
use crate::OutcomeDto;
use crate::execute::spotify_failure;
use crate::failure::{ErrorCode, Failure};
use crate::heal::Observed;
use crate::plan::{Command, Rooms};
use crate::request::{PlayRequest, zone_name};

/// The existing core batch bound, shared with all queue-selection surfaces.
pub const MAX_QUEUE_TRACKS: usize = control::MAX_SPOTIFY_TRACK_BATCH;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueTrackInput {
    /// A Spotify track or episode URI/link. Containers are never expanded.
    pub source_uri: String,
    /// Cosmetic queue metadata, normalized like an ordinary play request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

/// `POST /queue/tracks`: preserve the supplied order and repeated occurrences.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueTracksRequest {
    pub zone: String,
    pub tracks: Vec<QueueTrackInput>,
    /// Add after the current queue occurrence; otherwise add to the end.
    #[serde(default)]
    pub next: bool,
    /// Optional destination queue revision from `get_queue`, valid only when
    /// zone resolves to one group. A changed queue or coordinator rejects it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<String>,
}

/// `POST /play/tracks`: replace each selected group's queue with exactly these
/// ordered occurrences and start at the first in NORMAL mode.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct PlayTracksRequest {
    pub zone: String,
    pub tracks: Vec<QueueTrackInput>,
    /// Optional destination queue revision from `get_queue`, valid only when
    /// zone resolves to one group. Every original queue is retained through
    /// preparation and staged replacement.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_revision: Option<String>,
}

fn normalized_selection(
    zone: &str,
    tracks: &[QueueTrackInput],
    expected_revision: Option<&str>,
) -> Result<(String, Vec<QueueTrackInput>), Failure> {
    let zone = zone_name("zone", zone)?.to_string();
    if tracks.is_empty() || tracks.len() > MAX_QUEUE_TRACKS {
        return Err(Failure::invalid(format!(
            "select between 1 and {MAX_QUEUE_TRACKS} Spotify tracks or episodes"
        )));
    }
    if expected_revision.is_some_and(|revision| revision.trim().is_empty() || revision.len() > 1024)
    {
        return Err(Failure::invalid(
            "expected_revision must be nonempty and at most 1024 bytes",
        ));
    }
    let tracks = tracks
        .iter()
        .map(|track| {
            let normalized = PlayRequest {
                zone: zone.clone(),
                source_uri: track.source_uri.clone(),
                title: track.title.clone(),
            }
            .normalized()?;
            if !(normalized.source_uri.starts_with("spotify:track:")
                || normalized.source_uri.starts_with("spotify:episode:"))
            {
                return Err(Failure::invalid(
                    "each selected source must be a Spotify track or episode; albums, playlists, artists and shows are not expanded",
                ));
            }
            Ok(QueueTrackInput {
                source_uri: normalized.source_uri,
                title: normalized.title,
            })
        })
        .collect::<Result<Vec<_>, Failure>>()?;
    Ok((zone, tracks))
}

impl QueueTracksRequest {
    /// Validate before discovery, catalog reads or any speaker mutation.
    pub fn validate(&self) -> Result<(), Failure> {
        self.normalized().map(|_| ())
    }

    pub(super) fn normalized(&self) -> Result<Self, Failure> {
        let (zone, tracks) =
            normalized_selection(&self.zone, &self.tracks, self.expected_revision.as_deref())?;
        Ok(Self {
            zone,
            tracks,
            next: self.next,
            expected_revision: self.expected_revision.clone(),
        })
    }
}

impl PlayTracksRequest {
    /// Validate before discovery, renderer learning, action snapshots or caps.
    pub fn validate(&self) -> Result<(), Failure> {
        self.normalized().map(|_| ())
    }

    fn normalized(&self) -> Result<Self, Failure> {
        let (zone, tracks) =
            normalized_selection(&self.zone, &self.tracks, self.expected_revision.as_deref())?;
        Ok(Self {
            zone,
            tracks,
            expected_revision: self.expected_revision.clone(),
        })
    }
}

impl Surface {
    /// Start exactly the requested occurrences in each distinct selected group
    /// through ordinary playback permission, group caps and one bounded undo
    /// action. No Spotify Web API lookup is needed for concrete identities.
    pub fn play_tracks(
        &self,
        client: &Client,
        req: &PlayTracksRequest,
    ) -> Result<OutcomeDto, Failure> {
        let asked = req.normalized()?;
        self.control_set_io(client, "play", |transport, rooms| {
            self.plan_play_tracks(transport, rooms, &asked)
        })
    }

    /// Ordinary and reviewed playback retain the same original queues before
    /// renderer discovery, using the already admitted room snapshot.
    pub(in crate::surface) fn plan_play_tracks(
        &self,
        transport: &dyn Transport,
        rooms: &Rooms<'_>,
        req: &PlayTracksRequest,
    ) -> Result<Vec<Command>, Failure> {
        let asked = req.normalized()?;
        let mut targets: Vec<fsonos_core::ControlTarget<'_>> = Vec::new();
        for target in crate::plan::resolve_set(rooms, &asked.zone)? {
            if !targets
                .iter()
                .any(|known| known.coordinator.id == target.coordinator.id)
            {
                targets.push(target);
            }
        }
        // Queue revisions are coordinator-bound. Never apply one group's
        // token to another, or silently discard a caller's precondition.
        if targets.len() > 1 && asked.expected_revision.is_some() {
            return Err(Failure::invalid(
                "expected_revision requires a target resolving to one group; read each group's queue and address it separately for revision-bound playback",
            ));
        }
        let tracks: Vec<_> = asked
            .tracks
            .iter()
            .map(|track| {
                (
                    track.source_uri.clone(),
                    track.title.as_ref().unwrap_or(&track.source_uri).clone(),
                )
            })
            .collect();
        let mut commands = Vec::with_capacity(targets.len());
        for target in targets {
            let coordinator = &target.coordinator.id;
            let host = target.coordinator.ip;
            self.confirm_source_audience(rooms.households, coordinator)?;
            // Retain every destination before any household's renderer
            // learning. The set executor rechecks the complete selection
            // before caps, snapshots or playback writes and again per group.
            let destination =
                control::TrackBatchDestination::read(transport, host, coordinator)?;
            confirm_destination_revision(
                transport,
                host,
                coordinator,
                asked.expected_revision.as_deref(),
            )?;
            commands.push(Command::PlaySelection {
                coordinator: coordinator.clone(),
                tracks: tracks.clone(),
                destination,
            });
        }
        Ok(commands)
    }

    /// Queue a reviewed ordered selection without starting, pausing or
    /// replacing playback. All successful insertions share one undo receipt.
    pub fn queue_tracks(
        &self,
        client: &Client,
        req: &QueueTracksRequest,
    ) -> Result<QueueAddedDto, Failure> {
        self.authorize_logged(client, ADD)?;
        let asked = req.normalized()?;
        let intent = format!(
            "{ADD}: {} {} selected tracks next={}",
            asked.zone,
            asked.tracks.len(),
            asked.next
        );
        let mut uncertain = false;
        let result = (|| {
            let (households, group) = self.queue_households(client, &asked.zone)?;
            let coordinator = group.coordinator.clone();
            let player = control::locate(&households, &coordinator)?;
            let destination =
                control::TrackBatchDestination::read(&*self.transport, player.ip, &coordinator)
                    .map_err(CoreError::from)?;
            // Read the caller's token after retaining the core destination.
            // Core then keeps that same queue/source/group through every add.
            confirm_destination_revision(
                &*self.transport,
                player.ip,
                &coordinator,
                asked.expected_revision.as_deref(),
            )?;
            self.guard(client).authorize(ADD, false)?;
            self.queue_group_unchanged(player.ip, &group)?;
            let tracks: Vec<_> = asked
                .tracks
                .iter()
                .map(|track| {
                    (
                        track.source_uri.as_str(),
                        track.title.as_deref().unwrap_or(&track.source_uri),
                    )
                })
                .collect();
            let watch = Observed::new(&*self.transport);
            let queued = control::enqueue_spotify_tracks_bound(
                &watch,
                &households,
                &coordinator,
                &tracks,
                asked.next,
                &destination,
            )
            .map_err(|error| {
                uncertain = watch.wrote()
                    && matches!(
                        &error,
                        CoreError::Proto(ProtoError::Network { .. } | ProtoError::Malformed(_))
                            | CoreError::Uncertain(_)
                    );
                spotify_failure(error).with_hint(
                    "Read the destination queue before another add; the selected tracks were not automatically retried.",
                )
            })?;
            let count = u32::try_from(queued.uris.len()).expect("bounded queue selection");
            let mut done = format!(
                "queued {count} selected track(s) in {}'s group at position {} in the supplied order",
                player.room_name, queued.first
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
                QueueAddedDto {
                    done,
                    zone: player.room_name.clone(),
                    first: queued.first,
                    count,
                    playing_queue: queued.playing_queue,
                },
                before,
            ))
        })();
        match result {
            Ok((added, before)) => {
                self.record(client, intent, "allow".into(), added.done.clone(), before);
                Ok(added)
            }
            Err(failure) => {
                self.notice(&failure);
                let before = uncertain.then(|| actions::before_state_not_undoable("queue edit"));
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
                    before,
                );
                Err(failure)
            }
        }
    }
}

#[cfg(test)]
mod tests;
