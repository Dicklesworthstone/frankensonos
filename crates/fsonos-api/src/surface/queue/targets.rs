//! Add one source to independent group queues with one receipt and undo action.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::actions::{self, BeforeState, QueuedBefore};
use fsonos_core::control::TrackBatchDestination;
use fsonos_core::favorites::{Favorite, FavoriteKind};
use fsonos_core::policy::Client;
use fsonos_core::{CoreError, HouseholdState, control, favorite_queue};
use fsonos_proto::ProtoError;
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt::Write as _;
use std::net::IpAddr;
use std::sync::PoisonError;

use super::{ADD, QueueAddedDto, QueueRequest, Surface};
use crate::execute::spotify_failure;
use crate::heal::Observed;
use crate::plan::{Command, Rooms, resolve_set};
use crate::request::PlayRequest;
use crate::source::{SourceSelection, is_catalog_source};
use crate::surface::room_view;
use crate::surface::source_selection::{NativeSource, native_sources};
use crate::{ApiError, ErrorCode, Failure};

/// The existing single-group response, or an explicit receipt for a set.
/// Group receipts have no aggregate queue position: each queue owns its own.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum QueueAddOutcome {
    Single(QueueAddedDto),
    Groups(QueueAddedSetDto),
}

impl fastapi_openapi::JsonSchema for QueueAddOutcome {
    fn schema() -> fastapi_openapi::Schema {
        fastapi_openapi::Schema::one_of(vec![
            <QueueAddedDto as fastapi_openapi::JsonSchema>::schema(),
            <QueueAddedSetDto as fastapi_openapi::JsonSchema>::schema(),
        ])
    }

    fn schema_name() -> Option<&'static str> {
        Some("QueueAddOutcome")
    }
}

impl QueueAddOutcome {
    #[must_use]
    pub fn done(&self) -> &str {
        match self {
            Self::Single(added) => &added.done,
            Self::Groups(added) => &added.done,
        }
    }

    /// Include every destination's result, including uncertainty and undo.
    #[must_use]
    pub fn text(&self) -> String {
        let Self::Groups(batch) = self else {
            return format!("{}\n", self.done());
        };
        let mut text = details(&batch.done, &batch.added, &batch.failed);
        if !batch.logged {
            text.push_str("The action was not logged; no batch undo is available.\n");
        } else if !batch.undoable {
            text.push_str(
                "No safe batch undo is available; inspect the queues before another add.\n",
            );
        }
        text
    }
}

