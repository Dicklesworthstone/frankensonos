//! The durable local store.
//!
//! [`SqliteStore`] (fsqlite) in production; [`MemStore`] behind the same
//! [`Store`] trait for tests and store-less runs. Holds the inventory cache
//! (players and group edges, for reconnecting when SSDP is quiet), the
//! owner's Spotify library cache, play history (for the DJ's variety and
//! anti-repeat logic), the per-household Spotify render parameters learned
//! from favorites, and the DJ's own state: session steering, listening
//! feedback, and album track lists for completing whole works. The Spotify
//! sign-in's refresh token is not kept here: it is `fsonos-spotify`'s
//! owner-only `TokenCache` file in the data directory. All times are unix
//! seconds stamped by the caller, which keeps behavior deterministic in tests.

pub mod faults;
mod listening;
mod sqlite;

pub use listening::MAX_SPOTIFY_HEARD_TRACKS;
use listening::{latest_plays, merge_plays};
pub use sqlite::{LISTENING_RETENTION_SECS, SqliteStore};

/// The database file's name inside the data directory.
pub const FILE_NAME: &str = "fsonos.db";

/// Bounds for one playlist observation or resumable read. Source positions
/// include unavailable entries, so skipping them cannot evade the item bound.
pub const MAX_PLAYLIST_ENTRIES: u32 = 10_000;
/// Maximum aggregate metadata bytes in an observation or serialized checkpoint,
/// including the directory that discovers the owner's playlists.
pub const MAX_PLAYLIST_BYTES: usize = 16 * 1024 * 1024;
/// Maximum raw entries in a resumable playlist-directory scan.
pub const MAX_PLAYLIST_DIRECTORY_ENTRIES: u32 = 100_000;
/// Maximum artists in a resumable followed-artist directory.
pub const MAX_FOLLOWED_ARTISTS: u32 = 100_000;
/// Maximum aggregate metadata bytes in a followed-artist checkpoint.
pub const MAX_FOLLOWED_ARTIST_BYTES: usize = 16 * 1024 * 1024;
/// Maximum serialized artist-discovery cohort, cursor and publication receipt.
/// The reader separately bounds the finite artist list and each metadata field.
pub const MAX_SPOTIFY_ARTIST_DISCOVERY_BYTES: usize = 16 * 1024 * 1024;
/// At most 50 artists contribute three candidate samples in one continuation.
pub const MAX_SPOTIFY_ARTIST_CONTINUATION_ENTRIES: usize = 150;
/// Maximum compact state retained for an unfinished essential library read.
/// The Spotify reader also bounds pages, source items and individual metadata.
pub const MAX_SPOTIFY_LIBRARY_PROGRESS_BYTES: usize = 64 * 1024 * 1024;

use fsonos_proto::didl::SpotifyRenderParams;
use fsonos_types::{Player, Track, ZoneGroup};
use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

/// One entry of the play history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayRecord {
    pub zone: String,
    /// Service-facing URI, e.g. `spotify:track:<id>`.
    pub source_uri: String,
    /// When it played, in unix seconds (stamped by the caller).
    pub played_at: i64,
}

/// What a player called something while it played it (its title, and its
/// artist when given): how the history names a play the library the DJ
/// reads does not know (a stream, a track outside the owner's library).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayName {
    /// As the history keeps it ([`PlayRecord::source_uri`]).
    pub source_uri: String,
    pub title: String,
    pub artist: Option<String>,
}

/// A player as last seen, for reconnecting without a fresh SSDP pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedPlayer {
    pub household: String,
    pub player: Player,
    pub last_seen: i64,
}

/// One track of the owner's Spotify library cache. `track.uri` (the
/// household-specific renderer URI) is not cached. Build one in tests with
/// `..LibraryEntry::default()`, so a new field doesn't break them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryEntry {
    pub track: Track,
    /// Judged classical (the DJ plays it as part of a whole work). In a row
    /// cached before `candidate` was recorded, it marks a DJ candidate
    /// instead (see [`Self::is_candidate`]).
    pub is_classical: bool,
    /// When the owner saved it, in unix seconds.
    pub added: i64,
    /// `spotify:album:<id>`, for album grouping and whole works.
    pub album_uri: Option<String>,
    /// Album artists, `"; "`-joined like `track.artist`.
    pub album_artists: Option<String>,
    /// Spotify ID of the lead artist, for resumable catalog genre enrichment.
    /// Older cache rows have no ID until the next library read.
    pub artist_id: Option<String>,
    pub origin: LibraryOrigin,
    pub disc_number: Option<u32>,
    pub track_number: Option<u32>,
    /// The work this track belongs to (normalized composer and work), so the
    /// DJ can select whole works.
    pub work_key: Option<String>,
    /// Genre tags the library read gave (album or artist genres; often none).
    pub genres: Vec<String>,
    /// The album's release year, when known.
    pub release_year: Option<u16>,
    /// Spotify marks it explicit (the owner's DJ preferences decide).
    pub explicit: bool,
    /// Whether the DJ may play it: in the owner's library now, in any genre.
    /// `None` in a row cached before this was recorded.
    pub candidate: Option<bool>,
    /// How strongly the owner's own account favors it, per mille (1000 is
    /// neutral), when known.
    pub taste_weight: Option<u32>,
}

/// One playable track occurrence in an observed Spotify playlist. Positions
/// count every source entry, including skipped local files and episodes; two
/// occurrences of the same URI remain two tracks.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct PlaylistTrack {
    /// One-based position in the source playlist, not in the filtered cache.
    pub position: u32,
    pub source_uri: String,
    pub title: String,
    pub artist: Option<String>,
    pub album: Option<String>,
    pub album_uri: Option<String>,
    pub disc_number: Option<u32>,
    pub track_number: Option<u32>,
    pub duration_secs: Option<u32>,
    pub explicit: bool,
}

/// A bounded observation of playlist contents, kept independently of the DJ's
/// deduplicated library. `complete` means pagination ended without an error.
/// The reader fences new resumable reads by Spotify version; legacy rows do
/// not record that proof, and no observation promises current remote state.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct CachedPlaylist {
    pub playlist_uri: String,
    /// When this observation was read, in Unix seconds. Failed refreshes keep
    /// the previous observation, including its original timestamp.
    pub fetched_at: i64,
    /// The source's advertised count, including entries we cannot play.
    pub source_total: Option<u32>,
    /// Raw source entries examined, before filtering unavailable music.
    pub examined: u32,
    pub complete: bool,
    /// A local change detector for this entire cached observation, not a
    /// Spotify snapshot ID or an authorization token.
    pub revision: String,
    pub tracks: Vec<PlaylistTrack>,
}

impl CachedPlaylist {
    /// Make an observation and its deterministic local revision together.
    #[must_use]
    pub fn new(
        playlist_uri: String,
        fetched_at: i64,
        source_total: Option<u32>,
        examined: u32,
        complete: bool,
        tracks: Vec<PlaylistTrack>,
    ) -> Self {
        let mut observed = Self {
            playlist_uri,
            fetched_at,
            source_total,
            examined,
            complete,
            revision: String::new(),
            tracks,
        };
        observed.revision = observed.observation_revision();
        observed
    }

    fn observation_revision(&self) -> String {
        // This tuple contains only strings, integers, booleans and lists:
        // JSON serialization cannot fail. Explicit JSON field order plus a
        // fixed hash keeps the token stable across processes and platforms.
        let bytes = serde_json::to_vec(&(
            &self.playlist_uri,
            self.fetched_at,
            self.source_total,
            self.examined,
            self.complete,
            &self.tracks,
        ))
        .expect("playlist observation fields serialize to JSON");
        let hash = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
        });
        format!("playlist-v1-{hash:016x}")
    }

    fn validate(&self) -> Result<(), StoreError> {
        let source = |uri: &str, kind: &str| {
            uri.strip_prefix(&format!("spotify:{kind}:"))
                .is_some_and(|id| {
                    !id.is_empty()
                        && id.len() <= 128
                        && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
                })
        };
        let bad = |detail: &str| StoreError::Backend(format!("invalid cached playlist: {detail}"));
        if !source(&self.playlist_uri, "playlist") {
            return Err(bad("playlist URI is malformed"));
        }
        if self
            .source_total
            .is_some_and(|total| self.examined > total || (self.complete && self.examined != total))
        {
            return Err(bad("coverage disagrees with the advertised total"));
        }
        if self.examined > MAX_PLAYLIST_ENTRIES || self.tracks.len() > 10_000 {
            return Err(bad("more than 10,000 source entries"));
        }
        let mut bytes = self.playlist_uri.len();
        let mut previous = 0;
        for track in &self.tracks {
            if track.position <= previous || track.position > self.examined {
                return Err(bad(
                    "track positions are not ordered within the observation",
                ));
            }
            previous = track.position;
            if !source(&track.source_uri, "track") {
                return Err(bad("track URI is malformed"));
            }
            if std::iter::once(track.title.as_str())
                .chain(track.artist.as_deref())
                .chain(track.album.as_deref())
                .chain(track.album_uri.as_deref())
                .any(|field| field.len() > 16 * 1024)
            {
                return Err(bad("track metadata is too large"));
            }
            for field in std::iter::once(track.source_uri.as_str())
                .chain(std::iter::once(track.title.as_str()))
                .chain(track.artist.as_deref())
                .chain(track.album.as_deref())
                .chain(track.album_uri.as_deref())
            {
                bytes = bytes
                    .checked_add(field.len())
                    .filter(|size| *size <= MAX_PLAYLIST_BYTES)
                    .ok_or_else(|| bad("aggregate metadata is too large"))?;
            }
        }
        if self.revision != self.observation_revision() {
            return Err(bad("revision does not describe these contents"));
        }
        Ok(())
    }
}

impl LibraryEntry {
    /// Whether the DJ may play it: `candidate`, or `is_classical` in a row
    /// cached before candidacy was recorded (when only classical tracks were
    /// candidates).
    #[must_use]
    pub fn is_candidate(&self) -> bool {
        self.candidate.unwrap_or(self.is_classical)
    }
}

