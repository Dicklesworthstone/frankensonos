//! Refreshing the DJ's library on every surface: `fsonos dj sync`,
//! `GET`/`POST /dj/sync`, and the `dj_sync_status` / `dj_sync` tools.
//!
//! The DJ picks from the library cache in the store: the owner's saved
//! tracks and albums, their artists' genres, and the taste signals the
//! Spotify grant allows. The daemon refreshes it when it starts, if its
//! last refresh is more than a day old (or it never made one), and daily
//! after that. A sync asks for one now, after saving new music, say. It
//! reads Spotify (read-only) in the background and writes only the cache;
//! the DJ plays from what it found from its next pick. A start is logged.
//! It is not undoable: the next sync reads the library afresh anyway.
//! Artist genre enrichment continues in bounded passes of its own. Its
//! current pending work and retry deadlines are separate from the last
//! full refresh, so a saved library does not imply every genre was read.
//! Essential saved-library intake also has durable progress: a bounded pause
//! is unfinished work, and never replaces the last complete public library.
//! Followed-artist directories resume independently, so their request budget
//! does not permanently hide artists beyond the first pages.

pub mod library;
pub mod playlist;

mod genres;
pub use genres::GenreEnrichmentDto;
mod intake;
pub use intake::{SavedLibraryIntakeDto, SavedLibraryIntakePhase};
mod followed;
pub use followed::{FollowedArtistsDto, FollowedArtistsPhase};
mod recent;
pub use recent::RecentListeningDto;

use std::fmt::Write as _;

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use serde::{Deserialize, Serialize};

use super::Surface;
use crate::failure::Failure;

/// Where the library refresh stands: `GET /dj/sync`, and the answer to a
/// start.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct LibrarySyncDto {
    /// Where it stands, in a sentence.
    pub done: String,
    /// A library refresh or enrichment pass is reading Spotify now.
    pub running: bool,
    /// The last full library refresh that completed. Background enrichment
    /// does not change its timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<SyncedDto>,
    /// Why the latest attempt failed, when it did. It is retried at the
    /// daemon's next check (a rate limit resumes where it stopped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Current artist genre progress, separate from the last full refresh.
    /// Absent for older daemons or before progress has been observed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub genre_enrichment: Option<GenreEnrichmentDto>,
    /// Retained private saved-library progress and its retry deadlines. This
    /// can exist before the first completed refresh, and survives restarts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub library_intake: Option<SavedLibraryIntakeDto>,
    /// Current followed-artist directory progress, without private identities
    /// or cursor payloads. Independent of the last completed full refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub followed_artists: Option<FollowedArtistsDto>,
    /// Lightweight listening freshness, independent of full library publication.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub recent_listening: Option<RecentListeningDto>,
}

/// One completed refresh.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SyncedDto {
    /// When it completed (Unix seconds).
    pub at: i64,
    /// Distinct tracks read from Spotify.
    pub tracks: usize,
    /// Of those, the ones the DJ may play (explicit tracks left out).
    pub candidates: usize,
    /// Of the candidates, classical ones (played as whole works).
    pub classical: usize,
    /// Cached tracks no longer in the library, now retired.
    pub retired: usize,
    /// Artist genre lookups left after this pass. Cached successes let the
    /// next pass continue rather than repeat them.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub genre_artists_pending: usize,
    /// Refreshes in a row that left as many artists pending as the one
    /// before (Spotify refusing artist reads, say): each doubles the wait
    /// before the next, so a refusal does not re-read the whole library
    /// every few minutes.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub genre_stalled: usize,
    /// What the refresh couldn't read this time, a sentence each: taste
    /// signals the grant lacks, reads that failed or stopped early, artists
    /// left for the next refresh. It reads them again then. Empty when it
    /// read everything.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's skip_serializing_if passes the field by reference"
)]
fn is_zero(value: &usize) -> bool {
    *value == 0
}

