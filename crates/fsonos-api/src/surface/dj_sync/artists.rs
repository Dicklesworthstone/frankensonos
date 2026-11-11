//! Bounded artist-sample discovery, separate from followed-directory intake.

use fastapi::{JsonSchema, fastapi_openapi};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::timestamp_text;

/// Current progress through the finite artist cohort admitted by a full read.
/// Private artist identities and account receipts stay in the daemon store.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ArtistSamplingDto {
    /// Artists whose playable sample is still unfinished in this cohort.
    pub pending: usize,
    /// Successful samples in this full-refresh cohort, not a lifetime total.
    pub completed: usize,
    /// Artists attempted in the latest bounded pass, including temporary failures.
    pub attempted: usize,
    /// The private cohort reached its bound; automatic reads are paused.
    pub limited: bool,
    /// This worker is performing an artist-sample pass now.
    pub running: bool,
    /// Earliest next sample pass after progress backoff, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<i64>,
    /// Spotify's shared read cooldown, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<i64>,
    /// A full refresh must establish the current account and cohort first.
    pub requires_refresh: bool,
}

impl ArtistSamplingDto {
    /// Describe current discovery without advancing the full-sync timestamp.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = if self.requires_refresh {
            "Artist sampling needs a full library refresh before it can continue.".into()
        } else if self.limited {
            "Artist sampling reached its cache limit; automatic sample reads are paused.".into()
        } else if self.running {
            format!(
                "Discovering artist samples in the background: {} pending.",
                self.pending
            )
        } else if self.pending == 0 {
            "Artist sampling is complete for the current cohort.".into()
        } else {
            format!(
                "Artist sampling is incomplete: {} artist sample(s) pending.",
                self.pending
            )
        };
        if self.completed > 0 {
            let _ = write!(
                text,
                " This cohort has completed {} artist sample(s).",
                self.completed
            );
        }
        if self.attempted > 0 {
            let _ = write!(
                text,
                " The last pass attempted {} artist sample(s).",
                self.attempted
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
            if self.pending > 0 && !self.requires_refresh && !self.limited {
                if let Some(at) = self.retry_at.into_iter().chain(self.not_before).max() {
                    let _ = write!(
                        text,
                        " The next artist-sample pass is eligible at {}.",
                        timestamp_text(at)
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

    fn pending() -> ArtistSamplingDto {
        ArtistSamplingDto {
            pending: 70,
            completed: 50,
            attempted: 50,
            retry_at: Some(1_790_000_900),
            not_before: Some(1_790_003_600),
            ..Default::default()
        }
    }

    fn full() -> SyncedDto {
        SyncedDto {
            at: 1_790_000_000,
            tracks: 150,
            candidates: 148,
            classical: 2,
            retired: 4,
            genre_artists_pending: 0,
            genre_stalled: 0,
            notes: Vec::new(),
        }
    }

    #[test]
    fn progress_counts_the_cohort_and_preserves_the_completed_full_refresh() {
        let status = LibrarySyncDto::new(false, Some(full()), None)
            .with_artist_sampling(Some(pending()));
        assert_eq!(status.last, Some(full()));
        assert!(status.done.contains("70 artist sample(s) pending"));
        assert!(status.done.contains("cohort has completed 50 artist sample(s)"));
        assert!(status.done.contains("last pass attempted 50"));
        assert!(status.done.ends_with(&format!(
            "The next artist-sample pass is eligible at {}.",
            timestamp_text(1_790_003_600)
        )));
        let wire = serde_json::to_value(&status).unwrap();
        assert_eq!(wire["last"]["at"], full().at);
        assert_eq!(wire["artist_sampling"]["completed"], 50);
        assert!(wire["artist_sampling"].get("owner").is_none());
        assert!(wire["artist_sampling"].get("generation").is_none());
        assert!(wire["artist_sampling"].get("cycle").is_none());
        assert_eq!(
            serde_json::from_value::<LibrarySyncDto>(wire).unwrap(),
            status
        );
    }

    #[test]
    fn active_sampling_and_real_failures_do_not_replace_other_status_or_full_freshness() {
        let status = LibrarySyncDto::new(true, Some(full()), None)
            .with_genre_enrichment(Some(GenreEnrichmentDto {
                pending: 5,
                ..Default::default()
            }))
            .with_artist_sampling(Some(ArtistSamplingDto {
                running: true,
                ..pending()
            }));
        assert!(status.done.starts_with("Discovering artist samples"));
        assert!(!status.done.contains("Library refreshed"));
        assert_eq!(status.last, Some(full()));
        assert_eq!(status.genre_enrichment.unwrap().pending, 5);
        let failed = LibrarySyncDto::new(
            false,
            Some(full()),
            Some("Spotify is unavailable".into()),
        )
        .with_artist_sampling(Some(pending()));
        assert!(failed.done.starts_with("The last library update failed"));
        assert!(failed.done.contains("70 artist sample(s) pending"));
        assert_eq!(failed.last, Some(full()));
    }

    #[test]
    fn completion_cache_limits_and_required_full_refresh_are_distinct() {
        let complete = ArtistSamplingDto {
            pending: 0,
            completed: 120,
            attempted: 20,
            retry_at: None,
            not_before: None,
            ..Default::default()
        };
        assert!(complete.text().contains("complete for the current cohort"));
        assert!(!complete.text().contains("next artist-sample pass"));
        for status in [
            ArtistSamplingDto { limited: true, ..pending() },
            ArtistSamplingDto { requires_refresh: true, ..pending() },
        ] {
            assert!(!status.text().contains("complete for the current cohort"));
            assert!(!status.text().contains("next artist-sample pass"));
            assert!(!status.text().contains("will continue"));
        }
    }

    #[test]
    fn absent_sampling_is_backward_compatible_and_partial_progress_is_not_completion() {
        let old = serde_json::json!({"done":"Not read yet", "running":false});
        let parsed: LibrarySyncDto = serde_json::from_value(old.clone()).unwrap();
        assert!(parsed.artist_sampling.is_none());
        assert_eq!(serde_json::to_value(parsed).unwrap(), old);
        for invalid in [
            serde_json::json!({}),
            serde_json::json!({
                "pending":-1, "completed":50, "attempted":50,
                "running":false, "limited":false, "requires_refresh":false
            }),
        ] {
            let mut value = old.clone();
            value["artist_sampling"] = invalid;
            assert!(serde_json::from_value::<LibrarySyncDto>(value).is_err());
        }
    }
}