impl Default for LibraryEntry {
    fn default() -> Self {
        Self {
            track: Track {
                title: String::new(),
                artist: None,
                album: None,
                source_uri: String::new(),
                uri: None,
                duration_secs: None,
            },
            is_classical: false,
            added: 0,
            album_uri: None,
            album_artists: None,
            artist_id: None,
            origin: LibraryOrigin::default(),
            disc_number: None,
            track_number: None,
            work_key: None,
            genres: Vec::new(),
            release_year: None,
            explicit: false,
            candidate: None,
            taste_weight: None,
        }
    }
}

/// How a track got into the owner's library.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LibraryOrigin {
    /// A track of a saved album (and the default for rows cached before
    /// origin was recorded).
    #[default]
    SavedAlbum,
    /// A liked ("saved") track.
    LikedTrack,
    /// Both of the above.
    Both,
    /// Not saved: known from the owner's other taste signals (their own
    /// playlists, top tracks, recent plays).
    Taste,
}

impl LibraryOrigin {
    /// The stored form: `saved_album`, `liked_track`, `both` or `taste`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SavedAlbum => "saved_album",
            Self::LikedTrack => "liked_track",
            Self::Both => "both",
            Self::Taste => "taste",
        }
    }

    /// The inverse of [`Self::as_str`].
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "saved_album" => Some(Self::SavedAlbum),
            "liked_track" => Some(Self::LikedTrack),
            "both" => Some(Self::Both),
            "taste" => Some(Self::Taste),
            _ => None,
        }
    }
}

/// A cached OAuth refresh token for a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthEntry {
    pub refresh_token: String,
    /// Access-token expiry, in unix seconds.
    pub expires: i64,
}

/// A DJ session's steering, kept so it survives a daemon restart.
/// Serializable so an action's before-state can carry it for undo.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct DjSession {
    /// The group coordinator's player id: stable across regrouping, unlike
    /// zone names.
    pub coordinator: String,
    pub mood: Option<String>,
    /// Steering constraints, serialized by the DJ.
    pub constraints: Option<String>,
    /// When the steering lapses, in unix seconds. Expired sessions are kept
    /// until deleted; callers compare against their clock.
    pub expires: i64,
}

/// A saved scene: its spec is the scene as JSON (see `crate::scenes`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredScene {
    pub name: String,
    pub spec: String,
    /// When it was last saved, in unix seconds.
    pub updated: i64,
}

/// A stored schedule: its spec and action as `crate::schedule` writes them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSchedule {
    /// Assigned by the store; ignored by [`Store::add_schedule`].
    pub id: i64,
    pub spec: String,
    pub action: String,
    /// The policy client that created it: runs act as this client.
    pub creator: String,
    pub enabled: bool,
    /// Unix seconds.
    pub created: i64,
    /// The run last recorded (fired or skipped), in unix seconds.
    pub last_fired: Option<i64>,
}

/// One piece of listening feedback about something the DJ played.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Feedback {
    /// When it was given, in unix seconds.
    pub at: i64,
    pub work_key: Option<String>,
    pub composer_key: Option<String>,
    pub performer: Option<String>,
    /// A song's lead artist, normalized (a classical work keys its composer
    /// and performer instead).
    pub artist_key: Option<String>,
    /// The album it was heard from.
    pub album_key: Option<String>,
    /// The genres it carries (its tracks' tags).
    pub genres: Vec<String>,
    /// Positive for liked, negative for disliked; the magnitude is strength.
    pub signal: i64,
}

/// What feedback is looked up by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackKey<'a> {
    Work(&'a str),
    Composer(&'a str),
    Performer(&'a str),
    Artist(&'a str),
    Album(&'a str),
}

impl FeedbackKey<'_> {
    fn matches(self, f: &Feedback) -> bool {
        let (field, key) = match self {
            Self::Work(k) => (&f.work_key, k),
            Self::Composer(k) => (&f.composer_key, k),
            Self::Performer(k) => (&f.performer, k),
            Self::Artist(k) => (&f.artist_key, k),
            Self::Album(k) => (&f.album_key, k),
        };
        field.as_deref() == Some(key)
    }
}

/// One track of an album's cached track list. Expanded movements are not
/// library items: this cache is separate from the library.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AlbumTrack {
    pub disc_number: u32,
    pub track_number: u32,
    pub source_uri: String,
    pub title: String,
    pub duration_secs: Option<u32>,
    /// Spotify's content flag. Older cache rows did not retain it; `None`
    /// must be refreshed before this track can become an expanded movement.
    pub explicit: Option<bool>,
}

/// An album's cached track list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedAlbum {
    /// Ordered by disc, then track.
    pub tracks: Vec<AlbumTrack>,
    /// When the list was fetched, in unix seconds.
    pub fetched_at: i64,
}

/// A batch of optional artist metadata. Missing artists are absent from both
/// maps; an empty genre list in `cached` is a successful, reusable answer.
/// Failures stay per artist so healthy rows can still enrich the DJ's pool.
#[derive(Debug, Default)]
pub struct ArtistGenresBatch {
    pub cached: BTreeMap<String, (Vec<String>, i64)>,
    /// Diagnostic messages for artists whose cache entries could not be read.
    pub failed: BTreeMap<String, String>,
}

/// A mutating request, as the action log records it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Action {
    /// Unix seconds.
    pub at: i64,
    /// Who asked: the house-policy client key (`cli`, `mcp-stdio`, a tailnet
    /// principal, `unknown`, ...).
    pub client: String,
    /// The surface that carried it (`cli`, `http`, `mcp`).
    pub surface: String,
    /// What was asked, as the surface describes it.
    pub intent: String,
    /// The policy's verdict: `allow`, `clamp: <reason>` or `deny: <reason>`.
    pub decision: String,
    /// What happened: the outcome, or the failure.
    pub result: String,
    /// The affected zones before the action (serialized snapshots), or
    /// `None` when it cannot be undone (denied, or nothing captured).
    pub before_state: Option<String>,
    /// For an undo: the id of the action it reversed.
    pub undo_of: Option<i64>,
}

/// An [`Action`] with the id the store gave it (ids ascend with time).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedAction {
    pub id: i64,
    pub action: Action,
}

/// Which logged actions to list, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ActionFilter {
    /// Only this client's.
    pub client: Option<String>,
    /// Only those at or after this time (unix seconds).
    pub since: Option<i64>,
    /// At most this many; 0 means no limit.
    pub limit: usize,
}

/// A durable receipt for one adoption of a verified Spotify account.
/// A returning account receives a new generation, so delayed writes from its
/// previous adoption cannot publish or acknowledge another refresh's evidence.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotifyLibraryOwner {
    pub owner_id: String,
    pub generation: u64,
}

/// An artist-discovery cursor update consumed with the accepted library.
/// Exact bytes fence competing refreshes; the reader owns the bounded schema.
/// A new publication receipt also invalidates a delayed full-library snapshot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotifyArtistDiscoveryFence {
    pub expected: Option<String>,
    pub progress: String,
}

/// One validated playlist observation and the exact private proof it consumes.
/// The Spotify reader checks the opaque proof's snapshot and coverage before
/// preparing this value; the store fences and acknowledges it atomically.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpotifyPlaylistPublicationItem {
    pub contents: CachedPlaylist,
    pub expected_progress: String,
    pub applied_progress: String,
}

/// Playlist evidence used by a full library publication. An optional fence
/// distinguishes an unobserved directory from a checked absence. The reader
/// rotates the directory's publication receipt even when membership is equal,
/// so a later publication of a different playlist also invalidates this fence.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpotifyPlaylistPublicationFence {
    pub expected_directory: Option<String>,
    pub directory: Option<String>,
    pub observations: Vec<SpotifyPlaylistPublicationItem>,
}

/// Additive playlist evidence, never a replacement snapshot of the library.
/// Entries must be playable taste rows supported by these observations. Active
/// library rows retain their metadata and provenance; only a stronger taste
/// weight may be merged. A full read alone can retire current candidates.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SpotifyPlaylistPublication {
    pub expected_directory: Option<String>,
    pub directory: Option<String>,
    pub playlists: Option<Vec<(String, String)>>,
    pub entries: Vec<LibraryEntry>,
    pub observations: Vec<SpotifyPlaylistPublicationItem>,
}

/// Persisted state the daemon reads/writes across restarts.
pub trait Store {
    /// Append a play of `source_uri` in `zone` at `played_at`.
    fn record_play(
        &mut self,
        zone: &str,
        source_uri: &str,
        played_at: i64,
    ) -> Result<(), StoreError>;

    /// The last `limit` plays in recording order, oldest first (most recent
    /// last). `zone: None` covers every zone.
    fn recent_plays(&self, zone: Option<&str>, limit: usize)
    -> Result<Vec<PlayRecord>, StoreError>;

    /// How many times `source_uri` is among the last `window` plays in any zone.
    fn recent_play_count(&self, source_uri: &str, window: usize) -> Result<usize, StoreError> {
        Ok(self
            .recent_plays(None, window)?
            .iter()
            .filter(|p| p.source_uri == source_uri)
            .count())
    }

    /// Keep `name` for its URI, as of `at` (unix seconds), replacing the
    /// one kept before. Kept as long as plays are
    /// ([`LISTENING_RETENTION_SECS`] since it was last named).
    fn name_play(&mut self, name: &PlayName, at: i64) -> Result<(), StoreError>;

    /// The names kept ([`Self::name_play`]) for those of `uris` that have
    /// one, in no particular order.
    fn play_names(&self, uris: &[&str]) -> Result<Vec<PlayName>, StoreError>;

    /// Insert or update `players` of `household`, stamped `seen_at`.
    /// Players not listed keep their earlier row (and `last_seen`).
    fn save_players(
        &mut self,
        household: &str,
        players: &[Player],
        seen_at: i64,
    ) -> Result<(), StoreError>;

    /// Every cached player, ordered by household then player id.
    fn cached_players(&self) -> Result<Vec<CachedPlayer>, StoreError>;

    /// Replace `household`'s cached group structure with `groups`.
    fn save_groups(
        &mut self,
        household: &str,
        groups: &[ZoneGroup],
        updated: i64,
    ) -> Result<(), StoreError>;

    /// `household`'s cached groups, in the order they were saved.
    fn cached_groups(&self, household: &str) -> Result<Vec<ZoneGroup>, StoreError>;

