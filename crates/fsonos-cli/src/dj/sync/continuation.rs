//! Durable scheduling for bounded playlist and whole-work album enrichment. The public stamp's
//! timestamp remains the last full sync, while private state survives restarts.

use super::{
    CHECK, ErrorCode, Failure, LibrarySync, LibrarySyncDetailed, MAX_AGE, Path, STAMP, SyncedDto,
    failure, stalled,
};
use fsonos_spotify::cache::AlbumContinuation;
use fsonos_spotify::taste::PlaylistContinuation;
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;

const NOTE: &str = "Playlist continuation: ";
const ALBUM_NOTE: &str = "Album completion: ";

pub(super) enum Pass {
    Full,
    Playlists(String),
    Albums(Option<AlbumContinuation>),
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(super) struct Checkpoint {
    owner: Option<String>,
    pending: usize,
    limited: usize,
    stalled: usize,
    pub(super) retry_at: i64,
    pub(super) not_before: i64,
    pub(super) requires_refresh: bool,
    /// Completed background intake needs one authoritative full refresh to
    /// retire taste tracks that no current playlist or other signal supports.
    #[serde(default)]
    pub(super) reconcile: bool,
    /// Absent in old stamps: discover unfinished albums at the next check.
    #[serde(default)]
    albums: Option<AlbumContinuation>,
    #[serde(default)]
    album_stalled: usize,
    #[serde(default)]
    album_retry_at: i64,
    #[serde(default)]
    album_turn: bool,
}

impl Checkpoint {
    fn wait(stalled: usize) -> i64 {
        let check = i64::try_from(CHECK.as_secs()).unwrap_or(i64::MAX);
        let max = i64::try_from(MAX_AGE.as_secs()).unwrap_or(i64::MAX);
        let shift = u32::try_from(stalled.min(16)).unwrap_or(16);
        check.saturating_mul(1_i64 << shift).min(max)
    }

    fn cooldown(&mut self, now: i64, seconds: u64) {
        let until = now.saturating_add(i64::try_from(seconds).unwrap_or(i64::MAX));
        self.not_before = self.not_before.max(until);
        self.retry_at = self.retry_at.max(self.not_before);
        self.album_retry_at = self.album_retry_at.max(self.not_before);
    }

    fn reserve(&self, now: i64) -> Self {
        let mut reserved = self.clone();
        reserved.stalled = reserved.stalled.saturating_add(1);
        reserved.retry_at = now.saturating_add(Self::wait(reserved.stalled));
        reserved.retry_at = reserved.retry_at.max(reserved.not_before);
        reserved
    }

    fn observed(&self, report: &PlaylistContinuation, now: i64) -> Self {
        let progress =
            report.advanced > 0 || report.pending < self.pending || report.owner != self.owner;
        let stalled = if progress || report.pending == 0 {
            0
        } else {
            self.stalled.saturating_add(1)
        };
        let mut next = Self {
            owner: report.owner.clone(),
            pending: report.pending,
            limited: report.limited,
            stalled,
            retry_at: now.saturating_add(Self::wait(stalled)),
            not_before: self.not_before,
            requires_refresh: report.requires_refresh,
            reconcile: false,
            albums: self.albums.clone(),
            album_stalled: self.album_stalled,
            album_retry_at: if self.albums.is_none() {
                now.saturating_add(Self::wait(0))
            } else {
                self.album_retry_at
            },
            album_turn: self.album_turn,
        };
        next.cooldown(now, report.retry_after_secs);
        next
    }

    /// A completed playlist-only pass is additive. Once all pending snapshots
    /// are proved, schedule one full read to reconcile their authoritative
    /// union with saved music and the other taste signals. The intent survives
    /// restart and failed full reads; a successful full pass consumes it.
    fn completed(&self, report: &PlaylistContinuation, now: i64, continuation: bool) -> Self {
        let mut next = self.observed(report, now);
        next.reconcile = continuation
            && !next.requires_refresh
            && next.pending == 0
            && next.limited == 0
            && next.owner.is_some()
            && next.owner == self.owner
            && (self.pending > 0 || self.reconcile);
        next
    }

    fn owner_due(&self, now: i64) -> Option<String> {
        if self.pending == 0
            || self.requires_refresh
            || self.reconcile
            || now < self.retry_at
            || now < self.not_before
        {
            return None;
        }
        self.owner.clone().filter(|owner| {
            !owner.trim().is_empty() && owner.len() <= 1_024 && !owner.chars().any(char::is_control)
        })
    }

    fn album_due(&self, now: i64) -> bool {
        !self.requires_refresh
            && !self.reconcile
            && now >= self.not_before
            && now >= self.album_retry_at
            && self.albums.as_ref().is_none_or(|albums| albums.pending > 0)
    }

    fn reserve_albums(&self, now: i64) -> Self {
        let mut reserved = self.clone();
        reserved.album_stalled = reserved.album_stalled.saturating_add(1);
        reserved.album_retry_at = now
            .saturating_add(Self::wait(reserved.album_stalled))
            .max(reserved.not_before);
        reserved
    }

