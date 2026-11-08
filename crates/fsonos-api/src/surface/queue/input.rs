//! One queue-add contract for native favorites, saved Sonos playlists and Spotify sources.
//! The original `QueueAddRequest` remains source-compatible for Rust callers.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::actions::{self, QueuedBefore};
use fsonos_core::favorites::{self, FavoriteKind};
use fsonos_core::policy::Client;
use fsonos_core::{control, favorite_queue, playlists};
use fsonos_proto::didl::{SpotifyBare, SpotifyContainer};
use serde::{Deserialize, Serialize};

use super::{ADD, QueueAddRequest, QueueAddedDto, Surface};
use crate::failure::{ErrorCode, Failure};
use crate::request::{PlayFavoriteRequest, PlayRequest, zone_name};
use crate::surface::sonos_playlists;

/// `POST /queue` and MCP `add_to_queue`: give exactly one of `source_uri`
/// (Spotify), `favorite` or `playlist` (resolved only within the target household).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueRequest {
    pub zone: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_uri: Option<String>,
    /// A native favorite's id, numbered position or unambiguous title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub favorite: Option<String>,
    /// A saved Sonos playlist's SQ: id, numbered position or title.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playlist: Option<String>,
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
            playlist: None,
            title: req.title.clone(),
            next: req.next,
        }
    }
}

impl QueueRequest {
    /// Validate the mutually exclusive source choice before discovery or I/O.
    pub fn validate(&self) -> Result<(), Failure> {
        zone_name("zone", &self.zone)?;
        match (&self.source_uri, &self.favorite, &self.playlist) {
            (Some(uri), None, None) => {
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
                        "source_uri must name a Spotify track, album, playlist, artist, episode or show; use favorite or playlist for native sources",
                    ))
                }
            }
            (None, Some(favorite), None) => {
                if self.title.is_some() {
                    return Err(Failure::invalid(
                        "title is only valid with source_uri; a favorite keeps its native metadata",
                    ));
                }
                validate_favorite(&self.zone, favorite)
            }
            (None, None, Some(playlist)) => {
                if self.title.is_some() {
                    return Err(Failure::invalid(
                        "title is only valid with source_uri; a Sonos playlist keeps its native metadata",
                    ));
                }
                sonos_playlists::playlist_name(playlist)?;
                Ok(())
            }
            _ => Err(Failure::invalid(
                "give exactly one of source_uri, favorite or playlist for the queue",
            )),
        }
    }
}

#[derive(Clone, Copy)]
enum NativeCatalog {
    Favorites,
    Playlists,
}

impl NativeCatalog {
    fn label(self) -> &'static str {
        match self {
            Self::Favorites => "favorite",
            Self::Playlists => "Sonos playlist",
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
        match (&req.source_uri, &req.favorite, &req.playlist) {
            (Some(uri), None, None) => self.queue_add(
                client,
                &QueueAddRequest {
                    zone: req.zone.clone(),
                    source_uri: uri.clone(),
                    title: req.title.clone(),
                    next: req.next,
                },
            ),
            (None, Some(favorite), None) => {
                self.queue_favorite(client, &req.zone, favorite, req.next)
            }
            (None, None, Some(playlist)) => {
                self.queue_sonos_playlist(client, &req.zone, playlist, req.next)
            }
            _ => Err(Failure::invalid(
                "give exactly one of source_uri, favorite or playlist",
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
        self.queue_native(client, zone, favorite, next, NativeCatalog::Favorites)
    }

    /// Add a playlist saved on the target Sonos household, keeping its trusted
    /// native metadata and the same confirmed-range undo as native favorites.
    pub fn queue_sonos_playlist(
        &self,
        client: &Client,
        zone: &str,
        playlist: &str,
        next: bool,
    ) -> Result<QueueAddedDto, Failure> {
        self.queue_native(client, zone, playlist, next, NativeCatalog::Playlists)
    }

    fn queue_native(
        &self,
        client: &Client,
        zone: &str,
        query: &str,
        next: bool,
        catalog: NativeCatalog,
    ) -> Result<QueueAddedDto, Failure> {
        self.authorize_logged(client, ADD)?;
        match catalog {
            NativeCatalog::Favorites => validate_favorite(zone, query)?,
            NativeCatalog::Playlists => {
                zone_name("zone", zone)?;
                sonos_playlists::playlist_name(query)?;
            }
        }
        let zone = zone.trim();
        let query = query.trim();
        let label = catalog.label();
        let intent = format!("{ADD}: {zone} {label} {query:?} next={next}");
        // Whether the add failed after tracks may have been queued.
        let uncertain = std::cell::Cell::new(false);
        let result = (|| {
            let (households, group) = self.queue_households(client, zone)?;
            let coordinator = group.coordinator.clone();
            let player = control::locate(&households, &coordinator)?;
            let listed = match catalog {
                NativeCatalog::Favorites => {
                    favorites::list(&*self.transport, &households, &coordinator)?
                }
                NativeCatalog::Playlists => {
                    playlists::list(&*self.transport, &households, &coordinator)?
                }
            };
            let selected = match catalog {
                NativeCatalog::Favorites => favorites::find(&listed, query)?,
                NativeCatalog::Playlists => sonos_playlists::find(&listed, query)?,
            };
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
                Failure::from(error).with_hint(format!(
                    "Read the queue before another add; the {label} was not automatically retried.",
                ))
            })?;
            let count = u32::try_from(queued.uris.len()).map_err(|_| {
                Failure::new(
                    ErrorCode::Internal,
                    "confirmed queue range exceeds u32 tracks",
                )
            })?;
            let mut done = format!(
                "queued {count} track(s) from {label} {:?} in {zone} at position {}",
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
    fn saved_sonos_playlists_are_an_independent_native_source() {
        for playlist in ["SQ:1", "2", "Dvořák & friends"] {
            let req: QueueRequest =
                serde_json::from_value(json!({"zone": "Den@S1", "playlist": playlist})).unwrap();
            req.validate().unwrap();
            assert!(!req.next);
            assert!(req.favorite.is_none() && req.source_uri.is_none());
            assert_eq!(req.playlist.as_deref(), Some(playlist));
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
            json!({"zone": "Den", "playlist": ""}),
            json!({"zone": "Den", "playlist": "x\ny"}),
            json!({"zone": "Den", "playlist": "x".repeat(1025)}),
            json!({"zone": "Den", "playlist": "SQ:1", "title": "ignored"}),
            json!({"zone": "Den", "playlist": "SQ:1", "favorite": "FV:2/1"}),
            json!({"zone": "Den", "playlist": "SQ:1", "source_uri": "spotify:track:0SimQueueTrack00000021"}),
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