    /// Insert or replace library entries, keyed by `track.source_uri`.
    fn upsert_library(&mut self, entries: &[LibraryEntry]) -> Result<(), StoreError>;

    /// The whole library cache, ordered by `added` then `source_uri`.
    fn library(&self) -> Result<Vec<LibraryEntry>, StoreError>;

    /// Monotonic revision of the public library, playlist and album catalog.
    /// A supported store changes it atomically with every catalog write,
    /// including writes by another connection. Private sync progress, listening
    /// history and independent artist-genre caches do not change this token.
    /// Same-value row rewrites may conservatively advance it. Callers must
    /// bracket a catalog snapshot with equal revisions before reusing it.
    /// None disables reuse, including permanently after counter exhaustion.
    /// Custom stores remain safe until they implement the complete contract.
    fn library_catalog_revision(&self) -> Result<Option<u64>, StoreError> {
        Ok(None)
    }

    /// The Spotify account whose library and personal taste this cache holds.
    /// A legacy cache has no proved owner until its first complete library read.
    fn spotify_library_owner(&self) -> Result<Option<SpotifyLibraryOwner>, StoreError>;

    /// Fair artist-discovery progress belonging to this adoption of the account.
    /// A stale or returning account receipt cannot see another generation's cursor.
    /// Progress advances only with an accepted full or additive publication.
    fn spotify_artist_discovery(
        &self,
        owner: &SpotifyLibraryOwner,
    ) -> Result<Option<String>, StoreError>;

    /// Add a bounded artist sample batch while the adoption and exact previous
    /// artist receipt still match. Only candidate Taste rows may be supplied.
    /// Existing active rows retain their metadata and any stronger weight;
    /// no unrelated candidate, playlist or listening evidence is retired.
    /// The new finite-cohort receipt commits atomically with the merged rows.
    /// Receipt-only progress does not change the public catalog revision.
    fn publish_spotify_artist_continuation(
        &mut self,
        owner: &SpotifyLibraryOwner,
        fence: &SpotifyArtistDiscoveryFence,
        entries: &[LibraryEntry],
    ) -> Result<bool, StoreError>;

    /// Private essential-read progress for a verified incoming account, bound
    /// to the public ownership receipt observed before the read began. None
    /// is the receipt of a cache with no verified owner yet. A stale receipt or
    /// a checkpoint from another scope returns no progress. The Spotify reader
    /// validates the opaque checkpoint's schema, endpoints and coverage.
    fn spotify_library_progress(
        &self,
        incoming_id: &str,
        base: Option<&SpotifyLibraryOwner>,
    ) -> Result<Option<String>, StoreError>;

    /// Replace bounded private progress only while both the public receipt and
    /// the exact previous checkpoint match. An absent expected value requires
    /// no checkpoint for this scope. This never changes public library state.
    /// A competing read or account switch returns false without any writes.
    fn save_spotify_library_progress(
        &mut self,
        incoming_id: &str,
        base: Option<&SpotifyLibraryOwner>,
        expected: Option<&str>,
        progress: &str,
    ) -> Result<bool, StoreError>;

    /// Adopt a verified account only after its essential library read succeeds.
    /// On a different or previously unknown owner, atomically retire candidates,
    /// remove prior taste weights and personal playlist/listening observations,
    /// reset this owner's private playlist proofs, and record a new generation.
    /// Catalog caches, local play history and manual preferences are retained.
    /// Returns the durable receipt and whether ownership changed. Adopting the
    /// same owner preserves its generation and changes nothing.
    fn adopt_spotify_library_owner(
        &mut self,
        owner_id: &str,
    ) -> Result<(SpotifyLibraryOwner, bool), StoreError>;

    /// Adopt an account only while its exact completed checkpoint and the
    /// pre-adoption public receipt remain current. The caller must first prove
    /// that the essential read is complete. The checkpoint is retained and
    /// rebound to the returned receipt so failed enrichment or publication can
    /// resume it. A mismatch returns None without any public or private writes.
    fn adopt_spotify_library_from_progress(
        &mut self,
        incoming_id: &str,
        base: Option<&SpotifyLibraryOwner>,
        expected: &str,
    ) -> Result<Option<(SpotifyLibraryOwner, bool)>, StoreError>;

    /// Publish a prepared library and its personal evidence in one transaction,
    /// only while the receipt still names the current owner and generation.
    /// `None` preserves each optional projection; an empty directory clears
    /// it. Listening is merged monotonically and bounded, so an older full
    /// read cannot erase evidence accepted by the lightweight listening reader.
    /// Returns false without writes when any intervening adoption invalidated
    /// the receipt, even when the current account has the same ID again.
    fn publish_spotify_library(
        &mut self,
        owner: &SpotifyLibraryOwner,
        entries: &[LibraryEntry],
        playlists: Option<&[(String, String)]>,
        contents: &[CachedPlaylist],
        heard: Option<&[(String, i64)]>,
    ) -> Result<bool, StoreError>;

    /// Publish and consume a completed essential-read checkpoint in one
    /// transaction, only while both its rebound receipt and exact bytes match.
    /// A competing reader's new checkpoint must not be consumed or overwritten.
    /// Any failed fence leaves all public projections and private progress intact.
    /// A supplied playlist fence also checks and acknowledges its exact private
    /// evidence in this transaction, before another continuation can publish.
    /// The artist fence advances fair discovery only with this accepted library;
    /// a stale cursor leaves the library, all receipts and progress untouched.
    #[allow(clippy::too_many_arguments)]
    fn publish_spotify_library_from_progress(
        &mut self,
        owner: &SpotifyLibraryOwner,
        expected: &str,
        entries: &[LibraryEntry],
        playlists: Option<&[(String, String)]>,
        contents: &[CachedPlaylist],
        heard: Option<&[(String, i64)]>,
        playlist_fence: Option<&SpotifyPlaylistPublicationFence>,
        artist_fence: Option<&SpotifyArtistDiscoveryFence>,
    ) -> Result<bool, StoreError>;

    /// Add validated playlist candidates while preserving the current library.
    /// All ownership, directory and per-playlist receipts must still match.
    /// Contents retention, candidate/weight changes and proof acknowledgement
    /// commit together; a mismatch or error changes none of them. No candidate
    /// is retired, and a partial observation cannot replace complete contents.
    fn publish_spotify_playlist_continuation(
        &mut self,
        owner: &SpotifyLibraryOwner,
        publication: &SpotifyPlaylistPublication,
    ) -> Result<bool, StoreError>;

    /// Acknowledge an already published observation only if both its adoption
    /// receipt and exact private checkpoint remain unchanged. The generation
    /// fence prevents an older refresh from acknowledging a returning owner's
    /// new, not-yet-published proof. A mismatch returns false without writes.
    fn acknowledge_spotify_playlist(
        &mut self,
        owner: &SpotifyLibraryOwner,
        playlist_uri: &str,
        expected: &str,
        applied: &str,
    ) -> Result<bool, StoreError>;

    /// Remember the Spotify render parameters learned for `household`.
    fn save_render_params(
        &mut self,
        household: &str,
        params: &SpotifyRenderParams,
        learned_at: i64,
    ) -> Result<(), StoreError>;

    /// The render parameters learned for `household`, with when.
    fn render_params(
        &self,
        household: &str,
    ) -> Result<Option<(SpotifyRenderParams, i64)>, StoreError>;

    /// Remember `service`'s refresh token.
    fn save_auth(
        &mut self,
        service: &str,
        refresh_token: &str,
        expires: i64,
    ) -> Result<(), StoreError>;

    /// `service`'s cached refresh token, if any.
    fn auth(&self, service: &str) -> Result<Option<AuthEntry>, StoreError>;

    /// Insert or replace the session for `session.coordinator`.
    fn save_dj_session(&mut self, session: &DjSession) -> Result<(), StoreError>;

    /// `coordinator`'s session, if any (expired ones included).
    fn dj_session(&self, coordinator: &str) -> Result<Option<DjSession>, StoreError>;

    /// Every stored session, ordered by coordinator.
    fn dj_sessions(&self) -> Result<Vec<DjSession>, StoreError>;

    /// Forget `coordinator`'s session; nothing happens if there is none.
    fn delete_dj_session(&mut self, coordinator: &str) -> Result<(), StoreError>;

    /// Append one piece of feedback.
    fn record_feedback(&mut self, feedback: &Feedback) -> Result<(), StoreError>;

    /// Feedback about `key` given within `window` (unix seconds, end
    /// exclusive), oldest first; ties keep recording order.
    fn feedback(
        &self,
        key: FeedbackKey<'_>,
        window: Range<i64>,
    ) -> Result<Vec<Feedback>, StoreError>;

    /// All feedback given within `window` (unix seconds, end exclusive),
    /// whatever it is about, oldest first; ties keep recording order.
    fn feedback_between(&self, window: Range<i64>) -> Result<Vec<Feedback>, StoreError>;

    /// Replace `album_uri`'s cached track list; an empty list forgets it. A
    /// repeated (disc, track) keeps the last one given.
    fn save_album_tracks(
        &mut self,
        album_uri: &str,
        tracks: &[AlbumTrack],
        fetched_at: i64,
    ) -> Result<(), StoreError>;

    /// `album_uri`'s cached track list, if it has one.
    fn album_tracks(&self, album_uri: &str) -> Result<Option<CachedAlbum>, StoreError>;

    /// The cached track lists for `album_uris`, as `(uri, album)` pairs; a URI
    /// without a cached list is omitted. The pool build completes partly-held
    /// works from these, so it asks for every album it needs at once rather
    /// than one query per work. The default reads them one by one; a backing
    /// store overrides it with a single query.
    fn album_tracks_many(
        &self,
        album_uris: &[&str],
    ) -> Result<Vec<(String, CachedAlbum)>, StoreError> {
        let mut out = Vec::new();
        for &uri in album_uris {
            if let Some(album) = self.album_tracks(uri)? {
                out.push((uri.to_owned(), album));
            }
        }
        Ok(out)
    }

    /// Cache an artist's genre tags (none is an answer too: the Web API had
    /// no tags for it), read at `fetched_at`.
    fn save_artist_genres(
        &mut self,
        artist_id: &str,
        genres: &[String],
        fetched_at: i64,
    ) -> Result<(), StoreError>;