    fn observed_albums(&mut self, report: &AlbumContinuation, now: i64, keep_pages: bool) {
        if keep_pages
            && report.completed == 0
            && !report.requires_refresh
            && let Some(previous) = &self.albums
            && previous.pending > 0
            && report.pending > 0
            && previous.owner == report.owner
            && previous.generation == report.generation
        {
            let mut albums = report.clone();
            albums.inherit_progress(previous);
            self.albums = Some(albums);
            // A library observation must not keep postponing an album pass
            // that is already due, or count as another failed album attempt.
            self.cooldown(now, report.retry_after_secs);
            return;
        }
        let progress = report.completed > 0
            || report.advanced > 0
            || self.albums.as_ref().is_none_or(|previous| {
                report.pending < previous.pending
                    || report.owner != previous.owner
                    || report.generation != previous.generation
            });
        self.album_stalled = if progress || report.pending == 0 {
            0
        } else {
            self.album_stalled.saturating_add(1)
        };
        self.album_retry_at = now.saturating_add(Self::wait(self.album_stalled));
        let mut albums = report.clone();
        if keep_pages && let Some(previous) = &self.albums {
            albums.inherit_progress(previous);
        }
        if report.requires_refresh {
            self.requires_refresh = true;
            self.retry_at = self.album_retry_at;
        }
        self.albums = Some(albums);
        self.cooldown(now, report.retry_after_secs);
    }

    fn album_note(&self) -> Option<String> {
        let albums = self.albums.as_ref()?;
        if albums.requires_refresh {
            return Some(format!(
                "{ALBUM_NOTE}the Spotify account needs a full library refresh"
            ));
        }
        (albums.pending > 0 || albums.completed > 0).then(|| {
            format!(
                "{ALBUM_NOTE}{} album list(s) completed, {} unfinished; the next bounded pass follows the saved retry deadline",
                albums.completed, albums.pending
            )
        })
    }

    fn background_pass(&self, now: i64) -> Option<Pass> {
        let owner = self.owner_due(now);
        let albums = self.album_due(now);
        if albums && (self.album_turn || owner.is_none()) {
            Some(Pass::Albums(self.albums.clone()))
        } else {
            owner.map(Pass::Playlists)
        }
    }

    fn note(&self) -> String {
        if self.requires_refresh {
            return format!("{NOTE}the Spotify account changed; a full refresh will run next");
        }
        if self.reconcile {
            return format!(
                "{NOTE}background intake is complete; a full refresh will reconcile the library next"
            );
        }
        format!(
            "{NOTE}{} unfinished playlist(s), {} at a cache limit; {}",
            self.pending,
            self.limited,
            if self.pending == 0 {
                "background intake is complete"
            } else {
                "the next bounded pass follows the saved retry deadline"
            }
        )
    }
}

#[derive(Default, Deserialize)]
struct Envelope {
    #[serde(default)]
    playlist_continuation: Option<Checkpoint>,
}

pub(super) fn load(data_dir: &Path) -> Option<Checkpoint> {
    let text = std::fs::read_to_string(data_dir.join(STAMP)).ok()?;
    serde_json::from_str::<Envelope>(&text)
        .ok()?
        .playlist_continuation
}

fn write(
    data_dir: &Path,
    synced: Option<&SyncedDto>,
    checkpoint: &Checkpoint,
) -> std::io::Result<()> {
    // The additional field is ignored by existing SyncedDto readers. Before
    // any successful full refresh, no fabricated public success is written.
    let mut value = synced
        .map_or_else(
            || Ok(serde_json::Map::new()),
            |synced| {
                serde_json::to_value(synced)
                    .map(|value| value.as_object().cloned().unwrap_or_default())
            },
        )
        .map_err(std::io::Error::other)?;
    value.insert(
        "playlist_continuation".into(),
        serde_json::to_value(checkpoint).map_err(std::io::Error::other)?,
    );
    let text = serde_json::to_string_pretty(&value).map_err(std::io::Error::other)?;
    let tmp = data_dir.join(format!("{STAMP}.tmp"));
    std::fs::write(&tmp, text)?;
    std::fs::rename(tmp, data_dir.join(STAMP))
}

#[allow(clippy::needless_pass_by_value)] // a map_err adapter
fn store_failure(error: std::io::Error) -> Failure {
    Failure::new(
        ErrorCode::Internal,
        format!("record the library continuation deadline: {error}"),
    )
}

fn full_result(read: &LibrarySyncDetailed, at: i64) -> SyncedDto {
    SyncedDto {
        at,
        tracks: read.library.tracks,
        candidates: read.library.candidates,
        classical: read.library.classical,
        retired: read.library.retired,
        genre_artists_pending: read
            .genres
            .deferred
            .saturating_add(read.genres.failed.len()),
        genre_stalled: 0,
        notes: read.notes.clone(),
    }
}

fn continued_result(previous: SyncedDto, read: &LibrarySyncDetailed, at: i64) -> SyncedDto {
    continued_result_with_note(previous, read, at, NOTE)
}

fn continued_result_with_note(
    mut previous: SyncedDto,
    read: &LibrarySyncDetailed,
    at: i64,
    prefix: &str,
) -> SyncedDto {
    previous.tracks = read.library.tracks;
    previous.candidates = read.library.candidates;
    previous.classical = read.library.classical;
    previous.notes.retain(|note| !note.starts_with(prefix));
    // These are observations from the latest continuation, not a new full sync.
    if !read.notes.is_empty() {
        previous.notes.push(format!(
            "{prefix}last pass at {at}: {}",
            read.notes.join("; ")
        ));
    }
    previous
}

impl LibrarySync {
    pub(super) fn background(&self) -> Option<Result<SyncedDto, Failure>> {
        {
            let mut state = self.running();
            if state.running {
                return None;
            }
            state.running = true;
        }
        // A manual refresh cannot replace the saved owner or cooldown between
        // this selection and its admission. Preserve the last error if idle.
        let now = self.now();
        let pass = if self.due(now) {
            Some(Pass::Full)
        } else {
            load(&self.data_dir)
                .unwrap_or_default()
                .background_pass(now)
        };
        let Some(pass) = pass else {
            self.running().running = false;
            return None;
        };
        self.running().error = None;
        Some(self.run_pass(pass))
    }

