//! The durable local store.
//!
//! [`SqliteStore`] (fsqlite) in production; [`MemStore`] behind the same
//! [`Store`] trait for tests and store-less runs. Holds the inventory cache
//! (players and group edges, for reconnecting when SSDP is quiet), the
//! owner's Spotify library cache, play history (for the DJ's variety and
//! anti-repeat logic), the per-household Spotify render parameters learned
//! from favorites, and the local OAuth refresh token. All times are unix
//! seconds stamped by the caller, which keeps behavior deterministic in tests.

mod sqlite;

pub use sqlite::SqliteStore;

use fsonos_proto::didl::SpotifyRenderParams;
use fsonos_types::{Player, Track, ZoneGroup};
use std::collections::BTreeMap;

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
    pub is_classical: bool,
    /// When the owner saved it, in unix seconds.
    pub added: i64,
}

/// A cached OAuth refresh token for a service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthEntry {
    pub refresh_token: String,
    /// Access-token expiry, in unix seconds.
    pub expires: i64,
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
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store backend error: {0}")]
    Backend(String),
}

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
}