    /// An artist's cached genre tags and when they were read.
    fn artist_genres(&self, artist_id: &str) -> Result<Option<(Vec<String>, i64)>, StoreError>;

    /// Read each distinct artist once. Optional cache failures are returned
    /// alongside healthy answers, rather than discarding the whole batch.
    /// Backing stores can read bounded chunks without retrying failed chunks
    /// as one query per artist; this default preserves small custom stores.
    fn artist_genres_many(&self, artist_ids: &[&str]) -> ArtistGenresBatch {
        let mut ids = artist_ids.to_vec();
        ids.sort_unstable();
        ids.dedup();
        let mut out = ArtistGenresBatch::default();
        for id in ids {
            match self.artist_genres(id) {
                Ok(Some(cached)) => {
                    out.cached.insert(id.to_owned(), cached);
                }
                Ok(None) => {}
                Err(error) => {
                    out.failed.insert(id.to_owned(), error.to_string());
                }
            }
        }
        out
    }

    /// Replace the albums whose track lists couldn't be had (not there, or
    /// nothing in them playable), each with when it may be read again. A
    /// repeated album keeps the last time given.
    fn save_album_misses(&mut self, misses: &[(String, i64)]) -> Result<(), StoreError>;

    /// Those albums and when each may be read again, by album URI.
    fn album_misses(&self) -> Result<Vec<(String, i64)>, StoreError>;

    /// Replace the playlists in the owner's Spotify list
    /// (`spotify:playlist:<id>`, name), in its order. A repeated playlist
    /// keeps its first place.
    fn save_playlists(&mut self, playlists: &[(String, String)]) -> Result<(), StoreError>;

    /// Those playlists, in the list's order.
    fn playlists(&self) -> Result<Vec<(String, String)>, StoreError>;

    /// Private, bounded directory progress for this adoption of an account.
    /// A stale receipt returns no checkpoint, even after switching away and
    /// back to the same account. The Spotify reader validates its schema.
    fn spotify_playlist_directory(
        &self,
        owner: &SpotifyLibraryOwner,
    ) -> Result<Option<String>, StoreError>;

    /// Replace directory progress only while the adoption receipt and exact
    /// previous checkpoint still match. `expected: None` requires no checkpoint.
    /// A complete directory may also prune private playlist proofs to `active`
    /// in the same transaction; an unfinished scan must pass `None`. Public
    /// names and contents remain unchanged until library publication succeeds.
    /// Returns false without writes after an account switch or competing scan.
    fn save_spotify_playlist_directory(
        &mut self,
        owner: &SpotifyLibraryOwner,
        expected: Option<&str>,
        progress: &str,
        active: Option<&[String]>,
    ) -> Result<bool, StoreError>;

    /// Private followed-artist directory progress for the current adoption.
    /// A stale receipt returns no checkpoint, including after switching away
    /// and back to the same account. The Spotify reader validates its schema,
    /// cursor chain and artist metadata before reusing any persisted rows.
    fn spotify_followed_directory(
        &self,
        owner: &SpotifyLibraryOwner,
    ) -> Result<Option<String>, StoreError>;

    /// Replace bounded private followed-artist progress only while both the
    /// adoption receipt and exact previous checkpoint match. An absent expected
    /// value requires no checkpoint. Returns false without writes after a
    /// competing scan or account switch. This never publishes library or taste
    /// state, or changes playlist and essential-library checkpoints.
    fn save_spotify_followed_directory(
        &mut self,
        owner: &SpotifyLibraryOwner,
        expected: Option<&str>,
        progress: &str,
    ) -> Result<bool, StoreError>;

    /// Atomically replace one playlist's observed contents. An empty complete
    /// observation is retained as evidence of an empty playlist. This does not
    /// change the separate URI/name listing; callers decide refresh retention.
    fn save_playlist_contents(&mut self, contents: &CachedPlaylist) -> Result<(), StoreError>;

    /// The last saved observation, if any; absence is not an empty playlist.
    fn playlist_contents(&self, playlist_uri: &str) -> Result<Option<CachedPlaylist>, StoreError>;

    /// The last playlist ID whose contents a bounded taste scan attempted for
    /// this Spotify owner. It is a position in ID order, not proof of a read.
    fn playlist_scan_cursor(&self, owner_id: &str) -> Result<Option<String>, StoreError>;

    /// Advance one owner's scan before attempting the playlist read. Keeping
    /// attempts separately from observations lets failures yield to later lists
    /// without replacing last-good contents or their observation timestamps.
    fn save_playlist_scan_cursor(
        &mut self,
        owner_id: &str,
        playlist_id: &str,
    ) -> Result<(), StoreError>;

    /// Private, versioned Spotify read state, isolated by owner and playlist.
    /// The Spotify reader validates its schema and snapshot before resuming.
    fn playlist_progress(
        &self,
        owner_id: &str,
        playlist_uri: &str,
    ) -> Result<Option<String>, StoreError>;

    /// Atomically replace one bounded checkpoint after its snapshot is checked.
    /// This does not replace the independently published playlist observation.
    fn save_playlist_progress(
        &mut self,
        owner_id: &str,
        playlist_uri: &str,
        progress: &str,
    ) -> Result<(), StoreError>;

    /// Replace a playlist checkpoint only while its account adoption and exact
    /// previous bytes are still current. Readers capture `expected` before
    /// network I/O. A stale reader cannot overwrite a newer snapshot, including
    /// when an account has been switched away from and adopted again.
    fn save_spotify_playlist_progress(
        &mut self,
        owner: &SpotifyLibraryOwner,
        playlist_uri: &str,
        expected: Option<&str>,
        progress: &str,
    ) -> Result<bool, StoreError>;

    /// Forget this owner's checkpoints absent from a complete readable list.
    /// Callers must not prune from an incomplete directory or another account.
    fn retain_playlist_progress(
        &mut self,
        owner_id: &str,
        playlist_uris: &[String],
    ) -> Result<(), StoreError>;

    /// Replace what the owner played on Spotify lately, on any device
    /// (`spotify:track:` URI, unix seconds), as a sync read it. A track
    /// heard more than once keeps its latest play.
    fn save_spotify_heard(&mut self, plays: &[(String, i64)]) -> Result<(), StoreError>;

    /// Merge recent listening only while the exact account adoption remains
    /// current. Keep the latest timestamp per URI and the latest
    /// [`MAX_SPOTIFY_HEARD_TRACKS`] distinct tracks. Empty or delayed reads
    /// cannot erase evidence. A mismatch returns false without any writes;
    /// this does not change library candidates, weights or catalog revision.
    /// Implementations must compare ownership and merge in one transaction.
    fn merge_spotify_heard(
        &mut self,
        _owner: &SpotifyLibraryOwner,
        _plays: &[(String, i64)],
    ) -> Result<bool, StoreError> {
        Err(StoreError::Backend(
            "this store cannot atomically merge Spotify listening".into(),
        ))
    }

    /// Those plays, oldest first.
    fn spotify_heard(&self) -> Result<Vec<(String, i64)>, StoreError>;

    /// Log `action`; returns its id.
    fn record_action(&mut self, action: &Action) -> Result<i64, StoreError>;

    /// Logged actions matching `filter`, newest first.
    fn recent_actions(&self, filter: &ActionFilter) -> Result<Vec<LoggedAction>, StoreError>;

    /// The newest action that can still be undone (optionally only
    /// `client`'s): it has a before-state, is not itself an undo, and has not
    /// been undone.
    fn last_undoable_action(
        &self,
        client: Option<&str>,
    ) -> Result<Option<LoggedAction>, StoreError>;

    /// Drop actions older than `older_than` (unix seconds) and all but the
    /// newest `keep`; returns how many went.
    fn prune_actions(&mut self, keep: usize, older_than: i64) -> Result<usize, StoreError>;

    /// Insert or replace the scene called `scene.name`.
    fn save_scene(&mut self, scene: &StoredScene) -> Result<(), StoreError>;

    /// The scene called exactly `name`, if any.
    fn scene(&self, name: &str) -> Result<Option<StoredScene>, StoreError>;

    /// Every scene, ordered by name.
    fn scenes(&self) -> Result<Vec<StoredScene>, StoreError>;

    /// Forget the scene called exactly `name`; whether there was one.
    fn delete_scene(&mut self, name: &str) -> Result<bool, StoreError>;

    /// Add a schedule; returns its new id.
    fn add_schedule(&mut self, schedule: &StoredSchedule) -> Result<i64, StoreError>;

    /// Put `schedule` back under its own id (an undo of its removal), unless
    /// a schedule has that id now; whether it did.
    fn put_schedule(&mut self, schedule: &StoredSchedule) -> Result<bool, StoreError>;

    /// Every schedule, by id.
    fn schedules(&self) -> Result<Vec<StoredSchedule>, StoreError>;

    /// Turn schedule `id` on or off; whether it exists.
    fn set_schedule_enabled(&mut self, id: i64, enabled: bool) -> Result<bool, StoreError>;

    /// Record schedule `id`'s run at `at` (unix seconds), only if `at` is
    /// later than the run recorded last. True means this caller claimed the
    /// run; false (already recorded, or no such schedule) means do not run
    /// it, which keeps a run from firing twice.
    fn mark_schedule_fired(&mut self, id: i64, at: i64) -> Result<bool, StoreError>;

    /// Forget schedule `id`; whether there was one.
    fn delete_schedule(&mut self, id: i64) -> Result<bool, StoreError>;
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store backend error: {0}")]
    Backend(String),
}

/// An album's tracks keyed by (disc, track), so iteration is in album order.
type TracksByPosition = BTreeMap<(u32, u32), AlbumTrack>;

/// Reject an empty, oversized or control-bearing Spotify account identity.
pub(crate) fn validate_spotify_owner(owner_id: &str) -> Result<(), StoreError> {
    if owner_id.trim().is_empty()
        || owner_id.len() > 1_024
        || owner_id.chars().any(char::is_control)
    {
        return Err(StoreError::Backend("invalid Spotify library owner".into()));
    }
    Ok(())
}

