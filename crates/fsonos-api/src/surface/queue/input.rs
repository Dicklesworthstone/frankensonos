//! One queue-add contract for native household favorites and Spotify sources.
//! The original `QueueAddRequest` remains source-compatible for Rust callers.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::actions::{self, QueuedBefore};
use fsonos_core::favorites::{self, FavoriteKind};
use fsonos_core::policy::Client;
use fsonos_core::{control, favorite_queue};
use fsonos_proto::didl::{SpotifyBare, SpotifyContainer};
use serde::{Deserialize, Serialize};

use super::{ADD, QueueAddRequest, QueueAddedDto, Surface};
use crate::failure::{ErrorCode, Failure};
use crate::request::{PlayFavoriteRequest, PlayRequest, zone_name};

/// `POST /queue` and MCP `add_to_queue`: give exactly one of `source_uri`
/// (Spotify) and `favorite` (resolved only within the target household).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueRequest {
    pub zone: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// A native favorite's id, numbered position or unambiguous title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorite: Option<String>,
    /// Cosmetic title for a Spotify source; native metadata is kept intact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Insert after the currently playing queue track, otherwise append.
    #[serde(default)]
    pub next: bool,
}

impl From<&QueueAddRequest> for QueueRequest {
    fn from(req: &QueueAddRequest) -> Self {
        Self {
            zone: req.zone.clone(),
            source_uri: Some(req.source_uri.clone()),
            favorite: None,
            title: req.title.clone(),
            next: req.next,
        }
    }
}

impl QueueRequest {
    /// Validate the mutually exclusive source choice before discovery or I/O.
    pub fn validate(&self) -> Result<(), Failure> {
        zone_name("zone", &self.zone)?;
        match (&self.source_uri, &self.favorite) {
            (Some(uri), None) => {
                let normalized = PlayRequest {
                    zone: self.zone.clone(),
                    source_uri: uri.clone(),
                    title: self.title.clone(),
                }
                .normalized()?;
                if normalized.source_uri.starts_with("spotify:track:")
                    || SpotifyContainer::of(&normalized.source_uri).is_some()
                    || SpotifyBare::of(&normalized.source_uri).is_some()
                {
                    Ok(())
                } else {
                    Err(Failure::invalid(
                        "source_uri must name a Spotify track, album, playlist, artist, episode or show; use favorite for native sources",
                    ))
                }
            }
            (None, Some(favorite)) => {
                if self.title.is_some() {
                    return Err(Failure::invalid(
                        "title is only valid with source_uri; a favorite keeps its native metadata",
                    ));
                }
                validate_favorite(&self.zone, favorite)
            }
            _ => Err(Failure::invalid(
                "give exactly one of source_uri or favorite for the queue",
            )),
        }
    }
}

fn validate_favorite(zone: &str, favorite: &str) -> Result<(), Failure> {
    let req = PlayFavoriteRequest {
        zone: zone.into(),
        favorite: favorite.into(),
    };
    req.zone()?;
    let query = req.favorite()?;
    if query.chars().count() > 1024 || query.chars().any(char::is_control) {
        return Err(Failure::invalid(
            "favorite must be at most 1024 characters without control characters",
        ));
    }
    Ok(())
}

impl Surface {
    /// Add either kind through the same policy and action-log boundary.
    pub fn queue_request(
        &self,
        client: &Client,
        req: &QueueRequest,
    ) -> Result<QueueAddedDto, Failure> {
        self.authorize_logged(client, ADD)?;
        req.validate()?;
        match (&req.source_uri, &req.favorite) {
            (Some(uri), None) => self.queue_add(
                client,
                &QueueAddRequest {
                    zone: req.zone.clone(),
                    source_uri: uri.clone(),
                    title: req.title.clone(),
                    next: req.next,
                },
            ),
            (None, Some(favorite)) => self.queue_favorite(client, &req.zone, favorite, req.next),
            _ => Err(Failure::invalid(
                "give exactly one of source_uri or favorite",
            )),
        }
    }

