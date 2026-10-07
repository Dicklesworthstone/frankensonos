//! The durable local store abstraction.
//!
//! Backed by fsqlite in production (bead b-store). Defined as a trait so the
//! daemon and its tests can run against an in-memory implementation. Holds:
//! device inventory cache, Spotify music-library cache, play history (for the
//! DJ's variety/anti-repeat logic), and discovered per-household Spotify render
//! parameters.

/// One entry of the play history.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlayRecord {
    pub zone: String,
    /// Service-facing URI, e.g. `spotify:track:<id>`.
    pub source_uri: String,
    /// When it played, in unix seconds (stamped by the caller).
    pub played_at: i64,
}

/// Persisted state the daemon reads/writes across restarts.
pub trait Store {
    /// Append a play of `source_uri` in `zone` at `played_at` (unix seconds).
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
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store backend error: {0}")]
    Backend(String),
}

/// A trivial in-memory store so the rest of the daemon compiles and tests
/// without a database. The fsqlite-backed `SqliteStore` lands in bead b-store.
#[derive(Debug, Default)]
pub struct MemStore {
    history: Vec<PlayRecord>,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> MemStore {
        let mut s = MemStore::default();
        for (zone, uri, at) in [
            ("den", "spotify:track:1", 100),
            ("lounge", "spotify:track:2", 110),
            ("den", "spotify:track:3", 120),
            ("den", "spotify:track:1", 130),
        ] {
            s.record_play(zone, uri, at).unwrap();
        }
        s
    }

    fn uris(plays: &[PlayRecord]) -> Vec<&str> {
        plays.iter().map(|p| p.source_uri.as_str()).collect()
    }

    #[test]
    fn recent_plays_are_oldest_first_and_limited() {
        let s = store();
        let all = s.recent_plays(None, 10).unwrap();
        assert_eq!(
            uris(&all),
            [
                "spotify:track:1",
                "spotify:track:2",
                "spotify:track:3",
                "spotify:track:1"
            ]
        );
        assert_eq!(all[3].played_at, 130);
        assert_eq!(
            uris(&s.recent_plays(None, 2).unwrap()),
            ["spotify:track:3", "spotify:track:1"]
        );
        assert_eq!(s.recent_plays(None, 0).unwrap().len(), 0);
    }

    #[test]
    fn recent_plays_filter_by_zone() {
        let s = store();
        let den = s.recent_plays(Some("den"), 2).unwrap();
        assert_eq!(uris(&den), ["spotify:track:3", "spotify:track:1"]);
        assert!(den.iter().all(|p| p.zone == "den"));
        assert_eq!(
            uris(&s.recent_plays(Some("lounge"), 5).unwrap()),
            ["spotify:track:2"]
        );
        assert_eq!(s.recent_plays(Some("kitchen"), 5).unwrap().len(), 0);
    }

    #[test]
    fn counts_recent_plays_across_zones() {
        let s = store();
        assert_eq!(s.recent_play_count("spotify:track:1", 10).unwrap(), 2);
        assert_eq!(s.recent_play_count("spotify:track:1", 1).unwrap(), 1);
        assert_eq!(s.recent_play_count("spotify:track:2", 2).unwrap(), 0);
    }
}