impl LibrarySyncDto {
    /// The state, with its sentence.
    #[must_use]
    pub fn new(running: bool, last: Option<SyncedDto>, error: Option<String>) -> Self {
        let mut state = Self {
            done: String::new(),
            running,
            last,
            error,
            genre_enrichment: None,
            library_intake: None,
            followed_artists: None,
            recent_listening: None,
        };
        state.done = state.text();
        state
    }

    /// Add current, durable artist genre progress without changing the last
    /// full refresh. The sentence distinguishes enrichment from a full read
    /// and uses these current counts instead of an old stamp's pending count.
    #[must_use]
    pub fn with_genre_enrichment(mut self, enrichment: Option<GenreEnrichmentDto>) -> Self {
        self.genre_enrichment = enrichment;
        self.done = self.text();
        self
    }

    /// Add saved-library progress without presenting it as a completed refresh
    /// or treating a normal bounded pause as a failed read.
    #[must_use]
    pub fn with_library_intake(mut self, intake: Option<SavedLibraryIntakeDto>) -> Self {
        self.library_intake = intake;
        self.done = self.text();
        self
    }

    /// Add the followed-artist reader's current progress and retry deadlines.
    #[must_use]
    pub fn with_followed_artists(mut self, artists: Option<FollowedArtistsDto>) -> Self {
        self.followed_artists = artists;
        self.done = self.text();
        self
    }

    /// Add the recent-listening reader's freshness without changing `last`.
    #[must_use]
    pub fn with_recent_listening(mut self, recent: Option<RecentListeningDto>) -> Self {
        self.recent_listening = recent;
        self.done = self.text();
        self
    }

    fn text(&self) -> String {
        if self.running
            && let Some(recent) = &self.recent_listening
            && recent.running
        {
            return recent.text();
        }
        if self.running
            && let Some(genres) = &self.genre_enrichment
            && genres.running
        {
            return genres.text();
        }
        if self.running
            && let Some(intake) = &self.library_intake
            && intake.running
        {
            return intake.text();
        }
        if self.running
            && let Some(artists) = &self.followed_artists
            && artists.running
        {
            return artists.text();
        }
        let pending = self
            .library_intake
            .as_ref()
            .is_some_and(|intake| intake.pending);
        let mut text = match (self.running, &self.last, &self.error) {
            (true, _, _) => {
                "Refreshing the library from Spotify; GET /dj/sync (fsonos dj sync --status) \
                 shows when it is done."
                    .to_owned()
            }
            (false, _, Some(error))
                if self.genre_enrichment.is_some()
                    || self.library_intake.is_some()
                    || self.followed_artists.is_some() =>
            {
                format!("The last library update failed: {error}")
            }
            (false, _, Some(error)) => format!("The last library refresh failed: {error}"),
            (false, _, None) if pending => String::new(),
            (false, Some(last), None) => last.text_with_genres(self.genre_enrichment.is_none()),
            (false, None, None) => "The library has not been refreshed by this daemon yet.".into(),
        };
        if let Some(intake) = &self.library_intake {
            if !text.is_empty() {
                text.push(' ');
            }
            text.push_str(&intake.text());
            if pending
                && !self.running
                && self.error.is_none()
                && let Some(last) = &self.last
            {
                let _ = write!(
                    text,
                    " {}",
                    last.text_with_genres(self.genre_enrichment.is_none())
                );
            }
        }
        if let Some(genres) = &self.genre_enrichment {
            let _ = write!(text, " {}", genres.text());
        }
        if let Some(artists) = &self.followed_artists {
            let _ = write!(text, " {}", artists.text());
        }
        if let Some(recent) = &self.recent_listening {
            let _ = write!(text, " {}", recent.text());
        }
        text
    }
}

fn timestamp_text(at: i64) -> String {
    chrono::DateTime::from_timestamp(at, 0).map_or_else(
        || at.to_string(),
        |time| time.format("%Y-%m-%d %H:%M UTC").to_string(),
    )
}

impl SyncedDto {
    /// `Library refreshed <when>: …`.
    #[must_use]
    pub fn text(&self) -> String {
        self.text_with_genres(true)
    }

