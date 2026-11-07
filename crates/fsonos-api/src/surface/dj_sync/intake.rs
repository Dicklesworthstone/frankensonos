//! Durable saved-library intake, before a full refresh can be published.

use fastapi::{JsonSchema, fastapi_openapi};
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::timestamp_text;

/// What the retained saved-library read must finish. Ready pages may still
/// need enrichment or publication; they are not an accepted public library.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SavedLibraryIntakePhase {
    #[default]
    Reading,
    Verifying,
    Ready,
}

/// A compact observation of private progress, without exposing the owner's
/// identity, tokens, saved metadata, or the checkpoint's publication receipt.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SavedLibraryIntakeDto {
    /// A full refresh is still unfinished; a bounded pass can be idle while
    /// waiting for its retry deadline. Partial work is never the public library.
    pub pending: bool,
    pub phase: SavedLibraryIntakePhase,
    /// Validated pages retained in the current private read, not a total to fetch.
    pub pages: usize,
    /// Track rows retained across saved collections, before final deduplication.
    pub tracks: usize,
    /// Retained pages rechecked during this read's verification stage.
    pub verified_pages: usize,
    /// The library worker is performing a full refresh now.
    pub running: bool,
    /// Earliest automatic continuation after retry backoff, in Unix seconds.
    /// A later `not_before` deadline takes precedence.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_at: Option<i64>,
    /// Spotify's saved read cooldown deadline, in Unix seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub not_before: Option<i64>,
}