fn validate_spotify_library_progress(
    incoming_id: &str,
    expected: Option<&str>,
    progress: Option<&str>,
) -> Result<(), StoreError> {
    validate_spotify_owner(incoming_id)?;
    if expected.is_some_and(|value| value.len() > MAX_SPOTIFY_LIBRARY_PROGRESS_BYTES)
        || progress.is_some_and(|value| value.len() > MAX_SPOTIFY_LIBRARY_PROGRESS_BYTES)
    {
        return Err(StoreError::Backend(
            "Spotify library progress is too large".into(),
        ));
    }
    Ok(())
}

fn validate_spotify_artist_discovery(
    owner: &SpotifyLibraryOwner,
    expected: Option<&str>,
    progress: Option<&str>,
) -> Result<(), StoreError> {
    validate_spotify_owner(&owner.owner_id)?;
    if owner.generation == 0 || i64::try_from(owner.generation).is_err() {
        return Err(StoreError::Backend(
            "invalid Spotify library ownership generation".into(),
        ));
    }
    if expected.is_some_and(|value| value.len() > MAX_SPOTIFY_ARTIST_DISCOVERY_BYTES)
        || progress.is_some_and(|value| value.len() > MAX_SPOTIFY_ARTIST_DISCOVERY_BYTES)
    {
        return Err(StoreError::Backend(
            "artist-discovery progress is too large".into(),
        ));
    }
    Ok(())
}

fn validate_spotify_artist_publication(
    owner: &SpotifyLibraryOwner,
    fence: &SpotifyArtistDiscoveryFence,
    entries: &[LibraryEntry],
) -> Result<(), StoreError> {
    validate_spotify_artist_discovery(owner, fence.expected.as_deref(), Some(&fence.progress))?;
    if fence.progress.is_empty() || fence.expected.as_deref() == Some(fence.progress.as_str()) {
        return Err(StoreError::Backend(
            "artist continuation needs a new nonempty publication receipt".into(),
        ));
    }
    if entries.len() > MAX_SPOTIFY_ARTIST_CONTINUATION_ENTRIES {
        return Err(StoreError::Backend(
            "artist continuation has too many samples".into(),
        ));
    }
    let mut seen = BTreeSet::new();
    let mut bytes = 0_usize;
    for entry in entries {
        let source = entry
            .track
            .source_uri
            .strip_prefix("spotify:track:")
            .is_some_and(|id| {
                !id.is_empty()
                    && id.len() <= 128
                    && id.bytes().all(|byte| byte.is_ascii_alphanumeric())
            });
        if entry.origin != LibraryOrigin::Taste
            || entry.candidate != Some(true)
            || !source
            || !seen.insert(&entry.track.source_uri)
            || entry.genres.len() > 256
        {
            return Err(StoreError::Backend(
                "artist continuation needs unique playable candidate taste rows".into(),
            ));
        }
        let fields = std::iter::once(entry.track.source_uri.as_str())
            .chain(std::iter::once(entry.track.title.as_str()))
            .chain(entry.track.artist.as_deref())
            .chain(entry.track.album.as_deref())
            .chain(entry.album_uri.as_deref())
            .chain(entry.album_artists.as_deref())
            .chain(entry.artist_id.as_deref())
            .chain(entry.work_key.as_deref())
            .chain(entry.genres.iter().map(String::as_str));
        for field in fields {
            bytes = bytes.saturating_add(field.len());
            if field.len() > 16 * 1024 || bytes > MAX_SPOTIFY_ARTIST_DISCOVERY_BYTES {
                return Err(StoreError::Backend(
                    "artist sample metadata is too large".into(),
                ));
            }
        }
    }
    Ok(())
}

fn validate_spotify_followed_directory(
    owner: &SpotifyLibraryOwner,
    expected: Option<&str>,
    progress: Option<&str>,
) -> Result<(), StoreError> {
    validate_spotify_owner(&owner.owner_id)?;
    if owner.generation == 0 || i64::try_from(owner.generation).is_err() {
        return Err(StoreError::Backend(
            "invalid Spotify library ownership generation".into(),
        ));
    }
    if expected.is_some_and(|value| value.len() > MAX_FOLLOWED_ARTIST_BYTES)
        || progress.is_some_and(|value| value.len() > MAX_FOLLOWED_ARTIST_BYTES)
    {
        return Err(StoreError::Backend(
            "followed-artist directory is too large".into(),
        ));
    }
    Ok(())
}

fn validate_spotify_playlist_progress(
    owner: &SpotifyLibraryOwner,
    playlist_uri: &str,
    expected: Option<&str>,
    progress: &str,
) -> Result<(), StoreError> {
    validate_spotify_owner(&owner.owner_id)?;
    if owner.generation == 0 || i64::try_from(owner.generation).is_err() {
        return Err(StoreError::Backend(
            "invalid Spotify library ownership generation".into(),
        ));
    }
    if playlist_uri
        .strip_prefix("spotify:playlist:")
        .is_none_or(|id| {
            id.is_empty() || id.len() > 128 || !id.bytes().all(|b| b.is_ascii_alphanumeric())
        })
    {
        return Err(StoreError::Backend("invalid Spotify playlist URI".into()));
    }
    if expected.is_some_and(|value| value.len() > MAX_PLAYLIST_BYTES)
        || progress.len() > MAX_PLAYLIST_BYTES
    {
        return Err(StoreError::Backend("playlist progress is too large".into()));
    }
    Ok(())
}

fn validate_spotify_playlist_evidence(
    owner: &SpotifyLibraryOwner,
    expected_directory: Option<&str>,
    directory: Option<&str>,
    observations: &[SpotifyPlaylistPublicationItem],
) -> Result<(), StoreError> {
    validate_spotify_owner(&owner.owner_id)?;
    if owner.generation == 0 || i64::try_from(owner.generation).is_err() {
        return Err(StoreError::Backend(
            "invalid Spotify library ownership generation".into(),
        ));
    }
    if expected_directory.is_some_and(|value| value.len() > MAX_PLAYLIST_BYTES)
        || directory.is_some_and(|value| value.len() > MAX_PLAYLIST_BYTES)
    {
        return Err(StoreError::Backend(
            "playlist directory is too large".into(),
        ));
    }
    let mut seen = BTreeSet::new();
    for observation in observations {
        observation.contents.validate()?;
        validate_spotify_playlist_progress(
            owner,
            &observation.contents.playlist_uri,
            Some(&observation.expected_progress),
            &observation.applied_progress,
        )?;
        if !seen.insert(&observation.contents.playlist_uri) {
            return Err(StoreError::Backend(
                "duplicate playlist publication proof".into(),
            ));
        }
    }
    Ok(())
}

fn validate_spotify_playlist_publication(
    owner: &SpotifyLibraryOwner,
    publication: &SpotifyPlaylistPublication,
) -> Result<(), StoreError> {
    validate_spotify_playlist_evidence(
        owner,
        publication.expected_directory.as_deref(),
        publication.directory.as_deref(),
        &publication.observations,
    )?;
    let supported: BTreeSet<_> = publication
        .observations
        .iter()
        .flat_map(|observation| &observation.contents.tracks)
        .map(|track| track.source_uri.as_str())
        .collect();
    let mut seen = BTreeSet::new();
    for entry in &publication.entries {
        if entry.origin != LibraryOrigin::Taste
            || entry.candidate != Some(true)
            || !supported.contains(entry.track.source_uri.as_str())
            || !seen.insert(entry.track.source_uri.as_str())
        {
            return Err(StoreError::Backend(
                "playlist continuation needs unique candidate taste rows with proved occurrences"
                    .into(),
            ));
        }
    }
    if publication.playlists.as_ref().is_some_and(|playlists| {
        playlists.len() > MAX_PLAYLIST_DIRECTORY_ENTRIES as usize
            || publication.expected_directory.is_none()
    }) {
        return Err(StoreError::Backend(
            "unproved playlist directory publication".into(),
        ));
    }
    Ok(())
}

fn stronger_playlist_weight(current: Option<u32>, incoming: Option<u32>) -> Option<u32> {
    incoming.filter(|weight| *weight > current.unwrap_or(1_000))
}

/// An in-memory [`Store`] with the same semantics as [`SqliteStore`]
/// (`tests/store_conformance.rs` runs one suite against both).
#[derive(Debug, Default)]
pub struct MemStore {
    history: Vec<PlayRecord>,
    players: BTreeMap<(String, String), CachedPlayer>,
    groups: BTreeMap<String, Vec<ZoneGroup>>,
    library: BTreeMap<String, LibraryEntry>,
    catalog_revision: u64,
    spotify_library_owner: Option<SpotifyLibraryOwner>,
    spotify_library_progress: BTreeMap<String, SpotifyLibraryProgress>,
    render_params: BTreeMap<String, (SpotifyRenderParams, i64)>,
    auth: BTreeMap<String, AuthEntry>,
    dj_sessions: BTreeMap<String, DjSession>,
    feedback: Vec<Feedback>,
    album_tracks: BTreeMap<String, (TracksByPosition, i64)>,
    artist_genres: BTreeMap<String, (Vec<String>, i64)>,
    album_misses: BTreeMap<String, i64>,
    playlists: Vec<(String, String)>,
    playlist_directories: BTreeMap<String, (u64, String)>,
    followed_directories: BTreeMap<String, (u64, String)>,
    artist_discovery: BTreeMap<String, (u64, String)>,
    playlist_contents: BTreeMap<String, CachedPlaylist>,
    playlist_scan_cursors: BTreeMap<String, String>,
    playlist_progress: BTreeMap<(String, String), String>,
    spotify_heard: Vec<(String, i64)>,
    play_names: BTreeMap<String, PlayName>,
    actions: Vec<LoggedAction>,
    scenes: BTreeMap<String, StoredScene>,
    schedules: BTreeMap<i64, StoredSchedule>,
}

#[derive(Debug)]
struct SpotifyLibraryProgress {
    base: Option<SpotifyLibraryOwner>,
    progress: String,
}

impl MemStore {
    fn bump_library_catalog_revision(&mut self) {
        // Values beyond SQLite's signed range permanently disable reuse.
        self.catalog_revision = self.catalog_revision.saturating_add(1);
    }

    fn write_library(&mut self, entries: &[LibraryEntry]) -> bool {
        for entry in entries {
            let mut entry = entry.clone();
            entry.track.uri = None;
            self.library.insert(entry.track.source_uri.clone(), entry);
        }
        !entries.is_empty()
    }

