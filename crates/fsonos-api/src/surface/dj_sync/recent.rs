//! Lightweight recent-listening freshness, independent of library publication.

use fastapi::{JsonSchema, fastapi_openapi};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

/// A bounded latest-50 observation. Counts describe one accepted window, not
/// complete Spotify history or a new full-library refresh. Account IDs and
/// individual listening records are deliberately absent from this status.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RecentListeningDto {
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<i64>,
    pub received: usize,
    pub accepted: usize,
    pub skipped: usize,
    pub more_available: bool,
    pub missing_scope: bool,
    pub requires_refresh: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

impl RecentListeningDto {
    /// The window's freshness and limits, without suggesting complete history.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = if self.running {
            "Checking recent Spotify listening for the DJ's repeat avoidance.".into()
        } else if self.missing_scope {
            "Recent listening needs Spotify's user-read-recently-played grant; sign in again to enable it.".into()
        } else if self.requires_refresh {
            "Recent listening needs a full library refresh for the current Spotify account.".into()
        } else if let Some(error) = &self.error {
            format!(
                "Recent listening could not be refreshed: {error}. {}",
                if self.checked_at.is_some() {
                    "The last accepted window retains its original timestamp."
                } else {
                    "No accepted window is available for the current account."
                }
            )
        } else if let Some(at) = self.checked_at {
            format!(
                "Recent listening checked {}: {} history rows, {} distinct track identities accepted, {} unavailable or non-track rows skipped.",
                super::timestamp_text(at),
                self.received,
                self.accepted,
                self.skipped
            )
        } else {
            "Recent listening has not been observed by the background reader yet.".into()
        };
        text.push_str(" Each pass reads at most the latest 50 plays; older listening between checks can be missed.");
        if self.more_available {
            text.push_str(" The latest window may omit older plays.");
        }
        if let Some(at) = self.not_before {
            let _ = write!(
                text,
                " Spotify cooldown ends at {}.",
                super::timestamp_text(at)
            );
        }
        if !self.running
            && let Some(at) = self.retry_at
        {
            let _ = write!(
                text,
                " Next check no earlier than {}.",
                super::timestamp_text(at)
            );
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::dj_sync::LibrarySyncDto;

    #[test]
    fn recent_observation_does_not_fabricate_a_full_library_refresh() {
        let recent = RecentListeningDto {
            checked_at: Some(1_790_000_000),
            received: 50,
            accepted: 48,
            skipped: 1,
            more_available: true,
            ..RecentListeningDto::default()
        };
        let status = LibrarySyncDto::new(false, None, None).with_recent_listening(Some(recent));
        assert!(status.last.is_none());
        assert!(status.done.contains("latest 50"));
        assert!(status.done.contains("48 distinct"));
        let encoded = serde_json::to_value(&status).unwrap();
        assert!(encoded["recent_listening"].get("owner").is_none());
        assert_eq!(
            serde_json::from_value::<LibrarySyncDto>(encoded).unwrap(),
            status
        );
    }

    #[test]
    fn running_and_failed_observations_keep_their_own_status() {
        let running =
            LibrarySyncDto::new(true, None, None).with_recent_listening(Some(RecentListeningDto {
                running: true,
                ..RecentListeningDto::default()
            }));
        assert!(
            running
                .done
                .starts_with("Checking recent Spotify listening")
        );
        let failed = RecentListeningDto {
            error: Some("Spotify unavailable".into()),
            ..RecentListeningDto::default()
        };
        assert!(failed.text().contains("No accepted window is available"));
        let retained = RecentListeningDto {
            checked_at: Some(100),
            ..failed
        };
        assert!(retained.text().contains("retains its original timestamp"));
    }
}
