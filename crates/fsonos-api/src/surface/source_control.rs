//! Source playback and queue insertion bound to an explicitly reviewed room set.
//! The resolver revision is compared against the same snapshot used for planning.
//! Concrete destinations then survive catalog reads, caps and every speaker write.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::HouseholdState;
use fsonos_core::actions::QueueUndo;
use fsonos_core::policy::Client;
use serde::{Deserialize, Serialize};
use std::sync::PoisonError;

use super::audience::AudienceTransport;
use super::line_in::{PlayLineInRequest, plan_line_in};
use super::players::targets::resolve_view;
use super::queue::{
    PlayTracksRequest, QueueAddOutcome, QueueRequest, QueueTrackInput, QueueTracksRequest,
};
use super::source_selection::{NativeSource, native_sources};
use super::{Surface, affected, replacement_undo, room_view, set_runs};
use crate::execute::{self, Executed, OutcomeDto};
use crate::failure::{ErrorCode, Failure};
use crate::guard::Guard;
use crate::plan::{self, Command, Rooms};
use crate::request::{PlayRequest, zone_name};

/// Playback replaces the current source; queue modes preserve playback.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewedSourceMode {
    Play,
    Next,
    Append,
}

impl fastapi_openapi::JsonSchema for ReviewedSourceMode {
    fn schema() -> fastapi_openapi::Schema {
        fastapi_openapi::Schema::string_enum(
            ["play", "next", "append"]
                .into_iter()
                .map(str::to_owned)
                .collect(),
        )
    }

    fn schema_name() -> Option<&'static str> {
        Some("ReviewedSourceMode")
    }
}

/// Native identifiers and metadata are resolved within each target household.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReviewedSource {
    Uri {
        source_uri: String,
        #[serde(default)]
        title: Option<String>,
    },
    /// Concrete Spotify tracks and episodes, preserving order and repeats.
    Tracks {
        tracks: Vec<QueueTrackInput>,
    },
    Favorite {
        favorite: String,
    },
    Playlist {
        playlist: String,
    },
    /// One exact physical input from the destination household's /line-in read.
    LineIn {
        source: String,
    },
}

// The pinned derive ignores serde's enum names and discriminator. Describe
// the accepted source objects exactly; arbitrary metadata is never admitted.
impl fastapi_openapi::JsonSchema for ReviewedSource {
    fn schema() -> fastapi_openapi::Schema {
        use fastapi_openapi::{ObjectSchema, Schema};

        let variant = |kind: &str, required: &[&str], fields: Vec<(&str, Schema)>| {
            let mut properties: std::collections::HashMap<_, _> = fields
                .into_iter()
                .map(|(name, schema)| (name.to_owned(), schema))
                .collect();
            properties.insert("kind".into(), Schema::string_enum(vec![kind.into()]));
            Schema::Object(ObjectSchema {
                title: None,
                description: None,
                properties,
                required: required.iter().map(|name| (*name).to_owned()).collect(),
                additional_properties: Some(Box::new(Schema::Boolean(false))),
            })
        };
        Schema::one_of(vec![
            variant(
                "uri",
                &["kind", "source_uri"],
                vec![
                    ("source_uri", String::schema()),
                    ("title", Option::<String>::schema()),
                ],
            ),
            variant(
                "tracks",
                &["kind", "tracks"],
                vec![("tracks", Vec::<QueueTrackInput>::schema())],
            ),
            variant(
                "favorite",
                &["kind", "favorite"],
                vec![("favorite", String::schema())],
            ),
            variant(
                "playlist",
                &["kind", "playlist"],
                vec![("playlist", String::schema())],
            ),
            variant(
                "line_in",
                &["kind", "source"],
                vec![("source", String::schema())],
            ),
        ])
    }

    fn schema_name() -> Option<&'static str> {
        Some("ReviewedSource")
    }
}

/// `POST /sources/control`: one source, one admitted audience, one action.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ReviewedSourceRequest {
    pub zone: String,
    /// Exact revision from `/rooms/resolve?target=<zone>`.
    pub expected_revision: String,
    pub mode: ReviewedSourceMode,
    pub source: ReviewedSource,
}

/// Existing playback or insertion receipts, without an extra response wrapper.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum ReviewedSourceOutcome {
    Play(OutcomeDto),
    Queue(QueueAddOutcome),
}

