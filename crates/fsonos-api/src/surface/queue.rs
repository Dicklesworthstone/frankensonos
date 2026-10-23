//! The queue a room's group plays from (`get_queue`,
//! `GET /zones/{room}/queue`, `fsonos queue`): its tracks in order, a page at
//! a time, how many there are, and which one plays; and adding a Spotify
//! track, album or playlist to it, at its end or next (`add_to_queue`,
//! `POST /queue`, `fsonos play --queue|--next`), logged so `undo` takes the
//! tracks off again. [`QueueRequest`] also accepts a native household favorite.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::actions::{self, QueuedBefore};
use fsonos_core::policy::Client;
use fsonos_core::{CoreError, control};
use fsonos_proto::ProtoError;
use fsonos_proto::didl::{SpotifyBare, SpotifyContainer};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::{Surface, resolve, room_view};
use crate::execute::spotify_failure;
use crate::failure::Failure;
use crate::heal::Observed;
use crate::request::PlayRequest;

mod input;
pub use input::QueueRequest;

mod mode;
pub use mode::{MODE_READ, MODE_WRITE, PlayModeDto, PlayModeRequest, PlayModeSetDto};

mod playback;
mod search;
mod snapshot;
pub use playback::{PLAY, QueuePlayRequest};
pub use search::{MAX_SEARCH_RESULTS, QueueSearchDto, QueueSearchRequest, SEARCH_PAGE_READS};

mod movement;
pub use movement::{MOVE, QueueMoveRequest, QueueMovedDto};

mod removal;
pub use removal::{QueueRemoveRequest, QueueRemovedDto, REMOVE};

/// The tool name the queue read is authorized as.
pub const TOOL: &str = "get_queue";

/// The tool name adding to a queue is authorized as.
pub const ADD: &str = "add_to_queue";

/// The most items one page carries (Sonos pages at 100).
pub const MAX_PAGE: u32 = 100;

/// A page of a group's queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueueDto {
    /// The group, named by its coordinator's room.
    pub zone: String,
    /// How many tracks the queue holds.
    pub total: u32,
    /// Opaque coordinator/revision token shared by pages of this queue.
    /// Compare for equality only; it is not a lock or a permanent identity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<String>,
    /// The queue position (1-based) playing now, when the group plays from
    /// its queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub playing: Option<u32>,
    /// From the page's first position on, in order.
    pub items: Vec<QueueItemDto>,
}

/// One track on the queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueueItemDto {
    /// 1-based.
    pub position: u32,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub creator: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// What the player renders, when it says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub uri: Option<String>,
}

/// `POST /queue` body (`add_to_queue`): a Spotify item for the queue of
/// `zone`'s group, at its end or (`next`) right after the track playing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct QueueAddRequest {
    pub zone: String,
    /// `spotify:track|album|playlist:<id>`, or an `open.spotify.com` link.
    pub source_uri: String,
    /// Display title for the speaker's queue.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Right after the track playing, instead of at the end.
    #[serde(default)]
    pub next: bool,
}

/// What an add to the queue did.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueueAddedDto {
    /// One line for people.
    pub done: String,
    /// The group, named by its coordinator's room.
    pub zone: String,
    /// The queue position (1-based) of the first track added.
    pub first: u32,
    /// How many tracks were added (an album or playlist adds its tracks).
    pub count: u32,
    /// Whether the group plays from its queue, so they will play.
    pub playing_queue: bool,
}

impl QueueAddedDto {
    /// The `done` line.
    #[must_use]
    pub fn text(&self) -> String {
        format!("{}\n", self.done)
    }
}

impl QueueDto {
    /// A heading, then a line per track, the playing one marked.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = match (self.total, self.playing) {
            (0, _) => return format!("{}'s queue is empty.\n", self.zone),
            (n, Some(at)) => format!("{}'s queue: {n} track(s), playing {at}.\n", self.zone),
            (n, None) => format!(
                "{}'s queue: {n} track(s), not playing from it.\n",
                self.zone
            ),
        };
        for item in &self.items {
            let mark = if self.playing == Some(item.position) {
                "▶"
            } else {
                " "
            };
            let by = item
                .creator
                .as_deref()
                .map_or_else(String::new, |c| format!(" — {c}"));
            let _ = writeln!(text, "{mark} {:>3}. {}{by}", item.position, item.title);
        }
        let shown = self.items.last().map_or(0, |i| i.position);
        if shown < self.total {
            let _ = writeln!(text, "  … {} more", self.total - shown);
        }
        text
    }
}