    fn text_with_genres(&self, show_pending: bool) -> String {
        let when = timestamp_text(self.at);
        let retired = match self.retired {
            0 => String::new(),
            n => format!(", {n} no longer saved"),
        };
        let pending = match (show_pending, self.genre_artists_pending) {
            (false, _) | (_, 0) => String::new(),
            (true, 1) => {
                ", 1 artist genre lookup pending; continuing at the next daemon check".to_owned()
            }
            (true, n) => {
                format!(", {n} artist genre lookups pending; continuing at the next daemon check")
            }
        };
        let mut text = format!(
            "Library refreshed {when}: {} tracks, {} the DJ can play ({} classical){retired}{pending}.",
            self.tracks, self.candidates, self.classical
        );
        if !self.notes.is_empty() {
            let _ = write!(text, " Not everything was read: {}.", self.notes.join("; "));
        }
        text
    }
}

impl Surface {
    /// Where the library refresh stands (`dj_sync_status`).
    pub fn dj_sync_status(&self, client: &Client) -> Result<LibrarySyncDto, Failure> {
        self.guard(client).authorize("dj_sync_status", true)?;
        self.dj_engine()?.library_sync()
    }

    /// Start refreshing the library from Spotify now, unless a refresh is
    /// running (`dj_sync`), logged.
    pub fn dj_sync(&self, client: &Client) -> Result<LibrarySyncDto, Failure> {
        self.authorize_write(client, "dj_sync")?;
        let result = self.dj_engine()?.sync_library();
        let text = match &result {
            Ok(state) => state.done.clone(),
            Err(f) => format!("failed: {}", f.detail),
        };
        self.record(client, "dj_sync".to_owned(), "allow".to_owned(), text, None);
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn synced(retired: usize) -> SyncedDto {
        SyncedDto {
            at: 1_790_000_000,
            tracks: 1200,
            candidates: 1100,
            classical: 300,
            retired,
            genre_artists_pending: 0,
            genre_stalled: 0,
            notes: Vec::new(),
        }
    }

    #[test]
    fn the_sentence_says_where_it_stands() {
        assert!(
            LibrarySyncDto::new(true, Some(synced(0)), None)
                .done
                .starts_with("Refreshing")
        );
        assert_eq!(
            LibrarySyncDto::new(false, Some(synced(0)), None).done,
            "Library refreshed 2026-09-21 14:13 UTC: 1200 tracks, 1100 the DJ can play (300 \
             classical)."
        );
        assert!(
            synced(4)
                .text()
                .ends_with("(300 classical), 4 no longer saved.")
        );
        assert_eq!(
            LibrarySyncDto::new(false, Some(synced(0)), Some("Spotify is busy".into())).done,
            "The last library refresh failed: Spotify is busy"
        );
        assert!(
            LibrarySyncDto::new(false, None, None)
                .done
                .contains("not been refreshed")
        );
        let partial = SyncedDto {
            notes: vec![
                "the genre read stopped early (rate limited)".into(),
                "2 albums couldn't be read (the first: not found)".into(),
            ],
            ..synced(0)
        };
        assert!(
            partial.text().ends_with(
                "(300 classical). Not everything was read: the genre read stopped early (rate limited); 2 albums couldn't be read (the first: not found)."
            ),
            "{}",
            partial.text()
        );
        // A stamp from before notes reads with none.
        let old: SyncedDto = serde_json::from_str(
            r#"{"at":1790000000,"tracks":1,"candidates":1,"classical":0,"retired":0}"#,
        )
        .unwrap();
        assert!(old.notes.is_empty());
    }

    #[test]
    fn current_genre_progress_round_trips_without_rewriting_the_full_refresh() {
        let last = SyncedDto {
            genre_artists_pending: 99,
            ..synced(0)
        };
        let genres = GenreEnrichmentDto {
            pending: 12,
            completed: 7,
            retry_at: Some(last.at + 900),
            not_before: Some(last.at + 3600),
            ..Default::default()
        };
        let status = LibrarySyncDto::new(false, Some(last.clone()), None)
            .with_genre_enrichment(Some(genres.clone()));
        assert_eq!(status.last, Some(last.clone()));
        assert!(status.done.contains("12 artist genre lookups pending"));
        assert!(
            status
                .done
                .contains("last pass completed 7 artist genre lookups")
        );
        assert!(!status.done.contains("99 artist"), "{}", status.done);
        assert!(status.done.contains("Spotify cooldown"));
        assert!(status.done.ends_with(&format!(
            "The next genre pass is eligible at {}.",
            timestamp_text(last.at + 3600)
        )));
        let wire = serde_json::to_value(&status).unwrap();
        assert_eq!(wire["last"]["at"], last.at);
        assert_eq!(wire["genre_enrichment"]["pending"], 12);
        assert_eq!(wire["genre_enrichment"]["completed"], 7);
        assert_eq!(wire["genre_enrichment"]["not_before"], last.at + 3600);
        assert_eq!(
            serde_json::from_value::<LibrarySyncDto>(wire).unwrap(),
            status
        );
    }

    #[test]
    fn a_running_genre_pass_does_not_claim_another_full_refresh() {
        let status = LibrarySyncDto::new(true, Some(synced(0)), None).with_genre_enrichment(Some(
            GenreEnrichmentDto {
                pending: 8,
                completed: 2,
                running: true,
                ..Default::default()
            },
        ));
        assert!(status.done.starts_with("Enriching artist genres"));
        assert!(!status.done.contains("Library refreshed"));
        assert!(!status.done.contains("eligible"));
        assert_eq!(status.last.as_ref().unwrap().at, synced(0).at);
        assert!(status.running);
    }

    #[test]
    fn genre_completion_and_required_refresh_are_distinct() {
        let last = SyncedDto {
            genre_artists_pending: 99,
            ..synced(0)
        };
        let finished = LibrarySyncDto::new(false, Some(last.clone()), None).with_genre_enrichment(
            Some(GenreEnrichmentDto {
                completed: 4,
                retry_at: Some(last.at + 900),
                ..Default::default()
            }),
        );
        assert!(
            finished
                .done
                .contains("Artist genre enrichment is complete.")
        );
        assert!(!finished.done.contains("pending"), "{}", finished.done);
        assert!(!finished.done.contains("eligible"), "{}", finished.done);

        let refresh = LibrarySyncDto::new(false, Some(last), None).with_genre_enrichment(Some(
            GenreEnrichmentDto {
                requires_refresh: true,
                not_before: Some(1_790_000_900),
                ..Default::default()
            },
        ));
        assert!(refresh.done.contains("needs a full library refresh"));
        assert!(refresh.done.contains("Spotify cooldown"));
        assert!(!refresh.done.contains("enrichment is complete"));
        assert!(!refresh.done.contains("next genre pass is eligible"));
    }

    #[test]
    fn genre_failures_leave_the_completed_full_refresh_available() {
        let status = LibrarySyncDto::new(false, Some(synced(0)), Some("Spotify is busy".into()))
            .with_genre_enrichment(Some(GenreEnrichmentDto {
                pending: 1,
                ..Default::default()
            }));
        assert!(
            status
                .done
                .starts_with("The last library update failed: Spotify is busy")
        );
        assert!(status.done.contains("1 artist genre lookup pending"));
        assert!(status.done.contains("bounded background passes"));
        assert_eq!(status.last, Some(synced(0)));
        assert_eq!(status.error.as_deref(), Some("Spotify is busy"));
    }

    #[test]
    fn older_sync_responses_still_read_but_incomplete_genre_data_is_not_completion() {
        let old = serde_json::json!({"done": "Library refreshed", "running": false});
        let status: LibrarySyncDto = serde_json::from_value(old.clone()).unwrap();
        assert!(status.genre_enrichment.is_none());
        assert_eq!(serde_json::to_value(status).unwrap(), old);

        let mut malformed = old;
        malformed["genre_enrichment"] = serde_json::json!({});
        assert!(serde_json::from_value::<LibrarySyncDto>(malformed).is_err());
    }
}