    pub(super) fn run_pass(&self, pass: Pass) -> Result<SyncedDto, Failure> {
        let result = self.perform_pass(pass);
        let mut state = self.running();
        state.running = false;
        state.error = result.as_ref().err().map(|error| error.detail.clone());
        result
    }

    fn perform_pass(&self, pass: Pass) -> Result<SyncedDto, Failure> {
        let full = matches!(pass, Pass::Full);
        let album_pass = matches!(pass, Pass::Albums(_));
        let before = self.last();
        let checkpoint = load(&self.data_dir).unwrap_or_default();
        let now = self.now();
        if now < checkpoint.not_before {
            return Err(Failure::new(
                ErrorCode::NotReady,
                format!(
                    "Spotify asked this library to wait until {}; no refresh request was sent",
                    checkpoint.not_before
                ),
            ));
        }
        if !full && before.is_none() {
            return Err(Failure::new(
                ErrorCode::NotReady,
                "library continuation needs a successful full library refresh",
            ));
        }
        // Persist admission before network I/O: interrupted processes retain
        // their unfinished work and cannot restart the retry clock at zero.
        let mut reserved = if album_pass {
            checkpoint.reserve_albums(now)
        } else {
            checkpoint.reserve(now)
        };
        write(&self.data_dir, before.as_ref(), &reserved).map_err(store_failure)?;
        let read = self.read(pass);
        let (result, retry_after) = match read {
            Ok((result, seconds)) => (result.map_err(failure), seconds),
            Err(error) => (Err(error), 0),
        };
        let at = self.now();
        reserved.cooldown(at, retry_after);
        match result {
            Ok(mut read) => {
                read.albums.retry_after_secs = read.albums.retry_after_secs.max(retry_after);
                self.accepted(before, &checkpoint, &read, full, album_pass, at)
            }
            Err(error) => {
                write(&self.data_dir, before.as_ref(), &reserved).map_err(store_failure)?;
                Err(error)
            }
        }
    }

    /// Publish one accepted read to the scheduler and notify active DJ feeds.
    /// Shared by full reads and both continuation paths.
    fn accepted(
        &self,
        before: Option<SyncedDto>,
        checkpoint: &Checkpoint,
        read: &LibrarySyncDetailed,
        full: bool,
        album_pass: bool,
        at: i64,
    ) -> Result<SyncedDto, Failure> {
        let mut next = if album_pass {
            checkpoint.clone()
        } else {
            checkpoint.completed(&read.playlists, at, !full)
        };
        next.observed_albums(&read.albums, at, !album_pass);
        next.album_turn = !album_pass;
        next.cooldown(at, read.albums.retry_after_secs);
        let mut synced = if full {
            let mut synced = full_result(read, at);
            synced.genre_stalled = stalled(before.as_ref(), &synced);
            synced
        } else {
            let before = before.ok_or_else(|| {
                Failure::new(ErrorCode::NotReady, "the full library refresh disappeared")
            })?;
            if album_pass {
                continued_result_with_note(before, read, at, ALBUM_NOTE)
            } else {
                continued_result(before, read, at)
            }
        };
        if !album_pass {
            synced.notes.retain(|note| !note.starts_with(ALBUM_NOTE));
        }
        synced.notes.extend(next.album_note());
        if next.owner.is_some() && !album_pass {
            synced.notes.push(next.note());
        }
        // A partial page cannot yet complete a work. A finished album
        // publishes the same pool-refresh signal as a full sync.
        let completed_since_checkpoint = checkpoint.albums.as_ref().is_some_and(|old| {
            old.owner == read.albums.owner
                && old.generation == read.albums.generation
                && read.albums.pending < old.pending
        });
        if !next.requires_refresh
            && (!album_pass || read.albums.completed > 0 || completed_since_checkpoint)
        {
            self.fresh.store(true, Ordering::Release);
        }
        write(&self.data_dir, Some(&synced), &next).map_err(store_failure)?;
        Ok(synced)
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod album_tests;