impl Surface {
    /// `limit` tracks (at most [`MAX_PAGE`]) of `zone`'s group's queue,
    /// after the first `offset` (`get_queue`, read-only).
    pub fn queue(
        &self,
        client: &Client,
        zone: &str,
        offset: u32,
        limit: u32,
    ) -> Result<QueueDto, Failure> {
        self.guard(client).authorize(TOOL, true)?;
        let limit = limit.clamp(1, MAX_PAGE);
        if offset.checked_add(limit).is_none() {
            return Err(Failure::invalid("invalid queue page range"));
        }
        let (households, group) = self.queue_households(client, zone)?;
        let coordinator = control::locate(&households, &group.coordinator)?;
        snapshot::read(&*self.transport, coordinator, offset, limit).inspect_err(|f| self.notice(f))
    }

    /// Add `req`'s Spotify track, album or playlist to the queue of
    /// `req.zone`'s group (`add_to_queue`), logged so `undo` takes the
    /// tracks off again.
    pub fn queue_add(
        &self,
        client: &Client,
        req: &QueueAddRequest,
    ) -> Result<QueueAddedDto, Failure> {
        self.authorize_logged(client, ADD)?;
        let asked = PlayRequest {
            zone: req.zone.clone(),
            source_uri: req.source_uri.clone(),
            title: req.title.clone(),
        }
        .normalized()?;
        let uri = asked.source_uri;
        if !(uri.starts_with("spotify:track:")
            || SpotifyContainer::of(&uri).is_some()
            || SpotifyBare::of(&uri).is_some())
        {
            return Err(Failure::invalid(format!(
                "only a Spotify track, album, playlist, artist, episode or show can be queued, not {uri}"
            )));
        }
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(
            room_view(&households, aliases.as_ref(), client),
            &asked.zone,
        )
        .map_err(|f| self.explain(f))?;
        let coordinator = target.coordinator.id.clone();
        let zone = target.coordinator.room_name.clone();
        let title = asked.title.as_deref().unwrap_or(&uri);
        let where_ = if req.next { " next" } else { "" };
        let intent = format!("{ADD}: {zone} {uri}{where_}");
        let watch = Observed::new(&*self.transport);
        let queued = match control::enqueue_spotify(
            &watch,
            &households,
            &coordinator,
            &uri,
            title,
            req.next,
        ) {
            Ok(queued) => queued,
            Err(error) => {
                // Once the add went out, a lost reply, or a read-back that
                // failed or did not match, may follow tracks that were
                // queued: undo then stops at this add rather than reach past
                // it. A failure before the add, or a refusal, queued nothing.
                let maybe = watch.wrote()
                    && matches!(
                        &error,
                        CoreError::Proto(ProtoError::Network { .. } | ProtoError::Malformed(_))
                    );
                let failure = spotify_failure(error);
                self.notice(&failure);
                self.record(
                    client,
                    intent,
                    "allow".into(),
                    format!("failed: {}", failure.detail),
                    maybe.then(|| actions::before_state_not_undoable("queue edit")),
                );
                return Err(failure);
            }
        };
        let count = u32::try_from(queued.uris.len()).unwrap_or(u32::MAX);
        let mut done = format!(
            "queued {count} track(s) of {uri} in {zone}'s group at position {}",
            queued.first
        );
        if !queued.playing_queue {
            done.push_str(" (the group isn't playing its queue: they play when it does)");
        }
        self.record(
            client,
            intent,
            "allow".into(),
            done.clone(),
            actions::before_state_queued(&[QueuedBefore {
                coordinator,
                first: queued.first,
                uris: queued.uris,
            }]),
        );
        Ok(QueueAddedDto {
            done,
            zone,
            first: queued.first,
            count,
            playing_queue: queued.playing_queue,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn item(position: u32, title: &str) -> QueueItemDto {
        QueueItemDto {
            position,
            title: title.into(),
            creator: Some("Sim Ensemble".into()),
            album: None,
            uri: None,
        }
    }

    #[test]
    fn the_text_marks_what_plays_and_counts_what_is_not_shown() {
        let page = QueueDto {
            zone: "Kitchen".into(),
            total: 5,
            revision: None,
            playing: Some(2),
            items: vec![item(1, "I. Allegro"), item(2, "II. Andante")],
        };
        assert_eq!(
            page.text(),
            "Kitchen's queue: 5 track(s), playing 2.\n    \
               1. I. Allegro — Sim Ensemble\n\
             ▶   2. II. Andante — Sim Ensemble\n  … 3 more\n"
        );
        let empty = QueueDto {
            total: 0,
            playing: None,
            items: Vec::new(),
            ..page
        };
        assert_eq!(empty.text(), "Kitchen's queue is empty.\n");
    }
}