impl fastapi_openapi::JsonSchema for ReviewedSourceOutcome {
    fn schema() -> fastapi_openapi::Schema {
        fastapi_openapi::Schema::one_of(vec![
            <OutcomeDto as fastapi_openapi::JsonSchema>::schema(),
            <QueueAddOutcome as fastapi_openapi::JsonSchema>::schema(),
        ])
    }

    fn schema_name() -> Option<&'static str> {
        Some("ReviewedSourceOutcome")
    }
}

pub(super) fn validate_revision(revision: &str) -> Result<(), Failure> {
    if revision.len() != 64
        || !revision
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return Err(Failure::invalid(
            "expected_revision must be the revision returned by /rooms/resolve for this target",
        ));
    }
    Ok(())
}

pub(super) fn confirm_revision(
    rooms: &Rooms<'_>,
    target: &str,
    revision: &str,
) -> Result<(), Failure> {
    if resolve_view(rooms, target)?.revision != revision {
        return Err(Failure::new(
            ErrorCode::NotCoordinator,
            "the reviewed source target or its physical groups changed; nothing was changed",
        )
        .with_hint(
            "Read the target again and review every affected group before a new source request.",
        ));
    }
    Ok(())
}

impl ReviewedSourceRequest {
    /// Invalid combinations fail before discovery or source reads.
    pub fn validate(&self) -> Result<(), Failure> {
        zone_name("zone", &self.zone)?;
        validate_revision(&self.expected_revision)?;
        if let ReviewedSource::LineIn { source } = &self.source {
            if self.mode != ReviewedSourceMode::Play {
                return Err(Failure::invalid("physical line-in inputs support play only"));
            }
            return PlayLineInRequest {
                zone: self.zone.clone(),
                source: source.clone(),
            }
            .validate();
        }
        if let Some(request) = self.tracks_request() {
            return request.validate();
        }
        if self.mode == ReviewedSourceMode::Play
            && let ReviewedSource::Uri { source_uri, title } = &self.source
        {
            PlayRequest {
                zone: self.zone.clone(),
                source_uri: source_uri.clone(),
                title: title.clone(),
            }
            .normalized()?;
            return Ok(());
        }
        self.queue_request()?.validate()
    }

    fn tool(&self) -> &'static str {
        match (&self.mode, &self.source) {
            (
                ReviewedSourceMode::Play,
                ReviewedSource::Uri { .. }
                | ReviewedSource::Tracks { .. }
                | ReviewedSource::LineIn { .. },
            ) => "play",
            (ReviewedSourceMode::Play, _) => "play_favorite",
            _ => super::queue::ADD,
        }
    }

    fn tracks_request(&self) -> Option<QueueTracksRequest> {
        let ReviewedSource::Tracks { tracks } = &self.source else {
            return None;
        };
        Some(QueueTracksRequest {
            zone: self.zone.clone(),
            tracks: tracks.clone(),
            next: self.mode == ReviewedSourceMode::Next,
            // The enclosing revision describes the reviewed audience. It must
            // never be passed off as one coordinator's queue revision.
            expected_revision: None,
        })
    }

    fn queue_request(&self) -> Result<QueueRequest, Failure> {
        let mut request = QueueRequest {
            zone: self.zone.clone(),
            next: self.mode == ReviewedSourceMode::Next,
            ..QueueRequest::default()
        };
        match &self.source {
            ReviewedSource::Uri { source_uri, title } => {
                request.source_uri = Some(source_uri.clone());
                request.title = title.clone();
            }
            ReviewedSource::Favorite { favorite } => request.favorite = Some(favorite.clone()),
            ReviewedSource::Playlist { playlist } => request.playlist = Some(playlist.clone()),
            ReviewedSource::LineIn { .. } => {
                return Err(Failure::invalid("physical line-in inputs support play only"));
            }
            ReviewedSource::Tracks { .. } => {
                return Err(Failure::new(
                    ErrorCode::Internal,
                    "ordered tracks require their selection queue path",
                ));
            }
        }
        Ok(request)
    }

    fn plan(&self, surface: &Surface, rooms: &Rooms<'_>) -> Result<Vec<Command>, Failure> {
        let transport = &*surface.transport;
        let target = zone_name("zone", &self.zone)?;
        confirm_revision(rooms, target, &self.expected_revision)?;
        match &self.source {
            ReviewedSource::Uri { source_uri, title } => plan::plan_play_set(
                rooms,
                &PlayRequest {
                    zone: target.into(),
                    source_uri: source_uri.clone(),
                    title: title.clone(),
                },
            ),
            ReviewedSource::Tracks { tracks } => surface.plan_play_tracks(
                transport,
                rooms,
                &PlayTracksRequest {
                    zone: target.into(),
                    tracks: tracks.clone(),
                    expected_revision: None,
                },
            ),
            ReviewedSource::Favorite { favorite } => native_sources(
                transport,
                rooms,
                target,
                NativeSource::Favorite(favorite.trim()),
            ),
            ReviewedSource::Playlist { playlist } => native_sources(
                transport,
                rooms,
                target,
                NativeSource::Playlist(playlist.trim()),
            ),
            ReviewedSource::LineIn { source } => {
                plan_line_in(transport, rooms, target, source, true)
            }
        }
    }
}