    fn merge_playlist_library(&mut self, entries: &[LibraryEntry]) -> bool {
        let mut changed = false;
        for incoming in entries {
            if let Some(current) = self.library.get_mut(&incoming.track.source_uri)
                && current.is_candidate()
            {
                if let Some(weight) =
                    stronger_playlist_weight(current.taste_weight, incoming.taste_weight)
                {
                    current.taste_weight = Some(weight);
                    changed = true;
                }
            } else {
                changed |= self.write_library(std::slice::from_ref(incoming));
            }
        }
        changed
    }

    fn replace_playlists(&mut self, playlists: &[(String, String)]) -> bool {
        let changed = !self.playlists.is_empty() || !playlists.is_empty();
        self.playlists.clear();
        for (uri, name) in playlists {
            if !self.playlists.iter().any(|(existing, _)| existing == uri) {
                self.playlists.push((uri.clone(), name.clone()));
            }
        }
        changed
    }

    fn library_progress_for(
        &self,
        incoming_id: &str,
        base: Option<&SpotifyLibraryOwner>,
    ) -> Option<&str> {
        if self.spotify_library_owner.as_ref() != base {
            return None;
        }
        self.spotify_library_progress
            .get(incoming_id)
            .filter(|saved| saved.base.as_ref() == base)
            .map(|saved| saved.progress.as_str())
    }

    fn playlist_evidence_matches(
        &self,
        owner: &SpotifyLibraryOwner,
        expected_directory: Option<&str>,
        observations: &[SpotifyPlaylistPublicationItem],
    ) -> bool {
        self.spotify_library_owner.as_ref() == Some(owner)
            && self
                .playlist_directories
                .get(&owner.owner_id)
                .filter(|(generation, _)| *generation == owner.generation)
                .map(|(_, progress)| progress.as_str())
                == expected_directory
            && observations.iter().all(|observation| {
                self.playlist_progress
                    .get(&(
                        owner.owner_id.clone(),
                        observation.contents.playlist_uri.clone(),
                    ))
                    .map(String::as_str)
                    == Some(observation.expected_progress.as_str())
            })
    }

    fn apply_playlist_evidence(
        &mut self,
        owner: &SpotifyLibraryOwner,
        directory: Option<&str>,
        observations: &[SpotifyPlaylistPublicationItem],
    ) {
        for observation in observations {
            self.playlist_progress.insert(
                (
                    owner.owner_id.clone(),
                    observation.contents.playlist_uri.clone(),
                ),
                observation.applied_progress.clone(),
            );
        }
        if let Some(directory) = directory {
            self.playlist_directories.insert(
                owner.owner_id.clone(),
                (owner.generation, directory.to_owned()),
            );
        }
    }
}

impl Store for MemStore {
    fn record_play(
        &mut self,
        zone: &str,
        source_uri: &str,
        played_at: i64,
    ) -> Result<(), StoreError> {
        self.history.push(PlayRecord {
            zone: zone.to_string(),
            source_uri: source_uri.to_string(),
            played_at,
        });
        Ok(())
    }

