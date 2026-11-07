//! Durable scheduling for bounded playlist, album, genre and artist-directory reads. The public stamp's
//! timestamp remains the last full sync, while private state survives restarts.

use super::{
    CHECK, ErrorCode, Failure, LibrarySync, LibrarySyncDetailed, LibrarySyncDto, MAX_AGE, Path,
    ReadResult, Running, STAMP, SyncedDto, failure, stalled,
};
use fsonos_api::surface::dj_sync::GenreEnrichmentDto;
use fsonos_spotify::cache::{AlbumContinuation, GenreContinuation, SavedLibraryProgress};
use fsonos_spotify::taste::{FollowedArtistContinuation, PlaylistContinuation};
use serde::{Deserialize, Serialize};
use std::sync::atomic::Ordering;

const NOTE: &str = "Playlist continuation: ";
const ALBUM_NOTE: &str = "Album completion: ";
const GENRE_NOTE: &str = "Genre enrichment: ";
const FOLLOWED_NOTE: &str = "Followed artists: ";

mod followed;
use followed::followed_status;
mod intake;
pub(super) use intake::status;

pub(super) enum Pass {
    Full,
    Playlists(String),
    Albums(Option<AlbumContinuation>),
    Genres(Option<GenreContinuation>),
    Followed(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
enum PassKind {
    Full,
    Playlists,
    Albums,
    Genres,
    Followed,
}

impl Pass {
    fn kind(&self) -> PassKind {
        match self {
            Self::Full => PassKind::Full,
            Self::Playlists(_) => PassKind::Playlists,
            Self::Albums(_) => PassKind::Albums,
            Self::Genres(_) => PassKind::Genres,
            Self::Followed(_) => PassKind::Followed,
        }
    }
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
    #[serde(default)]
    genres: Option<GenreContinuation>,
    #[serde(default)]
    genre_stalled: usize,
    #[serde(default)]
    genre_retry_at: i64,
    #[serde(default)]
    followed: Option<FollowedArtistContinuation>,
    #[serde(default)]
    followed_stalled: usize,
    #[serde(default)]
    followed_retry_at: i64,
    /// Rotate the independent readers; old stamps retain album_turn.
    #[serde(default)]
    next_enrichment: Option<PassKind>,
    /// A full read has been admitted, but has not published a complete library.
    #[serde(default)]
    intake_pending: bool,
    /// Compact metadata only; retained page bodies stay in the private store.
    #[serde(default)]
    intake: Option<SavedLibraryProgress>,
    #[serde(default)]
    intake_error: Option<String>,
}

impl Checkpoint {
    fn wait(stalled: usize) -> i64 {
        let check = i64::try_from(CHECK.as_secs()).unwrap_or(i64::MAX);
        let max = i64::try_from(MAX_AGE.as_secs()).unwrap_or(i64::MAX);
        let shift = u32::try_from(stalled.min(16)).unwrap_or(16);
        check.saturating_mul(1_i64 << shift).min(max)
    }

    fn cooldown(&mut self, now: i64, seconds: u64) {
        if seconds > 0 {
            let until = now.saturating_add(i64::try_from(seconds).unwrap_or(i64::MAX));
            self.not_before = self.not_before.max(until);
        }
        self.retry_at = self.retry_at.max(self.not_before);
        self.album_retry_at = self.album_retry_at.max(self.not_before);
        self.genre_retry_at = self.genre_retry_at.max(self.not_before);
        self.followed_retry_at = self.followed_retry_at.max(self.not_before);
    }

    fn reserve(&self, now: i64) -> Self {
        let mut reserved = self.clone();
        reserved.stalled = reserved.stalled.saturating_add(1);
        reserved.retry_at = now.saturating_add(Self::wait(reserved.stalled));
        reserved.retry_at = reserved.retry_at.max(reserved.not_before);
        reserved
    }

    /// Keep the full-read intent across failures and restarts even when the last
    /// successful refresh is still recent. It also excludes enrichment passes.
    fn reserve_full(&self, now: i64) -> Self {
        let mut reserved = self.reserve(now);
        reserved.requires_refresh = true;
        reserved.intake_pending = true;
        reserved.intake_error = None;
        reserved
    }

    /// A healthy bounded read made progress without publishing a new library.
    /// Continue at the next check instead of treating its budget as a failure.
    fn continue_full(&mut self, now: i64) {
        self.requires_refresh = true;
        self.stalled = 0;
        self.retry_at = now.saturating_add(Self::wait(0)).max(self.not_before);
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
            genres: self.genres.clone(),
            genre_stalled: self.genre_stalled,
            genre_retry_at: if self.genres.is_none() {
                now.saturating_add(Self::wait(0))
            } else {
                self.genre_retry_at
            },
            next_enrichment: self.next_enrichment,
            followed: self.followed.clone(),
            followed_stalled: self.followed_stalled,
            followed_retry_at: self.followed_retry_at,
            intake_pending: self.intake_pending,
            intake: self.intake.clone(),
            intake_error: self.intake_error.clone(),
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

    pub(super) fn has_genres(&self) -> bool {
        self.genres.is_some()
    }

    fn genre_due(&self, now: i64) -> bool {
        !self.requires_refresh
            && !self.reconcile
            && now >= self.not_before
            && now >= self.genre_retry_at
            && self
                .genres
                .as_ref()
                .is_some_and(|genres| genres.pending > 0)
    }

    fn reserve_genres(&self, now: i64) -> Self {
        let mut reserved = self.clone();
        reserved.genre_stalled = reserved.genre_stalled.saturating_add(1);
        reserved.genre_retry_at = now
            .saturating_add(Self::wait(reserved.genre_stalled))
            .max(reserved.not_before);
        reserved
    }

    fn observed_genres(&mut self, report: &GenreContinuation, now: i64, keep_cursor: bool) {
        if keep_cursor
            && report.completed == 0
            && !report.requires_refresh
            && let Some(previous) = &self.genres
            && previous.pending > 0
            && report.pending > 0
            && previous.owner == report.owner
            && previous.generation == report.generation
        {
            let mut genres = report.clone();
            genres.inherit_progress(previous);
            self.genres = Some(genres);
            // A playlist/full observation must not reset a due genre reader
            // or move its cursor back to the first failing artist.
            self.cooldown(now, report.retry_after_secs);
            return;
        }
        let progress = report.completed > 0
            || self.genres.as_ref().is_none_or(|previous| {
                report.pending < previous.pending
                    || report.owner != previous.owner
                    || report.generation != previous.generation
            });
        self.genre_stalled = if progress || report.pending == 0 {
            0
        } else {
            self.genre_stalled.saturating_add(1)
        };
        self.genre_retry_at = now.saturating_add(Self::wait(self.genre_stalled));
        let mut genres = report.clone();
        if keep_cursor && let Some(previous) = &self.genres {
            genres.inherit_progress(previous);
        }
        if report.requires_refresh {
            self.requires_refresh = true;
            self.retry_at = self.genre_retry_at;
        }
        self.genres = Some(genres);
        self.cooldown(now, report.retry_after_secs);
    }

    fn genre_note(&self) -> Option<String> {
        let genres = self.genres.as_ref()?;
        if genres.requires_refresh {
            return Some(format!(
                "{GENRE_NOTE}the Spotify account needs a full library refresh"
            ));
        }
        (genres.pending > 0 || genres.completed > 0).then(|| {
            format!(
                "{GENRE_NOTE}{} artist lookup(s) completed, {} unfinished; the next bounded pass follows the saved retry deadline",
                genres.completed, genres.pending
            )
        })
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
            && report.advanced == 0
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
        let genres = self.genre_due(now);
        let followed = self.followed_due(now);
        let first = self.next_enrichment.unwrap_or(if self.album_turn {
            PassKind::Albums
        } else {
            PassKind::Playlists
        });
        let start = match first {
            PassKind::Full | PassKind::Playlists => 0,
            PassKind::Albums => 1,
            PassKind::Genres => 2,
            PassKind::Followed => 3,
        };
        for offset in 0..4 {
            match (start + offset) % 4 {
                0 => {
                    if let Some(owner) = &owner {
                        return Some(Pass::Playlists(owner.clone()));
                    }
                }
                1 if albums => return Some(Pass::Albums(self.albums.clone())),
                2 if genres => return Some(Pass::Genres(self.genres.clone())),
                3 => {
                    if let Some(owner) = &followed {
                        return Some(Pass::Followed(owner.clone()));
                    }
                }
                _ => {}
            }
        }
        None
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

fn genre_status(saved: &Checkpoint, running: bool, now: i64) -> Option<GenreEnrichmentDto> {
    let genres = saved.genres.as_ref()?;
    Some(GenreEnrichmentDto {
        pending: genres.pending,
        completed: genres.completed,
        running,
        retry_at: (genres.pending > 0 || genres.requires_refresh).then_some(saved.genre_retry_at),
        not_before: (saved.not_before > now).then_some(saved.not_before),
        requires_refresh: genres.requires_refresh || saved.requires_refresh,
    })
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
        genre_artists_pending: read.genre_continuation.pending,
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

/// Older full-sync stamps put genre diagnostics beside the other read notes.
/// A current genre pass replaces only those diagnostics, keeping taste and
/// album failures visible until their own readers resolve them.
fn legacy_genre_note(note: &str) -> bool {
    if note.starts_with("the genre read stopped early (") {
        return true;
    }
    note.split_once(' ').is_some_and(|(count, detail)| {
        count.parse::<usize>().is_ok()
            && (detail == "artists' genres wait for the next sync"
                || detail.starts_with("artist's genres couldn't be read (")
                || detail.starts_with("artists' genres couldn't be read ("))
    })
}

impl LibrarySync {
    pub(super) fn background(&self) -> Option<Result<LibrarySyncDto, Failure>> {
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

    pub(super) fn run_pass(&self, pass: Pass) -> Result<LibrarySyncDto, Failure> {
        {
            let mut state = self.running();
            state.genres = matches!(pass, Pass::Genres(_));
            state.followed = matches!(pass, Pass::Followed(_));
            state.intake = matches!(pass, Pass::Full);
        }
        let result = self.perform_pass(pass);
        {
            let mut state = self.running();
            state.running = false;
            state.genres = false;
            state.followed = false;
            state.intake = false;
            state.error = result.as_ref().err().map(|error| error.detail.clone());
        }
        result.map(|()| self.status())
    }

    fn perform_pass(&self, pass: Pass) -> Result<(), Failure> {
        let kind = pass.kind();
        let full = kind == PassKind::Full;
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
        let reserved = match kind {
            PassKind::Albums => checkpoint.reserve_albums(now),
            PassKind::Genres => checkpoint.reserve_genres(now),
            PassKind::Followed => checkpoint.reserve_followed(now),
            PassKind::Full => checkpoint.reserve_full(now),
            PassKind::Playlists => checkpoint.reserve(now),
        };
        write(&self.data_dir, before.as_ref(), &reserved).map_err(store_failure)?;
        let read = self.read(pass, checkpoint.albums.clone());
        self.finish_pass(before, &checkpoint, reserved, kind, read)
    }

    fn finish_pass(
        &self,
        before: Option<SyncedDto>,
        checkpoint: &Checkpoint,
        mut reserved: Checkpoint,
        kind: PassKind,
        read: Result<ReadResult, Failure>,
    ) -> Result<(), Failure> {
        let full = kind == PassKind::Full;
        let (result, retry_after, pending, progress, progress_observed) = match read {
            Ok(ReadResult {
                result,
                retry_after,
                progress,
                progress_observed,
            }) => {
                let pending = matches!(
                    &result,
                    Err(fsonos_spotify::SpotifyError::LibraryPending { .. })
                );
                (
                    result.map_err(failure),
                    retry_after,
                    pending,
                    progress,
                    progress_observed,
                )
            }
            Err(error) => (Err(error), 0, false, None, false),
        };
        let at = self.now();
        if full {
            // Starting a worker or calling /me is not an observation of saved
            // pages. Only a persisted checkpoint or definite invalidation can
            // replace the compact report retained by the previous attempt.
            if progress_observed {
                reserved.intake = progress;
            }
            reserved.intake_error = if pending {
                None
            } else {
                result.as_ref().err().map(|error| error.detail.clone())
            };
        }
        if full && pending {
            reserved.continue_full(at);
        }
        reserved.cooldown(at, retry_after);
        if full && pending {
            write(&self.data_dir, before.as_ref(), &reserved).map_err(store_failure)?;
            return Ok(());
        }
        match result {
            Ok(mut read) => {
                read.albums.retry_after_secs = read.albums.retry_after_secs.max(retry_after);
                read.genre_continuation.retry_after_secs =
                    read.genre_continuation.retry_after_secs.max(retry_after);
                read.followed_artists.retry_after_secs =
                    read.followed_artists.retry_after_secs.max(retry_after);
                self.accepted(before, checkpoint, &read, kind, at)
                    .map(|_| ())
            }
            Err(error) => {
                write(&self.data_dir, before.as_ref(), &reserved).map_err(store_failure)?;
                Err(error)
            }
        }
    }

    /// Publish one accepted read to the scheduler and notify active DJ feeds.
    /// Shared by full reads and all continuation paths.
    fn accepted(
        &self,
        before: Option<SyncedDto>,
        checkpoint: &Checkpoint,
        read: &LibrarySyncDetailed,
        kind: PassKind,
        at: i64,
    ) -> Result<SyncedDto, Failure> {
        let full = kind == PassKind::Full;
        let album_pass = kind == PassKind::Albums;
        let genre_pass = kind == PassKind::Genres;
        let followed_pass = kind == PassKind::Followed;
        let mut next = if album_pass || genre_pass || followed_pass {
            checkpoint.clone()
        } else {
            checkpoint.completed(&read.playlists, at, !full)
        };
        if full {
            next.intake_pending = false;
            next.intake = None;
            next.intake_error = None;
        }
        // Genre-only passes just observe missing albums; full and playlist
        // passes can now return newly read pages. Retain a previous checkpoint
        // only when this report contains no newer album progress.
        if !followed_pass {
            next.observed_albums(&read.albums, at, !album_pass);
        }
        if !genre_pass && !followed_pass {
            next.album_turn = !album_pass;
        }
        if !album_pass && !followed_pass {
            next.observed_genres(&read.genre_continuation, at, !genre_pass);
        }
        if full || followed_pass {
            next.observed_followed(&read.followed_artists, at);
        }
        next.next_enrichment = match kind {
            PassKind::Full => next.next_enrichment,
            PassKind::Playlists => Some(PassKind::Albums),
            PassKind::Albums => Some(PassKind::Genres),
            PassKind::Genres => Some(PassKind::Followed),
            PassKind::Followed => Some(PassKind::Playlists),
        };
        next.cooldown(
            at,
            read.albums
                .retry_after_secs
                .max(read.genre_continuation.retry_after_secs)
                .max(read.followed_artists.retry_after_secs)
                .max(read.playlists.retry_after_secs),
        );
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
            } else if genre_pass {
                let mut synced = continued_result_with_note(before, read, at, GENRE_NOTE);
                synced.notes.retain(|note| !legacy_genre_note(note));
                synced
            } else if followed_pass {
                continued_result_with_note(before, read, at, FOLLOWED_NOTE)
            } else {
                continued_result(before, read, at)
            }
        };
        if !album_pass && !followed_pass {
            synced.genre_artists_pending = read.genre_continuation.pending;
            if !full {
                synced.genre_stalled = next.genre_stalled;
            }
        }
        if !album_pass && !followed_pass {
            synced.notes.retain(|note| !note.starts_with(ALBUM_NOTE));
        }
        if !genre_pass && !followed_pass {
            synced.notes.retain(|note| !note.starts_with(GENRE_NOTE));
        }
        if !followed_pass {
            synced.notes.extend(next.album_note());
            synced.notes.extend(next.genre_note());
        }
        if full || followed_pass {
            synced.notes.extend(next.followed_note());
        }
        if next.owner.is_some() && !album_pass && !genre_pass && !followed_pass {
            synced.notes.push(next.note());
        }
        // A partial page cannot yet complete a work. A finished album
        // publishes the same pool-refresh signal as a full sync.
        let completed_since_checkpoint = checkpoint.albums.as_ref().is_some_and(|old| {
            old.owner == read.albums.owner
                && old.generation == read.albums.generation
                && read.albums.pending < old.pending
        });
        let genres_completed_since_checkpoint = checkpoint.genres.as_ref().is_some_and(|old| {
            old.owner == read.genre_continuation.owner
                && old.generation == read.genre_continuation.generation
                && read.genre_continuation.pending < old.pending
        });
        let published = match kind {
            PassKind::Full | PassKind::Playlists => true,
            PassKind::Albums => read.albums.completed > 0 || completed_since_checkpoint,
            PassKind::Genres => {
                read.genre_continuation.completed > 0 || genres_completed_since_checkpoint
            }
            PassKind::Followed => read.followed_artists.advanced > 0,
        };
        if !next.requires_refresh && published {
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

#[cfg(test)]
mod genre_tests;
