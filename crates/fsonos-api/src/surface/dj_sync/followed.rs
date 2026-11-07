//! Followed-artist directory progress, independent of published music.

use fastapi::{JsonSchema, fastapi_openapi};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::timestamp_text;

/// A verified directory can still await its full library reconciliation.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum FollowedArtistsPhase {
    #[default]
    Reading,
    Verifying,
    Ready,
}

/// A compact observation of the bounded followed-artist reader. The private
/// artist metadata, account identity, cursor and ownership receipt stay local.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[allow(clippy::struct_excessive_bools)] // Independent wire flags for waiting, bounds, activity and ownership.
pub struct FollowedArtistsDto {
    /// The directory still needs reading, verification or full reconciliation.
    /// A budget pause is normal progress; ready pages are not publication.
    pub pending: bool,
    pub phase: FollowedArtistsPhase,
    /// Validated pages retained in the current directory scan.
    pub pages: usize,
    /// Distinct artists retained in the current directory scan.
    pub artists: usize,
    /// Pages validated for reconciliation. A scan completed in one pass counts
    /// its fresh pages directly; a resumed scan rechecks its retained pages.
    pub verified_pages: usize,
    /// The private directory reached a cache bound. It remains incomplete,
    /// but no further automatic page reads are scheduled for this scan.
    pub limited: bool,
    /// The current library worker is performing a followed-artist pass.
    pub running: bool,
    /// Earliest automatic continuation after retry backoff, in Unix seconds.
    /// A later `not_before` deadline takes precedence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<i64>,
    /// Spotify's saved read cooldown deadline, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<i64>,
    /// A full library refresh is required before this reader can continue.
    pub requires_refresh: bool,
}