/// Once dispatch begins, every target has either a confirmed range or an
/// error here, even when none succeeded. Never repeat the whole batch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueueAddedSetDto {
    pub done: String,
    /// Every selected group returned a confirmed insertion.
    pub complete: bool,
    pub added: Vec<QueueAddedTargetDto>,
    pub failed: Vec<QueueTargetFailureDto>,
    /// The one aggregate action was saved successfully.
    pub logged: bool,
    /// All possible effects have verified undo ownership in that saved action.
    pub undoable: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueueAddedTargetDto {
    pub household: String,
    pub coordinator: String,
    pub queue: QueueAddedDto,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueueTargetFailureDto {
    pub household: String,
    pub coordinator: String,
    pub zone: String,
    pub error: ApiError,
}

struct Target {
    member: PlayerId,
    coordinator: PlayerId,
    host: IpAddr,
    zone: String,
    household: String,
}

struct Prepared {
    target: Target,
    destination: TrackBatchDestination,
}

enum Source {
    Spotify {
        uri: String,
        title: String,
        selection: Option<SourceSelection>,
    },
    Native(HashMap<PlayerId, Favorite>),
}

fn targets(rooms: &Rooms<'_>, zone: &str) -> Result<Vec<Target>, Failure> {
    let mut targets = Vec::new();
    for target in resolve_set(rooms, zone)? {
        if targets
            .iter()
            .any(|known: &Target| known.coordinator == target.coordinator.id)
        {
            continue;
        }
        let view = crate::zones::zone_for_target(rooms.households, &target, |_| {
            fsonos_types::TransportState::Unknown
        });
        targets.push(Target {
            member: target.player.id.clone(),
            coordinator: target.coordinator.id.clone(),
            host: target.coordinator.ip,
            zone: target.coordinator.room_name.clone(),
            household: view.household,
        });
    }
    Ok(targets)
}

impl Surface {
    /// Queue one source in each selected group without changing playback.
    /// A grouped-room alias with one coordinator keeps the original response.
    pub fn queue_request_targets(
        &self,
        client: &Client,
        request: &QueueRequest,
    ) -> Result<QueueAddOutcome, Failure> {
        self.authorize_logged(client, ADD)?;
        request.validate()?;
        let households = self.households()?;
        let aliases = self.aliases();
        let rooms = room_view(&households, aliases.as_ref(), client);
        let targets = targets(&rooms, request.zone.trim()).map_err(|error| self.explain(error))?;
        if let [target] = targets.as_slice() {
            // The alias may name two members of one group. Pin its first
            // requested room, rather than passing that alias to resolve_one.
            let request = QueueRequest {
                zone: target.member.0.clone(),
                ..request.clone()
            };
            return self
                .queue_request(client, &request)
                .map(QueueAddOutcome::Single);
        }
        let prepared = targets
            .into_iter()
            .map(|target| {
                self.confirm_source_audience(&households, &target.coordinator)?;
                let destination = TrackBatchDestination::read(
                    &*self.transport,
                    target.host,
                    &target.coordinator,
                )?;
                Ok(Prepared {
                    target,
                    destination,
                })
            })
            .collect::<Result<Vec<_>, Failure>>()?;
        // Every destination is retained before native or Spotify catalog I/O.
        let source = self.queue_targets_source(&rooms, request, &prepared)?;
        self.guard(client).authorize(ADD, false)?;
        for group in &prepared {
            self.confirm_queue_target(&households, group)?;
        }
        Ok(QueueAddOutcome::Groups(self.queue_targets_run(
            client,
            &households,
            request,
            &source,
            &prepared,
        )))
    }

    fn queue_targets_source(
        &self,
        rooms: &Rooms<'_>,
        request: &QueueRequest,
        prepared: &[Prepared],
    ) -> Result<Source, Failure> {
        if let Some(uri) = &request.source_uri {
            let normalized = PlayRequest {
                zone: request.zone.clone(),
                source_uri: uri.clone(),
                title: request.title.clone(),
            }
            .normalized()?;
            let selection = is_catalog_source(&normalized.source_uri)
                .then(|| self.catalog_selection(&normalized.source_uri))
                .transpose()?;
            let renderer_source = selection
                .as_ref()
                .map_or(normalized.source_uri.as_str(), |selection| {
                    selection.tracks[0].0.as_str()
                });
            for group in prepared {
                control::check_spotify_source(
                    &*self.transport,
                    rooms.households,
                    &group.target.coordinator,
                    renderer_source,
                )
                .map_err(spotify_failure)?;
            }
            return Ok(Source::Spotify {
                title: normalized
                    .title
                    .unwrap_or_else(|| normalized.source_uri.clone()),
                uri: normalized.source_uri,
                selection,
            });
        }
        let source = match (&request.favorite, &request.playlist) {
            (Some(query), None) => NativeSource::Favorite(query.trim()),
            (None, Some(query)) => NativeSource::Playlist(query.trim()),
            _ => return Err(Failure::invalid("give exactly one source for the queue")),
        };
        let mut selected = HashMap::new();
        for command in native_sources(&*self.transport, rooms, request.zone.trim(), source)? {
            let Command::PlayFavorite {
                coordinator,
                favorite,
                ..
            } = command
            else {
                return Err(Failure::new(
                    ErrorCode::Internal,
                    "native queue source was not selected",
                ));
            };
            if !matches!(favorite.kind, FavoriteKind::Track | FavoriteKind::Container) {
                return Err(Failure::invalid(format!(
                    "favorite {:?} is not a track, album or playlist and cannot be queued",
                    favorite.title
                ))
                .with_hint("Use play_favorite to play a station directly."));
            }
            selected.insert(coordinator, favorite);
        }
        Ok(Source::Native(selected))
    }

    fn confirm_queue_target(
        &self,
        households: &[HouseholdState],
        group: &Prepared,
    ) -> Result<(), Failure> {
        group.destination.confirm(
            &*self.transport,
            group.target.host,
            &group.target.coordinator,
        )?;
        self.confirm_source_audience(households, &group.target.coordinator)
    }

    fn queue_targets_run(
        &self,
        client: &Client,
        households: &[HouseholdState],
        request: &QueueRequest,
        source: &Source,
        prepared: &[Prepared],
    ) -> QueueAddedSetDto {
        let _changing = self
            .log
            .as_ref()
            .map(|log| log.restoring.read().unwrap_or_else(PoisonError::into_inner));
        let mut added = Vec::new();
        let mut failed = Vec::new();
        let mut undo = Vec::new();
        let mut uncertain = false;
        for group in prepared {
            let result = self
                .guard(client)
                .authorize(ADD, false)
                .and_then(|()| self.confirm_queue_target(households, group))
                .and_then(|()| source.enqueue(self, households, group, request.next));
            match result {
                Ok(queued) => {
                    added.push(group.receipt(&queued));
                    undo.push(QueuedBefore {
                        coordinator: group.target.coordinator.clone(),
                        first: queued.first,
                        uris: queued.uris,
                        proof: queued.proof,
                    });
                }
                Err(failure) => {
                    self.notice(&failure);
                    uncertain |= failure.code == ErrorCode::OutcomeUncertain;
                    failed.push(group.failure(&failure));
                }
            }
        }
        let before = if uncertain {
            Some(actions::before_state_not_undoable("uncertain queue batch"))
        } else {
            actions::before_state_queued(&undo)
        };
        let undoable = before.as_deref().is_some_and(|before| {
            BeforeState::parse(before)
                .is_ok_and(|before| before.not_undoable.is_none() && !before.queued.is_empty())
        });
        let done = summary(added.len(), &failed);
        let source_label = request
            .source_uri
            .as_deref()
            .or(request.favorite.as_deref())
            .or(request.playlist.as_deref())
            .unwrap_or_default();
        let logged = self.record(
            client,
            format!(
                "{ADD}: {} {source_label:?} next={}",
                request.zone, request.next
            ),
            "allow".into(),
            details(&done, &added, &failed),
            before,
        );
        QueueAddedSetDto {
            done,
            complete: failed.is_empty(),
            added,
            failed,
            logged,
            undoable: logged && undoable,
        }
    }
}

impl Source {
    fn enqueue(
        &self,
        surface: &Surface,
        households: &[HouseholdState],
        group: &Prepared,
        next: bool,
    ) -> Result<control::Queued, Failure> {
        let watch = Observed::new(&*surface.transport);
        let coordinator = &group.target.coordinator;
        let result = match self {
            Self::Spotify {
                uri,
                title,
                selection: None,
            } => control::enqueue_spotify_bound(
                &watch,
                households,
                coordinator,
                uri,
                title,
                next,
                &group.destination,
            ),
            Self::Spotify {
                selection: Some(selection),
                ..
            } => {
                let tracks: Vec<_> = selection
                    .tracks
                    .iter()
                    .map(|(uri, title)| (uri.as_str(), title.as_str()))
                    .collect();
                control::enqueue_spotify_tracks_bound(
                    &watch,
                    households,
                    coordinator,
                    &tracks,
                    next,
                    &group.destination,
                )
            }
            Self::Native(selected) => {
                let favorite = selected.get(coordinator).ok_or_else(|| {
                    Failure::new(
                        ErrorCode::Internal,
                        "this group's native queue source was lost",
                    )
                })?;
                favorite_queue::enqueue_bound(
                    &watch,
                    households,
                    coordinator,
                    favorite,
                    next,
                    &group.destination,
                )
            }
        };
        result.map_err(|error| {
            let uncertain = watch.wrote()
                && matches!(
                    &error,
                    CoreError::Proto(ProtoError::Network { .. } | ProtoError::Malformed(_))
                        | CoreError::Uncertain(_)
                );
            let failure = spotify_failure(error);
            if uncertain && failure.code != ErrorCode::OutcomeUncertain {
                Failure::new(ErrorCode::OutcomeUncertain, failure.detail)
            } else {
                failure
            }
        })
    }
}

impl Prepared {
    fn receipt(&self, queued: &control::Queued) -> QueueAddedTargetDto {
        let count = u32::try_from(queued.uris.len()).unwrap_or(u32::MAX);
        let mut done = format!("queued {count} track(s) at position {}", queued.first);
        if !queued.playing_queue {
            done.push_str(" (the group is not playing its queue; playback was not changed)");
        }
        QueueAddedTargetDto {
            household: self.target.household.clone(),
            coordinator: self.target.coordinator.0.clone(),
            queue: QueueAddedDto {
                done,
                zone: self.target.zone.clone(),
                first: queued.first,
                count,
                playing_queue: queued.playing_queue,
            },
        }
    }

    fn failure(&self, failure: &Failure) -> QueueTargetFailureDto {
        let mut error = failure.body();
        // This is one destination's result after batch dispatch, not permission
        // to replay a request that may already have changed the other queues.
        error.retryable = false;
        error.hint =
            "Read this group's queue before a new targeted add; never repeat the whole batch."
                .into();
        QueueTargetFailureDto {
            household: self.target.household.clone(),
            coordinator: self.target.coordinator.0.clone(),
            zone: self.target.zone.clone(),
            error,
        }
    }
}

fn details(done: &str, added: &[QueueAddedTargetDto], failed: &[QueueTargetFailureDto]) -> String {
    let mut text = format!("{done}\n");
    for added in added {
        let _ = writeln!(
            text,
            "{}@{}: {}",
            added.queue.zone, added.household, added.queue.done
        );
    }
    for failed in failed {
        let _ = writeln!(
            text,
            "{}@{}: {}: {} {}",
            failed.zone,
            failed.household,
            failed.error.code,
            failed.error.detail,
            failed.error.hint
        );
    }
    text
}

fn summary(added: usize, failed: &[QueueTargetFailureDto]) -> String {
    let total = added + failed.len();
    let mut done = format!("queued in {added} of {total} groups");
    if !failed.is_empty() {
        let uncertain = failed
            .iter()
            .filter(|failed| failed.error.code == ErrorCode::OutcomeUncertain)
            .count();
        let _ = write!(
            done,
            "; incomplete: {} failed, {uncertain} with uncertain outcomes; do not repeat the whole batch",
            failed.len()
        );
    }
    done
}

#[cfg(test)]
mod tests;