    /// Queue the named native favorite in the room's freshly resolved group.
    /// Only identities actually confirmed on the queue are recorded for undo.
    /// No caller-supplied renderer metadata is accepted by this path.
    pub fn queue_favorite(
        &self,
        client: &Client,
        zone: &str,
        favorite: &str,
        next: bool,
    ) -> Result<QueueAddedDto, Failure> {
        self.authorize_logged(client, ADD)?;
        validate_favorite(zone, favorite)?;
        let zone = zone.trim();
        let query = favorite.trim();
        let intent = format!("{ADD}: {zone} favorite {query:?} next={next}");
        // Whether the add failed after tracks may have been queued.
        let uncertain = std::cell::Cell::new(false);
        let result = (|| {
            let (households, group) = self.queue_households(client, zone)?;
            let coordinator = group.coordinator.clone();
            let player = control::locate(&households, &coordinator)?;
            let favorites = favorites::list(&*self.transport, &households, &coordinator)?;
            let selected = favorites::find(&favorites, query)?;
            if !matches!(selected.kind, FavoriteKind::Track | FavoriteKind::Container) {
                return Err(Failure::invalid(format!(
                    "favorite {:?} is not a track, album or playlist and cannot be queued",
                    selected.title
                ))
                .with_hint(
                    "Use play_favorite to play a station directly; browse shortcuts cannot play.",
                ));
            }
            self.queue_group_unchanged(player.ip, &group)?;
            let queued = favorite_queue::enqueue(
                &*self.transport,
                &households,
                &coordinator,
                selected,
                next,
            )
            .map_err(|error| {
                uncertain.set(favorite_queue::is_uncertain(&error));
                Failure::from(error).with_hint(
                    "Read the queue before another add; the favorite was not automatically retried.",
                )
            })?;
            let count = u32::try_from(queued.uris.len()).map_err(|_| {
                Failure::new(
                    ErrorCode::Internal,
                    "confirmed queue range exceeds u32 tracks",
                )
            })?;
            let mut done = format!(
                "queued {count} track(s) from favorite {:?} in {zone} at position {}",
                selected.title, queued.first
            );
            if !queued.playing_queue {
                done.push_str(" (the group is not playing its queue; playback was not changed)");
            }
            let before = actions::before_state_queued(&[QueuedBefore {
                coordinator,
                first: queued.first,
                uris: queued.uris,
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
                // An add with an uncertain outcome may have queued tracks no
                // undo range records: undo stops at it.
                let kept = uncertain
                    .get()
                    .then(|| actions::before_state_not_undoable("queue edit"));
                self.record(
                    client,
                    intent,
                    "allow".into(),
                    format!("failed: {}", failure.detail),
                    kept,
                );
                Err(failure)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn old_spotify_bodies_and_rust_requests_keep_their_meaning() {
        let old = QueueAddRequest {
            zone: "Den@S2".into(),
            source_uri: "spotify:track:0SimQueueTrack00000021".into(),
            title: Some("Track".into()),
            next: true,
        };
        let req: QueueRequest =
            serde_json::from_value(serde_json::to_value(&old).unwrap()).unwrap();
        assert_eq!(req, QueueRequest::from(&old));
        req.validate().unwrap();
        assert!(
            serde_json::to_value(&req)
                .unwrap()
                .get("favorite")
                .is_none()
        );
    }

    #[test]
    fn native_favorites_default_to_append_and_keep_literal_unicode() {
        for favorite in ["FV:2/7", "2", "Dvořák & friends"] {
            let req: QueueRequest =
                serde_json::from_value(json!({"zone": "Den@S1", "favorite": favorite})).unwrap();
            req.validate().unwrap();
            assert!(!req.next);
            assert_eq!(req.favorite.as_deref(), Some(favorite));
        }
    }

    #[test]
    fn ambiguous_sources_and_silently_ignored_fields_are_refused() {
        for body in [
            json!({"zone": "Den"}),
            json!({"zone": "Den", "favorite": ""}),
            json!({"zone": "Den", "favorite": "a\nb"}),
            json!({"zone": "Den", "favorite": "a".repeat(1025)}),
            json!({"zone": "", "favorite": "FV:2/1"}),
            json!({"zone": "Den", "favorite": "FV:2/1", "title": "discarded"}),
            json!({"zone": "Den", "favorite": "FV:2/1", "source_uri": "spotify:track:0SimQueueTrack00000021"}),
            json!({"zone": "Den", "source_uri": ""}),
            json!({"zone": "Den", "source_uri": "x-rincon-mp3radio://example.invalid/radio"}),
        ] {
            let req: QueueRequest = serde_json::from_value(body.clone()).unwrap();
            assert!(req.validate().is_err(), "{body}");
        }
        for body in [
            json!({"zone": "Den", "favorite": "FV:2/1", "metadata": "untrusted"}),
            json!({"zone": "Den", "favorite": "FV:2/1", "client": "cli"}),
            json!({"zone": "Den", "favorite": "FV:2/1", "next": "true"}),
        ] {
            assert!(serde_json::from_value::<QueueRequest>(body).is_err());
        }
    }
}
