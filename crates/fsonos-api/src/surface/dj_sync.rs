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
    /// A refresh is reading Spotify now.
    pub running: bool,
    /// The last refresh that completed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last: Option<SyncedDto>,
    /// Why the latest attempt failed, when it did. It is retried at the
    /// daemon's next check (a rate limit resumes where it stopped).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
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
}

impl LibrarySyncDto {
    /// The state, with its sentence.
    #[must_use]
    pub fn new(running: bool, last: Option<SyncedDto>, error: Option<String>) -> Self {
        let done = match (running, &last, &error) {
            (true, _, _) => {
                "Refreshing the library from Spotify; GET /dj/sync (fsonos dj sync) shows when \
                 it is done."
                    .to_owned()
            }
            (false, _, Some(error)) => format!("The last library refresh failed: {error}"),
            (false, Some(last), None) => last.text(),
            (false, None, None) => "The library has not been refreshed by this daemon yet.".into(),
        };
        Self {
            done,
            running,
            last,
            error,
        }
    }
}

impl SyncedDto {
    /// `Library refreshed <when>: …`.
    #[must_use]
    pub fn text(&self) -> String {
        let when = chrono::DateTime::from_timestamp(self.at, 0).map_or_else(
            || self.at.to_string(),
            |t| t.format("%Y-%m-%d %H:%M UTC").to_string(),
        );
        let retired = match self.retired {
            0 => String::new(),
            n => format!(", {n} no longer saved"),
        };
        format!(
            "Library refreshed {when}: {} tracks, {} the DJ can play ({} classical){retired}.",
            self.tracks, self.candidates, self.classical
        )
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
    }
}