impl Surface {
    /// Apply the review once. Neither aliases nor a single selected group are
    /// resolved again after admission; source reads cannot expand the audience.
    pub fn control_reviewed_source(
        &self,
        client: &Client,
        request: &ReviewedSourceRequest,
    ) -> Result<ReviewedSourceOutcome, Failure> {
        request.validate()?;
        if request.mode != ReviewedSourceMode::Play {
            let result = match request.tracks_request() {
                Some(tracks) => {
                    self.queue_tracks_targets_reviewed(client, &tracks, &request.expected_revision)
                }
                None => self.queue_request_targets_reviewed(
                    client,
                    &request.queue_request()?,
                    &request.expected_revision,
                ),
            };
            return result.map(ReviewedSourceOutcome::Queue);
        }
        let tool = request.tool();
        self.authorize_logged(client, tool)?;
        let households = self.households()?;
        let aliases = self.aliases();
        let rooms = room_view(&households, aliases.as_ref(), client);
        let commands = request
            .plan(self, &rooms)
            .map_err(|failure| self.explain(failure))?;
        let commands = self
            .prepare_sources(&households, commands)
            .inspect_err(|failure| self.notice(failure))?;
        for command in &commands {
            let coordinator = command.addressed().ok_or_else(|| {
                Failure::new(ErrorCode::Internal, "a reviewed source has no destination")
            })?;
            self.confirm_reviewed_group(&households, coordinator)?;
        }
        self.control_prepared_set(
            client,
            tool,
            &households,
            commands,
            set_runs::Execution::ReviewedSource,
        )
        .map(ReviewedSourceOutcome::Play)
    }

    /// The ordinary caps, queue replacement receipts and history remain in
    /// force, but recovery cannot redirect this already reviewed destination.
    pub(super) fn execute_reviewed_source(
        &self,
        households: &[HouseholdState],
        guard: &Guard<'_>,
        command: &Command,
        queue_undo: &mut Option<QueueUndo>,
    ) -> Result<OutcomeDto, Failure> {
        let coordinator = command.addressed().ok_or_else(|| {
            Failure::new(ErrorCode::Internal, "a reviewed source has no destination")
        })?;
        let fence = AudienceTransport::new(self, households, coordinator);
        let _changing = self
            .log
            .as_ref()
            .filter(|_| replacement_undo::replaces(command))
            .map(|log| log.restoring.read().unwrap_or_else(PoisonError::into_inner));
        let result = (|| {
            fence.check()?;
            self.confirm_source_selection(households, command)?;
            let pending = replacement_undo::capture(self, households, command);
            if let Some(sleep) = &self.sleep {
                for coordinator in affected(households, command) {
                    sleep.playback_changing(&coordinator);
                }
            }
            let mut lowered = self.lower_before_playing_on(&fence, households, guard, command)?;
            let pending = replacement_undo::confirm(self, households, pending, &lowered.volumes);
            let (bounded, mut notes) = guard.bound(&fence, households, command.clone())?;
            let mut executed = if pending.is_some() {
                execute::execute_with_receipt(&fence, households, &bounded)
            } else {
                execute::execute(&fence, households, &bounded).map(Executed::from)
            }?;
            *queue_undo =
                replacement_undo::finish(self, households, pending, executed.replacement.as_ref());
            self.remember_play(households, &bounded);
            lowered.notes.append(&mut notes);
            lowered.notes.append(&mut executed.outcome.notes);
            executed.outcome.notes = lowered.notes;
            Ok(executed.outcome)
        })();
        fence.finish(result)
    }
}

#[cfg(test)]
mod tests;
