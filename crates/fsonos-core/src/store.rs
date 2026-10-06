//! The durable local store abstraction.
//!
//! Backed by fsqlite in production (bead FND-DEPS). Defined as a trait so the
//! daemon and its tests can run against an in-memory implementation. Holds:
//! device inventory cache, Spotify music-library cache, play history (for the
//! DJ's variety/anti-repeat logic), and discovered per-household Spotify render
//! parameters.

use fsonos_types::Track;

/// Persisted state the daemon reads/writes across restarts.
pub trait Store {
    /// Append a played track to the history (for anti-repeat / variety).
    fn record_play(&mut self, group: &str, track: &Track) -> Result<(), StoreError>;

    /// How many times `source_uri` has played in the last `window` entries.
    fn recent_play_count(&self, source_uri: &str, window: usize) -> Result<usize, StoreError>;
}

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store backend error: {0}")]
    Backend(String),
}

/// A trivial in-memory store so the rest of the daemon compiles and tests
/// without a database. The fsqlite-backed `SqliteStore` lands in FND-DEPS.
#[derive(Debug, Default)]
pub struct MemStore {
    history: Vec<(String, String)>, // (group, source_uri)
}

impl Store for MemStore {
    fn record_play(&mut self, group: &str, track: &Track) -> Result<(), StoreError> {
        self.history
            .push((group.to_string(), track.source_uri.clone()));
        Ok(())
    }

    fn recent_play_count(&self, source_uri: &str, window: usize) -> Result<usize, StoreError> {
        let n = self
            .history
            .iter()
            .rev()
            .take(window)
            .filter(|(_, uri)| uri == source_uri)
            .count();
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_recent_plays() {
        let mut s = MemStore::default();
        let t = Track {
            title: "x".into(),
            artist: None,
            album: None,
            source_uri: "spotify:track:1".into(),
            uri: None,
            duration_secs: None,
        };
        s.record_play("office", &t).unwrap();
        assert_eq!(s.recent_play_count("spotify:track:1", 10).unwrap(), 1);
    }
}