impl SavedLibraryIntakeDto {
    /// Describe retained progress without calling a normal bounded pause a failure.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = if self.running {
            match self.phase {
                SavedLibraryIntakePhase::Reading => {
                    "Reading the saved Spotify library in a bounded pass."
                }
                SavedLibraryIntakePhase::Verifying => {
                    "Verifying retained saved-library pages before publication."
                }
                SavedLibraryIntakePhase::Ready => {
                    "Finishing the full library refresh from retained saved-library pages."
                }
            }
        } else if self.pending {
            "Saved-library intake is unfinished; its progress is retained."
        } else {
            "Saved-library intake is complete."
        }
        .to_owned();
        let _ = write!(
            text,
            " Retained {} validated page(s) and {} track row(s).",
            self.pages, self.tracks
        );
        if self.verified_pages > 0 || self.phase == SavedLibraryIntakePhase::Verifying {
            let _ = write!(text, " Rechecked {} retained page(s).", self.verified_pages);
        }
        if self.pending {
            text.push_str(" This saved music has not been published yet.");
        }
        if !self.running {
            if let Some(at) = self.not_before {
                let _ = write!(
                    text,
                    " Spotify cooldown: no reads before {}.",
                    timestamp_text(at)
                );
            }
            if self.pending {
                if let Some(at) = self.retry_at.into_iter().chain(self.not_before).max() {
                    let _ = write!(
                        text,
                        " The next library pass is eligible at {}.",
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

    fn waiting() -> SavedLibraryIntakeDto {
        SavedLibraryIntakeDto {
            pending: true,
            phase: SavedLibraryIntakePhase::Verifying,
            pages: 512,
            tracks: 24_500,
            verified_pages: 96,
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
    fn a_bounded_pause_before_first_publication_is_progress_not_failure() {
        let status = LibrarySyncDto::new(false, None, None).with_library_intake(Some(waiting()));
        assert!(
            status
                .done
                .starts_with("Saved-library intake is unfinished")
        );
        assert!(
            status
                .done
                .contains("512 validated page(s) and 24500 track row(s)")
        );
        assert!(status.done.contains("Rechecked 96 retained page(s)"));
        assert!(status.done.contains("has not been published yet"));
        assert!(!status.done.contains("failed"));
        assert!(status.done.ends_with(&format!(
            "The next library pass is eligible at {}.",
            timestamp_text(1_790_003_600)
        )));
        let wire = serde_json::to_value(&status).unwrap();
        assert!(wire.get("last").is_none());
        assert!(wire.get("error").is_none());
        assert_eq!(wire["library_intake"]["phase"], "verifying");
        assert_eq!(
            serde_json::from_value::<LibrarySyncDto>(wire).unwrap(),
            status
        );
    }

    #[test]
    fn retained_intake_does_not_replace_full_refresh_history_or_genre_progress() {
        let previous = last();
        let status = LibrarySyncDto::new(false, Some(previous.clone()), None)
            .with_genre_enrichment(Some(GenreEnrichmentDto {
                pending: 12,
                ..Default::default()
            }))
            .with_library_intake(Some(waiting()));
        assert_eq!(status.last, Some(previous));
        assert!(
            status
                .done
                .starts_with("Saved-library intake is unfinished")
        );
        assert!(status.done.contains("125 tracks"));
        assert!(status.done.contains("12 artist genre lookups pending"));
        assert!(!status.done.contains("99 artist"));
        let without_intake = status.with_library_intake(None);
        assert!(without_intake.done.starts_with("Library refreshed"));
        assert!(
            serde_json::to_value(without_intake)
                .unwrap()
                .get("library_intake")
                .is_none()
        );
    }

    #[test]
    fn a_running_verification_reports_its_stage_without_inventing_completion() {
        let status = LibrarySyncDto::new(true, Some(last()), None).with_library_intake(Some(
            SavedLibraryIntakeDto {
                running: true,
                ..waiting()
            },
        ));
        assert!(
            status
                .done
                .starts_with("Verifying retained saved-library pages")
        );
        assert!(status.done.contains("Rechecked 96 retained page(s)"));
        assert!(!status.done.contains("eligible"));
        assert!(!status.done.contains("Library refreshed"));
        assert_eq!(status.last.as_ref().unwrap().at, last().at);
    }

    #[test]
    fn ready_pages_are_still_pending_until_the_full_refresh_is_published() {
        let ready = SavedLibraryIntakeDto {
            phase: SavedLibraryIntakePhase::Ready,
            verified_pages: 512,
            ..waiting()
        };
        let status = LibrarySyncDto::new(false, None, None).with_library_intake(Some(ready));
        assert!(!status.done.contains("intake is complete"));
        assert!(status.done.contains("has not been published yet"));
        let finished = LibrarySyncDto::new(false, Some(last()), None).with_library_intake(Some(
            SavedLibraryIntakeDto {
                pending: false,
                phase: SavedLibraryIntakePhase::Ready,
                ..Default::default()
            },
        ));
        assert!(finished.done.contains("Saved-library intake is complete"));
        assert!(!finished.done.contains("next library pass"));
    }

    #[test]
    fn a_real_failure_is_not_hidden_by_retained_progress() {
        let status =
            LibrarySyncDto::new(false, Some(last()), Some("Spotify is unavailable".into()))
                .with_library_intake(Some(waiting()));
        assert!(
            status
                .done
                .starts_with("The last library update failed: Spotify is unavailable")
        );
        assert!(status.done.contains("Saved-library intake is unfinished"));
        assert_eq!(status.error.as_deref(), Some("Spotify is unavailable"));
        assert_eq!(status.last, Some(last()));
    }

    #[test]
    fn older_responses_still_read_but_partial_progress_shapes_do_not_claim_completion() {
        let old = serde_json::json!({"done": "Not read", "running": false});
        let status: LibrarySyncDto = serde_json::from_value(old.clone()).unwrap();
        assert!(status.library_intake.is_none());
        assert_eq!(serde_json::to_value(status).unwrap(), old);
        for invalid in [
            serde_json::json!({}),
            serde_json::json!({
                "pending": true, "running": false, "phase": "published",
                "pages": 1, "tracks": 50, "verified_pages": 0
            }),
        ] {
            let mut malformed = old.clone();
            malformed["library_intake"] = invalid;
            assert!(serde_json::from_value::<LibrarySyncDto>(malformed).is_err());
        }
    }
}
