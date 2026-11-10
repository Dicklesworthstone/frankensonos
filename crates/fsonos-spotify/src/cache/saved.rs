//! Bounded, durable intake of the essential saved library. These checkpoints
//! are private: neither a paused scan nor an error can publish partial music,
//! retire candidates, adopt an account, or advance the full-sync timestamp.

use crate::SpotifyError;
use crate::library::{LibraryItem, LibraryProgress, SavedLibraryProgress};
use crate::session::Session;
use asupersync::Cx;
use fsonos_core::store::{SpotifyLibraryOwner, Store, StoreError};
use std::time::{SystemTime, UNIX_EPOCH};

/// Page requests per full-library pass, including resumed-page verification.
/// A separate account identity read precedes the pass. Ordinary libraries
/// finish in one pass; larger reads continue from durable private progress.
pub const SAVED_LIBRARY_PAGE_BUDGET: usize = 512;

// Flush on every error and completed pass, and periodically while progressing.
// This avoids rewriting a large metadata snapshot after every individual page.
const CHECKPOINT_EVERY: usize = 16;

pub(super) struct SavedLibrary {
    pub(super) items: Vec<LibraryItem>,
    pub(super) proof: String,
}

/// An early identity/storage error has not observed the checkpoint. A retained
/// report or definite conflict/publication result authoritatively replaces it.
#[derive(Default)]
pub(super) struct Observation {
    pub(super) progress: Option<SavedLibraryProgress>,
    pub(super) observed: bool,
}

impl Observation {
    fn retain(&mut self, progress: SavedLibraryProgress) {
        self.progress = Some(progress);
        self.observed = true;
    }

    pub(super) fn clear(&mut self) {
        self.progress = None;
        self.observed = true;
    }
}

pub(super) fn changed() -> SpotifyError {
    StoreError::Backend(
        "the Spotify account or saved-library read changed during this refresh; refresh again"
            .into(),
    )
    .into()
}

fn now() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |duration| {
            i64::try_from(duration.as_secs()).unwrap_or(i64::MAX)
        })
}

fn save<S: Store + ?Sized>(
    store: &mut S,
    owner: &str,
    base: Option<&SpotifyLibraryOwner>,
    expected: &mut Option<String>,
    progress: &LibraryProgress,
    report: &mut Observation,
) -> Result<(), SpotifyError> {
    let encoded = progress.encode()?;
    if expected.as_deref() == Some(encoded.as_str()) {
        report.retain(progress.summary(base));
        return Ok(());
    }
    if !store.save_spotify_library_progress(owner, base, expected.as_deref(), &encoded)? {
        report.clear();
        return Err(changed());
    }
    *expected = Some(encoded);
    report.retain(progress.summary(base));
    Ok(())
}

pub(super) async fn read<S: Store + ?Sized>(
    session: &mut Session,
    cx: &Cx,
    store: &mut S,
    owner: &str,
    base: Option<&SpotifyLibraryOwner>,
    page_budget: usize,
    report: &mut Observation,
) -> Result<SavedLibrary, SpotifyError> {
    // Two saved collections can need root checks before any resumed work.
    if page_budget < 3 {
        return Err(SpotifyError::Decode(
            "saved-library pass needs at least three page requests".into(),
        ));
    }
    let at = now();
    let mut expected = store.spotify_library_progress(owner, base)?;
    let mut progress = expected
        .as_deref()
        .and_then(|encoded| LibraryProgress::restore(encoded, owner, session.endpoints(), at))
        .unwrap_or_else(|| LibraryProgress::new(owner, session.endpoints(), at));
    progress.claim()?;
    // Claim before HTTP: another full sync cannot consume the same completed
    // evidence while this pass is validating or enriching it.
    save(store, owner, base, &mut expected, &progress, report)?;

    let mut used = 0usize;
    for index in progress.roots() {
        if used == page_budget {
            break;
        }
        let body = session.get_brief(cx, progress.proof_url(index)).await?;
        used += 1;
        if !progress.matches(index, &body)? {
            progress = LibraryProgress::new(owner, session.endpoints(), at);
            progress.claim()?;
            save(store, owner, base, &mut expected, &progress, report)?;
            break;
        }
    }

    while used < page_budget {
        let check = progress.next_check();
        let url = match progress.next_url() {
            Some(url) => url.to_owned(),
            None => match check {
                Some(index) => progress.proof_url(index).to_owned(),
                None => break,
            },
        };
        let body = match session.get_brief(cx, &url).await {
            Ok(body) => body,
            Err(error) => {
                save(store, owner, base, &mut expected, &progress, report)?;
                return Err(error);
            }
        };
        used += 1;
        let accepted = match check {
            Some(index) => progress.matches(index, &body).map(|same| {
                if same {
                    progress.checked();
                }
                same
            }),
            None if progress.collection_changed(&body) => Ok(false),
            None => progress.ingest(&body).map(|()| true),
        };
        match accepted {
            Ok(true) => {}
            Ok(false) => {
                // Changed counts or a same-total middle-page replacement
                // cannot prove a complete library. Start a fresh private read.
                progress = LibraryProgress::new(owner, session.endpoints(), at);
                progress.claim()?;
                save(store, owner, base, &mut expected, &progress, report)?;
                // An inconsistent live collection can return the same changed
                // total indefinitely. Yield after clearing the stale intake;
                // repeatedly restarting here would spend the entire budget
                // rereading the same pages without making progress.
                if check.is_none() {
                    break;
                }
            }
            Err(error) => {
                save(store, owner, base, &mut expected, &progress, report)?;
                return Err(error);
            }
        }
        if used.is_multiple_of(CHECKPOINT_EVERY) {
            save(store, owner, base, &mut expected, &progress, report)?;
        }
    }

    let complete = progress.finish();
    save(store, owner, base, &mut expected, &progress, report)?;
    if !complete {
        return Err(SpotifyError::LibraryPending {
            pages: progress.pages(),
            tracks: progress.tracks(),
        });
    }
    Ok(SavedLibrary {
        items: progress.into_items(),
        proof: expected.ok_or_else(changed)?,
    })
}
