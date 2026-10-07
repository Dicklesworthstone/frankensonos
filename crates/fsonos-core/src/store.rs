//! The durable local store.
//!
//! [`SqliteStore`] (fsqlite) in production; [`MemStore`] behind the same
//! [`Store`] trait for tests and store-less runs. Holds the inventory cache
//! (players and group edges, for reconnecting when SSDP is quiet), the
//! owner's Spotify library cache, play history (for the DJ's variety and
//! anti-repeat logic), the per-household Spotify render parameters learned
//! from favorites, the local OAuth refresh token, and the DJ's own state:
//! session steering, listening feedback, and album track lists for completing
//! whole works. All times are unix seconds stamped by the caller, which keeps
//! behavior deterministic in tests.

mod sqlite;

pub use sqlite::SqliteStore;

use fsonos_proto::didl::SpotifyRenderParams;
use fsonos_types::{Player, Track, ZoneGroup};
use std::collections::BTreeMap;
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

/// A player as last seen, for reconnecting without a fresh SSDP pass.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedPlayer {
    pub household: String,
    pub player: Player,
    pub last_seen: i64,
}

/// One track of the owner's Spotify library cache. `track.uri` (the
/// household-specific renderer URI) is not cached.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LibraryEntry {
    pub track: Track,
    /// False for explicit tracks too: the DJ never plays those.
    pub is_classical: bool,
    /// When the owner saved it, in unix seconds.
    pub added: i64,
    /// `spotify:album:<id>`, for album grouping and whole works.
    pub album_uri: Option<String>,
    /// Album artists, `"; "`-joined like `track.artist`.
    pub album_artists: Option<String>,
    pub origin: LibraryOrigin,
    pub disc_number: Option<u32>,
    pub track_number: Option<u32>,
    /// The work this track belongs to (normalized composer and work), so the
    /// DJ can select whole works.
    pub work_key: Option<String>,
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
}

impl LibraryOrigin {
    /// The stored form: `saved_album`, `liked_track` or `both`.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::SavedAlbum => "saved_album",
            Self::LikedTrack => "liked_track",
            Self::Both => "both",
        }
    }

    /// The inverse of [`Self::as_str`].
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "saved_album" => Some(Self::SavedAlbum),
            "liked_track" => Some(Self::LikedTrack),
            "both" => Some(Self::Both),
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
#[derive(Debug, Clone, PartialEq, Eq)]
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

/// One piece of listening feedback about something the DJ played.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Feedback {
    /// When it was given, in unix seconds.
    pub at: i64,
    pub work_key: Option<String>,
    pub composer_key: Option<String>,
    pub performer: Option<String>,
    /// Positive for liked, negative for disliked; the magnitude is strength.
    pub signal: i64,
}

/// What feedback is looked up by.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FeedbackKey<'a> {
    Work(&'a str),
    Composer(&'a str),
    Performer(&'a str),
}

impl FeedbackKey<'_> {
    fn matches(self, f: &Feedback) -> bool {
        let (field, key) = match self {
            Self::Work(k) => (&f.work_key, k),
            Self::Composer(k) => (&f.composer_key, k),
            Self::Performer(k) => (&f.performer, k),
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
}

/// An album's cached track list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CachedAlbum {
    /// Ordered by disc, then track.
    pub tracks: Vec<AlbumTrack>,
    /// When the list was fetched, in unix seconds.
    pub fetched_at: i64,
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
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store backend error: {0}")]
    Backend(String),
}

/// An album's tracks keyed by (disc, track), so iteration is in album order.
type TracksByPosition = BTreeMap<(u32, u32), AlbumTrack>;

/// An in-memory [`Store`] with the same semantics as [`SqliteStore`]
/// (`tests/store_conformance.rs` runs one suite against both).
#[derive(Debug, Default)]
pub struct MemStore {
    history: Vec<PlayRecord>,
    players: BTreeMap<(String, String), CachedPlayer>,
    groups: BTreeMap<String, Vec<ZoneGroup>>,
    library: BTreeMap<String, LibraryEntry>,
    render_params: BTreeMap<String, (SpotifyRenderParams, i64)>,
    auth: BTreeMap<String, AuthEntry>,
    dj_sessions: BTreeMap<String, DjSession>,
    feedback: Vec<Feedback>,
    album_tracks: BTreeMap<String, (TracksByPosition, i64)>,
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
        for e in entries {
            let mut e = e.clone();
            e.track.uri = None;
            self.library.insert(e.track.source_uri.clone(), e);
        }
        Ok(())
    }

    fn library(&self) -> Result<Vec<LibraryEntry>, StoreError> {
        let mut all: Vec<LibraryEntry> = self.library.values().cloned().collect();
        all.sort_by(|a, b| (a.added, &a.track.source_uri).cmp(&(b.added, &b.track.source_uri)));
        Ok(all)
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

    fn save_album_tracks(
        &mut self,
        album_uri: &str,
        tracks: &[AlbumTrack],
        fetched_at: i64,
    ) -> Result<(), StoreError> {
        if tracks.is_empty() {
            self.album_tracks.remove(album_uri);
        } else {
            let by_position = tracks
                .iter()
                .map(|t| ((t.disc_number, t.track_number), t.clone()))
                .collect();
            self.album_tracks
                .insert(album_uri.to_string(), (by_position, fetched_at));
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
}
