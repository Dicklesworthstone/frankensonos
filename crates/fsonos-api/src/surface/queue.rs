//! The queue a room's group plays from, read-only (`get_queue`,
//! `GET /zones/{room}/queue`, `fsonos queue`): its tracks in order, a page at
//! a time, how many there are, and which one plays.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use fsonos_core::{CoreError, control};
use fsonos_proto::content::{self, BrowseFlag, QUEUE};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::{Surface, resolve, room_view};
use crate::failure::Failure;

/// The tool name the queue read is authorized as.
pub const TOOL: &str = "get_queue";

/// The most items one page carries (Sonos pages at 100).
pub const MAX_PAGE: u32 = 100;

/// A page of a group's queue.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct QueueDto {
    /// The group, named by its coordinator's room.
    pub zone: String,
    /// How many tracks the queue holds.
    pub total: u32,
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
        let households = self.households()?;
        let aliases = self.aliases();
        let target = resolve(room_view(&households, aliases.as_ref(), client), zone)
            .map_err(|f| self.explain(f))?;
        let coordinator = target.coordinator;
        let page = content::browse(
            &*self.transport,
            coordinator.ip,
            QUEUE,
            BrowseFlag::DirectChildren,
            offset,
            limit.clamp(1, MAX_PAGE),
        )
        .map_err(|e| Failure::from(CoreError::from(e)))
        .inspect_err(|f| self.notice(f))?;
        // The position the group plays: 0 when it isn't playing its queue.
        let playing = control::playback(&*self.transport, &households, &coordinator.id)
            .map_err(Failure::from)
            .inspect_err(|f| self.notice(f))?
            .position
            .track;
        let items = (offset.saturating_add(1)..)
            .zip(page.objects)
            .map(|(position, o)| QueueItemDto {
                position,
                title: o.title,
                creator: o.creator,
                album: o.album,
                uri: o.res.map(|r| r.uri),
            })
            .collect();
        Ok(QueueDto {
            zone: coordinator.room_name.clone(),
            total: page.total_matches,
            playing: (playing > 0 && playing <= page.total_matches).then_some(playing),
            items,
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