    fn recent_plays(
        &self,
        zone: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PlayRecord>, StoreError> {
        let mut plays: Vec<PlayRecord> = self
            .history
            .iter()
            .rev()
            .filter(|p| zone.is_none_or(|z| p.zone == z))
            .take(limit)
            .cloned()
            .collect();
        plays.reverse();
        Ok(plays)
    }

    fn name_play(&mut self, name: &PlayName, _at: i64) -> Result<(), StoreError> {
        self.play_names
            .insert(name.source_uri.clone(), name.clone());
        Ok(())
    }

    fn play_names(&self, uris: &[&str]) -> Result<Vec<PlayName>, StoreError> {
        Ok(uris
            .iter()
            .filter_map(|uri| self.play_names.get(*uri).cloned())
            .collect())
    }

    fn save_players(
        &mut self,
        household: &str,
        players: &[Player],
        seen_at: i64,
    ) -> Result<(), StoreError> {
        // A player id is unique across households: moving households moves it.
        for p in players {
            self.players.retain(|(_, id), _| *id != p.id.0);
            self.players.insert(
                (household.to_string(), p.id.0.clone()),
                CachedPlayer {
                    household: household.to_string(),
                    player: p.clone(),
                    last_seen: seen_at,
                },
            );
        }
        Ok(())
    }

    fn cached_players(&self) -> Result<Vec<CachedPlayer>, StoreError> {
        Ok(self.players.values().cloned().collect())
    }

    fn save_groups(
        &mut self,
        household: &str,
        groups: &[ZoneGroup],
        _updated: i64,
    ) -> Result<(), StoreError> {
        self.groups.insert(household.to_string(), groups.to_vec());
        Ok(())
    }

    fn cached_groups(&self, household: &str) -> Result<Vec<ZoneGroup>, StoreError> {
        Ok(self.groups.get(household).cloned().unwrap_or_default())
    }

    fn upsert_library(&mut self, entries: &[LibraryEntry]) -> Result<(), StoreError> {
        if self.write_library(entries) {
            self.bump_library_catalog_revision();
        }
        Ok(())
    }

    fn library(&self) -> Result<Vec<LibraryEntry>, StoreError> {
        let mut all: Vec<LibraryEntry> = self.library.values().cloned().collect();
        all.sort_by(|a, b| (a.added, &a.track.source_uri).cmp(&(b.added, &b.track.source_uri)));
        Ok(all)
    }

    fn library_catalog_revision(&self) -> Result<Option<u64>, StoreError> {
        Ok(i64::try_from(self.catalog_revision)
            .ok()
            .map(|_| self.catalog_revision))
    }

    fn spotify_library_owner(&self) -> Result<Option<SpotifyLibraryOwner>, StoreError> {
        Ok(self.spotify_library_owner.clone())
    }

    fn spotify_artist_discovery(
        &self,
        owner: &SpotifyLibraryOwner,
    ) -> Result<Option<String>, StoreError> {
        validate_spotify_artist_discovery(owner, None, None)?;
        if self.spotify_library_owner.as_ref() != Some(owner) {
            return Ok(None);
        }
        Ok(self
            .artist_discovery
            .get(&owner.owner_id)
            .filter(|(generation, _)| *generation == owner.generation)
            .map(|(_, progress)| progress.clone()))
    }

    fn publish_spotify_artist_continuation(
        &mut self,
        owner: &SpotifyLibraryOwner,
        fence: &SpotifyArtistDiscoveryFence,
        entries: &[LibraryEntry],
    ) -> Result<bool, StoreError> {
        validate_spotify_artist_publication(owner, fence, entries)?;
        if self.spotify_library_owner.as_ref() != Some(owner)
            || self.spotify_artist_discovery(owner)?.as_deref() != fence.expected.as_deref()
        {
            return Ok(false);
        }
        let changed = self.merge_playlist_library(entries);
        self.artist_discovery.insert(
            owner.owner_id.clone(),
            (owner.generation, fence.progress.clone()),
        );
        if changed {
            self.bump_library_catalog_revision();
        }
        Ok(true)
    }

    fn spotify_library_progress(
        &self,
        incoming_id: &str,
        base: Option<&SpotifyLibraryOwner>,
    ) -> Result<Option<String>, StoreError> {
        validate_spotify_owner(incoming_id)?;
        Ok(self
            .library_progress_for(incoming_id, base)
            .map(str::to_owned))
    }

    fn save_spotify_library_progress(
        &mut self,
        incoming_id: &str,
        base: Option<&SpotifyLibraryOwner>,
        expected: Option<&str>,
        progress: &str,
    ) -> Result<bool, StoreError> {
        validate_spotify_library_progress(incoming_id, expected, Some(progress))?;
        if self.spotify_library_owner.as_ref() != base
            || self.library_progress_for(incoming_id, base) != expected
        {
            return Ok(false);
        }
        self.spotify_library_progress.insert(
            incoming_id.to_owned(),
            SpotifyLibraryProgress {
                base: base.cloned(),
                progress: progress.to_owned(),
            },
        );
        Ok(true)
    }

    fn adopt_spotify_library_owner(
        &mut self,
        owner_id: &str,
    ) -> Result<(SpotifyLibraryOwner, bool), StoreError> {
        validate_spotify_owner(owner_id)?;
        if let Some(owner) = &self.spotify_library_owner
            && owner.owner_id == owner_id
        {
            return Ok((owner.clone(), false));
        }
        let generation = self
            .spotify_library_owner
            .as_ref()
            .map_or(0, |owner| owner.generation)
            .checked_add(1)
            .filter(|generation| i64::try_from(*generation).is_ok())
            .ok_or_else(|| StoreError::Backend("Spotify owner generation exhausted".into()))?;
        let owner = SpotifyLibraryOwner {
            owner_id: owner_id.to_owned(),
            generation,
        };
        for entry in self.library.values_mut() {
            entry.candidate = Some(false);
            entry.taste_weight = None;
        }
        self.playlists.clear();
        self.playlist_contents.clear();
        self.spotify_heard.clear();
        // These proofs may have been published under a previous adoption of
        // this account; that public cache has just been discarded.
        self.playlist_progress
            .retain(|(previous, _), _| previous != owner_id);
        self.playlist_directories.remove(owner_id);
        self.followed_directories.remove(owner_id);
        self.artist_discovery.remove(owner_id);
        // Every essential checkpoint was bound to the previous public receipt.
        self.spotify_library_progress.clear();
        self.spotify_library_owner = Some(owner.clone());
        self.bump_library_catalog_revision();
        Ok((owner, true))
    }

    fn adopt_spotify_library_from_progress(
        &mut self,
        incoming_id: &str,
        base: Option<&SpotifyLibraryOwner>,
        expected: &str,
    ) -> Result<Option<(SpotifyLibraryOwner, bool)>, StoreError> {
        validate_spotify_library_progress(incoming_id, Some(expected), None)?;
        if self.library_progress_for(incoming_id, base) != Some(expected) {
            return Ok(None);
        }
        let (owner, changed) = self.adopt_spotify_library_owner(incoming_id)?;
        self.spotify_library_progress.insert(
            incoming_id.to_owned(),
            SpotifyLibraryProgress {
                base: Some(owner.clone()),
                progress: expected.to_owned(),
            },
        );
        Ok(Some((owner, changed)))
    }

    fn publish_spotify_library(
        &mut self,
        owner: &SpotifyLibraryOwner,
        entries: &[LibraryEntry],
        playlists: Option<&[(String, String)]>,
        contents: &[CachedPlaylist],
        heard: Option<&[(String, i64)]>,
    ) -> Result<bool, StoreError> {
        // Validate the entire batch before mutating any personal projection.
        for observation in contents {
            observation.validate()?;
        }
        if self.spotify_library_owner.as_ref() != Some(owner) {
            return Ok(false);
        }
        let mut changed = self.write_library(entries);
        if let Some(playlists) = playlists {
            changed |= self.replace_playlists(playlists);
        }
        for observation in contents {
            self.playlist_contents
                .insert(observation.playlist_uri.clone(), observation.clone());
        }
        if let Some(heard) = heard {
            self.spotify_heard = merge_plays(&self.spotify_heard, heard);
        }
        if changed || !contents.is_empty() {
            self.bump_library_catalog_revision();
        }
        Ok(true)
    }

    #[allow(clippy::too_many_arguments)]
    fn publish_spotify_library_from_progress(
        &mut self,
        owner: &SpotifyLibraryOwner,
        expected: &str,
        entries: &[LibraryEntry],
        playlists: Option<&[(String, String)]>,
        contents: &[CachedPlaylist],
        heard: Option<&[(String, i64)]>,
        playlist_fence: Option<&SpotifyPlaylistPublicationFence>,
        artist_fence: Option<&SpotifyArtistDiscoveryFence>,
    ) -> Result<bool, StoreError> {
        validate_spotify_library_progress(&owner.owner_id, Some(expected), None)?;
        if let Some(fence) = artist_fence {
            validate_spotify_artist_discovery(
                owner,
                fence.expected.as_deref(),
                Some(&fence.progress),
            )?;
            if self.spotify_artist_discovery(owner)?.as_deref() != fence.expected.as_deref() {
                return Ok(false);
            }
        }
        if let Some(fence) = playlist_fence {
            validate_spotify_playlist_evidence(
                owner,
                fence.expected_directory.as_deref(),
                fence.directory.as_deref(),
                &fence.observations,
            )?;
            if !self.playlist_evidence_matches(
                owner,
                fence.expected_directory.as_deref(),
                &fence.observations,
            ) {
                return Ok(false);
            }
        }
        if self.library_progress_for(&owner.owner_id, Some(owner)) != Some(expected) {
            return Ok(false);
        }
        if !self.publish_spotify_library(owner, entries, playlists, contents, heard)? {
            return Ok(false);
        }
        if let Some(fence) = playlist_fence {
            self.apply_playlist_evidence(owner, fence.directory.as_deref(), &fence.observations);
        }
        if let Some(fence) = artist_fence {
            self.artist_discovery.insert(
                owner.owner_id.clone(),
                (owner.generation, fence.progress.clone()),
            );
        }
        self.spotify_library_progress.remove(&owner.owner_id);
        Ok(true)
    }

    fn publish_spotify_playlist_continuation(
        &mut self,
        owner: &SpotifyLibraryOwner,
        publication: &SpotifyPlaylistPublication,
    ) -> Result<bool, StoreError> {
        validate_spotify_playlist_publication(owner, publication)?;
        if !self.playlist_evidence_matches(
            owner,
            publication.expected_directory.as_deref(),
            &publication.observations,
        ) {
            return Ok(false);
        }
        let mut changed = self.merge_playlist_library(&publication.entries);
        if let Some(playlists) = &publication.playlists {
            changed |= self.replace_playlists(playlists);
        }
        for observation in &publication.observations {
            let incoming = &observation.contents;
            if !incoming.complete
                && self
                    .playlist_contents
                    .get(&incoming.playlist_uri)
                    .is_some_and(|current| current.complete)
            {
                continue;
            }
            self.playlist_contents
                .insert(incoming.playlist_uri.clone(), incoming.clone());
            changed = true;
        }
        self.apply_playlist_evidence(
            owner,
            publication.directory.as_deref(),
            &publication.observations,
        );
        if changed {
            self.bump_library_catalog_revision();
        }
        Ok(true)
    }

    fn acknowledge_spotify_playlist(
        &mut self,
        owner: &SpotifyLibraryOwner,
        playlist_uri: &str,
        expected: &str,
        applied: &str,
    ) -> Result<bool, StoreError> {
        if expected.len() > MAX_PLAYLIST_BYTES || applied.len() > MAX_PLAYLIST_BYTES {
            return Err(StoreError::Backend("playlist progress is too large".into()));
        }
        let key = (owner.owner_id.clone(), playlist_uri.to_owned());
        if self.spotify_library_owner.as_ref() != Some(owner)
            || self.playlist_progress.get(&key).map(String::as_str) != Some(expected)
        {
            return Ok(false);
        }
        self.playlist_progress.insert(key, applied.to_owned());
        Ok(true)
    }

    fn save_render_params(
        &mut self,
        household: &str,
        params: &SpotifyRenderParams,
        learned_at: i64,
    ) -> Result<(), StoreError> {
        self.render_params
            .insert(household.to_string(), (params.clone(), learned_at));
        Ok(())
    }

    fn render_params(
        &self,
        household: &str,
    ) -> Result<Option<(SpotifyRenderParams, i64)>, StoreError> {
        Ok(self.render_params.get(household).cloned())
    }

    fn save_auth(
        &mut self,
        service: &str,
        refresh_token: &str,
        expires: i64,
    ) -> Result<(), StoreError> {
        self.auth.insert(
            service.to_string(),
            AuthEntry {
                refresh_token: refresh_token.to_string(),
                expires,
            },
        );
        Ok(())
    }

    fn auth(&self, service: &str) -> Result<Option<AuthEntry>, StoreError> {
        Ok(self.auth.get(service).cloned())
    }

    fn save_dj_session(&mut self, session: &DjSession) -> Result<(), StoreError> {
        self.dj_sessions
            .insert(session.coordinator.clone(), session.clone());
        Ok(())
    }

    fn dj_session(&self, coordinator: &str) -> Result<Option<DjSession>, StoreError> {
        Ok(self.dj_sessions.get(coordinator).cloned())
    }

    fn dj_sessions(&self) -> Result<Vec<DjSession>, StoreError> {
        Ok(self.dj_sessions.values().cloned().collect())
    }

    fn delete_dj_session(&mut self, coordinator: &str) -> Result<(), StoreError> {
        self.dj_sessions.remove(coordinator);
        Ok(())
    }

    fn record_feedback(&mut self, feedback: &Feedback) -> Result<(), StoreError> {
        self.feedback.push(feedback.clone());
        Ok(())
    }

    fn feedback(
        &self,
        key: FeedbackKey<'_>,
        window: Range<i64>,
    ) -> Result<Vec<Feedback>, StoreError> {
        let mut found: Vec<Feedback> = self
            .feedback
            .iter()
            .filter(|f| window.contains(&f.at) && key.matches(f))
            .cloned()
            .collect();
        // Stable: equal times keep recording order.
        found.sort_by_key(|f| f.at);
        Ok(found)
    }

    fn feedback_between(&self, window: Range<i64>) -> Result<Vec<Feedback>, StoreError> {
        let mut found: Vec<Feedback> = self
            .feedback
            .iter()
            .filter(|f| window.contains(&f.at))
            .cloned()
            .collect();
        found.sort_by_key(|f| f.at);
        Ok(found)
    }

    fn save_album_tracks(
        &mut self,
        album_uri: &str,
        tracks: &[AlbumTrack],
        fetched_at: i64,
    ) -> Result<(), StoreError> {
        let changed = if tracks.is_empty() {
            self.album_tracks.remove(album_uri).is_some()
        } else {
            let by_position = tracks
                .iter()
                .map(|t| ((t.disc_number, t.track_number), t.clone()))
                .collect();
            self.album_tracks
                .insert(album_uri.to_string(), (by_position, fetched_at));
            true
        };
        if changed {
            self.bump_library_catalog_revision();
        }
        Ok(())
    }

    fn album_tracks(&self, album_uri: &str) -> Result<Option<CachedAlbum>, StoreError> {
        Ok(self
            .album_tracks
            .get(album_uri)
            .map(|(tracks, fetched_at)| CachedAlbum {
                tracks: tracks.values().cloned().collect(),
                fetched_at: *fetched_at,
            }))
    }

    fn save_artist_genres(
        &mut self,
        artist_id: &str,
        genres: &[String],
        fetched_at: i64,
    ) -> Result<(), StoreError> {
        self.artist_genres
            .insert(artist_id.to_string(), (genres.to_vec(), fetched_at));
        Ok(())
    }

    fn artist_genres(&self, artist_id: &str) -> Result<Option<(Vec<String>, i64)>, StoreError> {
        Ok(self.artist_genres.get(artist_id).cloned())
    }

    fn save_album_misses(&mut self, misses: &[(String, i64)]) -> Result<(), StoreError> {
        self.album_misses = misses.iter().cloned().collect();
        Ok(())
    }

    fn album_misses(&self) -> Result<Vec<(String, i64)>, StoreError> {
        Ok(self
            .album_misses
            .iter()
            .map(|(uri, &at)| (uri.clone(), at))
            .collect())
    }

    fn save_playlists(&mut self, playlists: &[(String, String)]) -> Result<(), StoreError> {
        if self.replace_playlists(playlists) {
            self.bump_library_catalog_revision();
        }
        Ok(())
    }

    fn playlists(&self) -> Result<Vec<(String, String)>, StoreError> {
        Ok(self.playlists.clone())
    }

    fn save_playlist_contents(&mut self, contents: &CachedPlaylist) -> Result<(), StoreError> {
        contents.validate()?;
        self.playlist_contents
            .insert(contents.playlist_uri.clone(), contents.clone());
        self.bump_library_catalog_revision();
        Ok(())
    }

    fn spotify_playlist_directory(
        &self,
        owner: &SpotifyLibraryOwner,
    ) -> Result<Option<String>, StoreError> {
        if self.spotify_library_owner.as_ref() != Some(owner) {
            return Ok(None);
        }
        Ok(self
            .playlist_directories
            .get(&owner.owner_id)
            .filter(|(generation, _)| *generation == owner.generation)
            .map(|(_, progress)| progress.clone()))
    }

    fn save_spotify_playlist_directory(
        &mut self,
        owner: &SpotifyLibraryOwner,
        expected: Option<&str>,
        progress: &str,
        active: Option<&[String]>,
    ) -> Result<bool, StoreError> {
        if progress.len() > MAX_PLAYLIST_BYTES
            || expected.is_some_and(|value| value.len() > MAX_PLAYLIST_BYTES)
        {
            return Err(StoreError::Backend(
                "playlist directory is too large".into(),
            ));
        }
        if self.spotify_library_owner.as_ref() != Some(owner)
            || self.spotify_playlist_directory(owner)?.as_deref() != expected
        {
            return Ok(false);
        }
        self.playlist_directories.insert(
            owner.owner_id.clone(),
            (owner.generation, progress.to_owned()),
        );
        if let Some(active) = active {
            self.retain_playlist_progress(&owner.owner_id, active)?;
        }
        Ok(true)
    }

    fn spotify_followed_directory(
        &self,
        owner: &SpotifyLibraryOwner,
    ) -> Result<Option<String>, StoreError> {
        validate_spotify_followed_directory(owner, None, None)?;
        if self.spotify_library_owner.as_ref() != Some(owner) {
            return Ok(None);
        }
        Ok(self
            .followed_directories
            .get(&owner.owner_id)
            .filter(|(generation, _)| *generation == owner.generation)
            .map(|(_, progress)| progress.clone()))
    }

    fn save_spotify_followed_directory(
        &mut self,
        owner: &SpotifyLibraryOwner,
        expected: Option<&str>,
        progress: &str,
    ) -> Result<bool, StoreError> {
        validate_spotify_followed_directory(owner, expected, Some(progress))?;
        let current = self
            .followed_directories
            .get(&owner.owner_id)
            .filter(|(generation, _)| *generation == owner.generation)
            .map(|(_, progress)| progress.as_str());
        if self.spotify_library_owner.as_ref() != Some(owner) || current != expected {
            return Ok(false);
        }
        self.followed_directories.insert(
            owner.owner_id.clone(),
            (owner.generation, progress.to_owned()),
        );
        Ok(true)
    }

    fn playlist_contents(&self, playlist_uri: &str) -> Result<Option<CachedPlaylist>, StoreError> {
        Ok(self.playlist_contents.get(playlist_uri).cloned())
    }

    fn playlist_scan_cursor(&self, owner_id: &str) -> Result<Option<String>, StoreError> {
        Ok(self.playlist_scan_cursors.get(owner_id).cloned())
    }

    fn save_playlist_scan_cursor(
        &mut self,
        owner_id: &str,
        playlist_id: &str,
    ) -> Result<(), StoreError> {
        self.playlist_scan_cursors
            .insert(owner_id.to_owned(), playlist_id.to_owned());
        Ok(())
    }

    fn playlist_progress(
        &self,
        owner_id: &str,
        playlist_uri: &str,
    ) -> Result<Option<String>, StoreError> {
        Ok(self
            .playlist_progress
            .get(&(owner_id.to_owned(), playlist_uri.to_owned()))
            .cloned())
    }

    fn save_playlist_progress(
        &mut self,
        owner_id: &str,
        playlist_uri: &str,
        progress: &str,
    ) -> Result<(), StoreError> {
        if progress.len() > MAX_PLAYLIST_BYTES {
            return Err(StoreError::Backend("playlist progress is too large".into()));
        }
        self.playlist_progress.insert(
            (owner_id.to_owned(), playlist_uri.to_owned()),
            progress.to_owned(),
        );
        Ok(())
    }

    fn save_spotify_playlist_progress(
        &mut self,
        owner: &SpotifyLibraryOwner,
        playlist_uri: &str,
        expected: Option<&str>,
        progress: &str,
    ) -> Result<bool, StoreError> {
        validate_spotify_playlist_progress(owner, playlist_uri, expected, progress)?;
        let key = (owner.owner_id.clone(), playlist_uri.to_owned());
        if self.spotify_library_owner.as_ref() != Some(owner)
            || self.playlist_progress.get(&key).map(String::as_str) != expected
        {
            return Ok(false);
        }
        self.playlist_progress.insert(key, progress.to_owned());
        Ok(true)
    }

    fn retain_playlist_progress(
        &mut self,
        owner_id: &str,
        playlist_uris: &[String],
    ) -> Result<(), StoreError> {
        let keep: std::collections::BTreeSet<_> = playlist_uris.iter().cloned().collect();
        self.playlist_progress
            .retain(|(owner, uri), _| owner != owner_id || keep.contains(uri));
        Ok(())
    }

    fn save_spotify_heard(&mut self, plays: &[(String, i64)]) -> Result<(), StoreError> {
        self.spotify_heard = latest_plays(plays);
        Ok(())
    }

    fn merge_spotify_heard(
        &mut self,
        owner: &SpotifyLibraryOwner,
        plays: &[(String, i64)],
    ) -> Result<bool, StoreError> {
        if self.spotify_library_owner.as_ref() != Some(owner) {
            return Ok(false);
        }
        self.spotify_heard = merge_plays(&self.spotify_heard, plays);
        Ok(true)
    }

    fn spotify_heard(&self) -> Result<Vec<(String, i64)>, StoreError> {
        Ok(self.spotify_heard.clone())
    }

    fn record_action(&mut self, action: &Action) -> Result<i64, StoreError> {
        // Like an INTEGER PRIMARY KEY: one past the largest id present.
        let id = self.actions.last().map_or(1, |a| a.id + 1);
        self.actions.push(LoggedAction {
            id,
            action: action.clone(),
        });
        Ok(id)
    }

    fn recent_actions(&self, filter: &ActionFilter) -> Result<Vec<LoggedAction>, StoreError> {
        let limit = if filter.limit == 0 {
            usize::MAX
        } else {
            filter.limit
        };
        Ok(self
            .actions
            .iter()
            .rev()
            .filter(|a| filter.client.as_ref().is_none_or(|c| a.action.client == *c))
            .filter(|a| filter.since.is_none_or(|t| a.action.at >= t))
            .take(limit)
            .cloned()
            .collect())
    }

    fn last_undoable_action(
        &self,
        client: Option<&str>,
    ) -> Result<Option<LoggedAction>, StoreError> {
        let undone: Vec<i64> = self
            .actions
            .iter()
            .filter_map(|a| a.action.undo_of)
            .collect();
        Ok(self
            .actions
            .iter()
            .rev()
            .find(|a| {
                a.action.before_state.is_some()
                    && a.action.undo_of.is_none()
                    && !undone.contains(&a.id)
                    && client.is_none_or(|c| a.action.client == c)
            })
            .cloned())
    }

    fn prune_actions(&mut self, keep: usize, older_than: i64) -> Result<usize, StoreError> {
        let before = self.actions.len();
        let excess = before.saturating_sub(keep);
        let mut index = 0;
        self.actions.retain(|a| {
            index += 1;
            index > excess && a.action.at >= older_than
        });
        Ok(before - self.actions.len())
    }

    fn save_scene(&mut self, scene: &StoredScene) -> Result<(), StoreError> {
        self.scenes.insert(scene.name.clone(), scene.clone());
        Ok(())
    }

    fn scene(&self, name: &str) -> Result<Option<StoredScene>, StoreError> {
        Ok(self.scenes.get(name).cloned())
    }

    fn scenes(&self) -> Result<Vec<StoredScene>, StoreError> {
        Ok(self.scenes.values().cloned().collect())
    }

    fn delete_scene(&mut self, name: &str) -> Result<bool, StoreError> {
        Ok(self.scenes.remove(name).is_some())
    }

    fn add_schedule(&mut self, schedule: &StoredSchedule) -> Result<i64, StoreError> {
        let id = self.schedules.keys().next_back().map_or(1, |last| last + 1);
        self.schedules.insert(
            id,
            StoredSchedule {
                id,
                ..schedule.clone()
            },
        );
        Ok(id)
    }

    fn put_schedule(&mut self, schedule: &StoredSchedule) -> Result<bool, StoreError> {
        if self.schedules.contains_key(&schedule.id) {
            return Ok(false);
        }
        self.schedules.insert(schedule.id, schedule.clone());
        Ok(true)
    }

    fn schedules(&self) -> Result<Vec<StoredSchedule>, StoreError> {
        Ok(self.schedules.values().cloned().collect())
    }

    fn set_schedule_enabled(&mut self, id: i64, enabled: bool) -> Result<bool, StoreError> {
        Ok(self
            .schedules
            .get_mut(&id)
            .map(|s| s.enabled = enabled)
            .is_some())
    }

    fn mark_schedule_fired(&mut self, id: i64, at: i64) -> Result<bool, StoreError> {
        match self.schedules.get_mut(&id) {
            Some(s) if s.last_fired.is_none_or(|last| last < at) => {
                s.last_fired = Some(at);
                Ok(true)
            }
            _ => Ok(false),
        }
    }

    fn delete_schedule(&mut self, id: i64) -> Result<bool, StoreError> {
        Ok(self.schedules.remove(&id).is_some())
    }
}

#[cfg(test)]
mod catalog_revision_tests {
    use super::*;

    #[test]
    fn an_exhausted_catalog_clock_never_reuses_an_old_token_or_blocks_writes() {
        let mut store = MemStore {
            catalog_revision: i64::MAX.unsigned_abs() - 1,
            ..MemStore::default()
        };
        for (name, expected) in [
            ("Last", Some(i64::MAX.unsigned_abs())),
            ("Overflow", None),
            ("StillWritable", None),
        ] {
            let entry = LibraryEntry {
                track: Track {
                    source_uri: format!("spotify:track:{name}"),
                    title: name.into(),
                    artist: None,
                    album: None,
                    uri: None,
                    duration_secs: None,
                },
                ..LibraryEntry::default()
            };
            store.upsert_library(&[entry]).unwrap();
            assert_eq!(store.library_catalog_revision().unwrap(), expected);
        }
        assert_eq!(store.library().unwrap().len(), 3);
    }
}