impl FollowedArtistsDto {
    /// Describe directory progress without claiming a new published library.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = if self.requires_refresh {
            "Followed-artist intake needs a full library refresh before it can continue."
        } else if self.limited {
            "Followed-artist intake reached its cache limit; the retained directory is incomplete."
        } else if self.pending && self.phase == FollowedArtistsPhase::Ready {
            "Followed artists are retained and verified; a full library refresh must reconcile them."
        } else if self.running {
            match self.phase {
                FollowedArtistsPhase::Reading => "Reading followed artists in a bounded background pass.",
                FollowedArtistsPhase::Verifying => "Verifying retained followed-artist pages in a bounded background pass.",
                FollowedArtistsPhase::Ready => "Finishing the followed-artist directory read.",
            }
        } else if self.pending {
            if self.phase == FollowedArtistsPhase::Verifying {
                "Followed-artist verification is unfinished; its progress is retained."
            } else {
                "Followed-artist intake is unfinished; its progress is retained."
            }
        } else {
            "Followed-artist directory intake is complete."
        }
        .to_owned();
        let _ = write!(
            text,
            " Retained {} validated page(s) and {} artist(s).",
            self.pages, self.artists
        );
        if self.verified_pages > 0 || self.phase == FollowedArtistsPhase::Verifying {
            let _ = write!(
                text,
                " Validated {} page(s) for reconciliation.",
                self.verified_pages
            );
        }
        if !self.running {
            if let Some(at) = self.not_before {
                let _ = write!(
                    text,
                    " Spotify cooldown: no reads before {}.",
                    timestamp_text(at)
                );
            }
            if self.pending && !self.requires_refresh && !self.limited {
                if let Some(at) = self.retry_at.into_iter().chain(self.not_before).max() {
                    let pass = if self.phase == FollowedArtistsPhase::Ready {
                        "library reconciliation"
                    } else {
                        "followed-artist pass"
                    };
                    let _ = write!(
                        text,
                        " The next {pass} is eligible at {}.",
                        timestamp_text(at)
                    );
                } else if self.phase == FollowedArtistsPhase::Ready {
                    text.push_str(
                        " The daemon will reconcile this directory in a full library refresh.",
                    );
                } else {
                    text.push_str(" The daemon will continue in bounded background passes.");
                }
            }
        }
        text
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::surface::dj_sync::{GenreEnrichmentDto, LibrarySyncDto, SyncedDto};

    fn waiting() -> FollowedArtistsDto {
        FollowedArtistsDto {
            pending: true,
            pages: 20,
            artists: 1_000,
            retry_at: Some(1_790_000_900),
            not_before: Some(1_790_003_600),
            ..Default::default()
        }
    }

    fn last() -> SyncedDto {
        SyncedDto {
            at: 1_790_000_000,
            tracks: 125,
            candidates: 120,
            classical: 10,
            retired: 3,
            genre_artists_pending: 99,
            genre_stalled: 0,
            notes: Vec::new(),
        }
    }

    #[test]
    fn directory_progress_keeps_the_last_full_refresh_and_uses_the_later_deadline() {
        let previous = last();
        let status = LibrarySyncDto::new(false, Some(previous.clone()), None)
            .with_followed_artists(Some(waiting()));
        assert_eq!(status.last, Some(previous));
        assert!(status.done.contains("Followed-artist intake is unfinished"));
        assert!(
            status
                .done
                .contains("20 validated page(s) and 1000 artist(s)")
        );
        assert!(!status.done.contains("failed"));
        assert!(status.done.ends_with(&format!(
            "The next followed-artist pass is eligible at {}.",
            timestamp_text(1_790_003_600)
        )));
        let wire = serde_json::to_value(&status).unwrap();
        assert_eq!(wire["last"]["at"], last().at);
        assert_eq!(wire["followed_artists"]["artists"], 1_000);
        assert_eq!(
            serde_json::from_value::<LibrarySyncDto>(wire).unwrap(),
            status
        );
    }

    #[test]
    fn a_running_directory_pass_does_not_report_a_new_full_refresh() {
        let status = LibrarySyncDto::new(true, Some(last()), None)
            .with_genre_enrichment(Some(GenreEnrichmentDto {
                pending: 12,
                ..Default::default()
            }))
            .with_followed_artists(Some(FollowedArtistsDto {
                running: true,
                ..waiting()
            }));
        assert!(status.done.starts_with("Reading followed artists"));
        assert!(!status.done.contains("Library refreshed"));
        assert!(!status.done.contains("eligible"));
        assert_eq!(status.last.as_ref().unwrap().at, last().at);
        assert_eq!(status.genre_enrichment.as_ref().unwrap().pending, 12);
    }

    #[test]
    fn completion_and_required_refresh_are_distinct() {
        let finished = FollowedArtistsDto {
            pending: false,
            phase: FollowedArtistsPhase::Ready,
            verified_pages: 20,
            ..waiting()
        };
        assert!(finished.text().contains("directory intake is complete"));
        assert!(!finished.text().contains("next followed-artist pass"));
        let invalidated = FollowedArtistsDto {
            requires_refresh: true,
            ..finished
        };
        assert!(invalidated.text().contains("needs a full library refresh"));
        assert!(!invalidated.text().contains("intake is complete"));
        assert!(!invalidated.text().contains("next followed-artist pass"));
    }

    #[test]
    fn verification_and_ready_directory_still_wait_for_reconciliation() {
        let verifying = FollowedArtistsDto {
            phase: FollowedArtistsPhase::Verifying,
            verified_pages: 8,
            ..waiting()
        };
        assert!(verifying.text().contains("verification is unfinished"));
        assert!(
            verifying
                .text()
                .contains("Validated 8 page(s) for reconciliation")
        );
        let ready = FollowedArtistsDto {
            phase: FollowedArtistsPhase::Ready,
            verified_pages: 20,
            ..verifying
        };
        assert!(
            ready
                .text()
                .contains("a full library refresh must reconcile them")
        );
        assert!(
            ready
                .text()
                .contains("next library reconciliation is eligible")
        );
        assert!(!ready.text().contains("intake is complete"));
        assert!(!ready.text().contains("next followed-artist pass"));
    }

    #[test]
    fn a_cache_bound_is_incomplete_without_promising_automatic_progress() {
        let limited = FollowedArtistsDto {
            pending: false,
            limited: true,
            ..waiting()
        };
        assert!(limited.text().contains("reached its cache limit"));
        assert!(!limited.text().contains("intake is complete"));
        assert!(!limited.text().contains("is eligible"));
        assert!(!limited.text().contains("will continue"));
    }

    #[test]
    fn real_failures_and_current_genre_progress_remain_visible() {
        let status =
            LibrarySyncDto::new(false, Some(last()), Some("Spotify is unavailable".into()))
                .with_genre_enrichment(Some(GenreEnrichmentDto {
                    pending: 12,
                    ..Default::default()
                }))
                .with_followed_artists(Some(waiting()));
        assert!(
            status
                .done
                .starts_with("The last library update failed: Spotify is unavailable")
        );
        assert!(status.done.contains("12 artist genre lookups pending"));
        assert!(!status.done.contains("99 artist"));
        assert!(status.done.contains("Followed-artist intake is unfinished"));
        assert_eq!(status.last, Some(last()));
    }

    #[test]
    fn old_responses_read_but_partial_directory_data_does_not_claim_completion() {
        let old = serde_json::json!({"done": "Not read", "running": false});
        let status: LibrarySyncDto = serde_json::from_value(old.clone()).unwrap();
        assert!(status.followed_artists.is_none());
        assert_eq!(serde_json::to_value(status).unwrap(), old);
        for invalid in [
            serde_json::json!({}),
            serde_json::json!({
                "pending": true, "pages": -1, "artists": 50,
                "running": false, "requires_refresh": false
            }),
        ] {
            let mut malformed = old.clone();
            malformed["followed_artists"] = invalid;
            assert!(serde_json::from_value::<LibrarySyncDto>(malformed).is_err());
        }
    }
}
