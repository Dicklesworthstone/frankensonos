//! The daemon's store as the DJ engine sees it: locked for each call, not
//! for a whole DJ command. A DJ start waiting on Spotify, or a top-up
//! talking to the speakers, used to hold the store for seconds, and every
//! command's action log, the scheduler's tick and each read waited behind
//! it. The engine's own lock still keeps its commands apart.
//!
//! The engine takes its lock before any store call, so no engine method may
//! run under [`Surface::with_store`]'s lock: every one goes through
//! [`Surface::with_dj_store`]. That waits out an undo's restore, which the
//! store no longer kept the DJ out of.

use fsonos_core::store::{
    Action, ActionFilter, AlbumTrack, AuthEntry, CachedAlbum, CachedPlayer, DjSession, Feedback,
    FeedbackKey, LibraryEntry, LoggedAction, PlayName, PlayRecord, Store, StoreError, StoredScene,
    StoredSchedule,
};
use fsonos_proto::didl::SpotifyRenderParams;
use fsonos_types::{Player, ZoneGroup};
use std::ops::Range;
use std::sync::{Mutex, MutexGuard, PoisonError};

mod reviewed_undo;

use super::Surface;
use crate::failure::{ErrorCode, Failure};

/// Whether `zone` recorded `uri` within [`super::JUST_RECORDED`] of `at`:
/// the same play, written a moment ago from another thread (the heard
/// history and the DJ hear the same playback event, each on its own). The
/// caller holds the store for the check and the insert after it.
pub(super) fn recorded_just_now(
    store: &dyn Store,
    zone: &str,
    uri: &str,
    at: i64,
) -> Result<bool, StoreError> {
    let last = store.recent_plays(Some(zone), 1)?;
    Ok(last
        .first()
        .is_some_and(|p| p.source_uri == uri && (at - p.played_at).abs() < super::JUST_RECORDED))
}

/// The store behind a mutex, locked for each call (an undo's restore uses it
/// too, see [`Surface::undo`]).
pub(super) struct SharedStore<'a>(pub(super) &'a Mutex<Box<dyn Store + Send>>);

impl SharedStore<'_> {
    fn lock(&self) -> MutexGuard<'_, Box<dyn Store + Send>> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

impl Surface {
    /// Run `f`, a DJ engine call, on the store, if the surface keeps one:
    /// [`Self::with_store`] with the store locked per call (see the module
    /// docs).
    pub(super) fn with_dj_store<R>(
        &self,
        f: impl FnOnce(&mut dyn Store) -> Result<R, StoreError>,
    ) -> Result<Option<R>, Failure> {
        let Some(log) = &self.log else {
            return Ok(None);
        };
        let _restoring = log.restoring.read().unwrap_or_else(PoisonError::into_inner);
        f(&mut SharedStore(&log.store))
            .map(Some)
            .map_err(|e| Failure::new(ErrorCode::Internal, e.to_string()))
    }
}

impl Store for SharedStore<'_> {
    fn record_play(
        &mut self,
        zone: &str,
        source_uri: &str,
        played_at: i64,
    ) -> Result<(), StoreError> {
        // The heard history may have written this very play a moment ago.
        let mut store = self.lock();
        if recorded_just_now(&**store, zone, source_uri, played_at)? {
            return Ok(());
        }
        store.record_play(zone, source_uri, played_at)
    }

    fn recent_plays(
        &self,
        zone: Option<&str>,
        limit: usize,
    ) -> Result<Vec<PlayRecord>, StoreError> {
        self.lock().recent_plays(zone, limit)
    }

    fn recent_play_count(&self, source_uri: &str, window: usize) -> Result<usize, StoreError> {
        self.lock().recent_play_count(source_uri, window)
    }

    fn name_play(&mut self, name: &PlayName, at: i64) -> Result<(), StoreError> {
        self.lock().name_play(name, at)
    }

    fn play_names(&self, uris: &[&str]) -> Result<Vec<PlayName>, StoreError> {
        self.lock().play_names(uris)
    }

    fn save_players(
        &mut self,
        household: &str,
        players: &[Player],
        seen_at: i64,
    ) -> Result<(), StoreError> {
        self.lock().save_players(household, players, seen_at)
    }

    fn cached_players(&self) -> Result<Vec<CachedPlayer>, StoreError> {
        self.lock().cached_players()
    }

    fn save_groups(
        &mut self,
        household: &str,
        groups: &[ZoneGroup],
        updated: i64,
    ) -> Result<(), StoreError> {
        self.lock().save_groups(household, groups, updated)
    }

    fn cached_groups(&self, household: &str) -> Result<Vec<ZoneGroup>, StoreError> {
        self.lock().cached_groups(household)
    }

    fn upsert_library(&mut self, entries: &[LibraryEntry]) -> Result<(), StoreError> {
        self.lock().upsert_library(entries)
    }

    fn library(&self) -> Result<Vec<LibraryEntry>, StoreError> {
        self.lock().library()
    }

    fn save_render_params(
        &mut self,
        household: &str,
        params: &SpotifyRenderParams,
        learned_at: i64,
    ) -> Result<(), StoreError> {
        self.lock()
            .save_render_params(household, params, learned_at)
    }

    fn render_params(
        &self,
        household: &str,
    ) -> Result<Option<(SpotifyRenderParams, i64)>, StoreError> {
        self.lock().render_params(household)
    }

    fn save_auth(
        &mut self,
        service: &str,
        refresh_token: &str,
        expires: i64,
    ) -> Result<(), StoreError> {
        self.lock().save_auth(service, refresh_token, expires)
    }

    fn auth(&self, service: &str) -> Result<Option<AuthEntry>, StoreError> {
        self.lock().auth(service)
    }

    fn save_dj_session(&mut self, session: &DjSession) -> Result<(), StoreError> {
        self.lock().save_dj_session(session)
    }

    fn dj_session(&self, coordinator: &str) -> Result<Option<DjSession>, StoreError> {
        self.lock().dj_session(coordinator)
    }

    fn dj_sessions(&self) -> Result<Vec<DjSession>, StoreError> {
        self.lock().dj_sessions()
    }

    fn delete_dj_session(&mut self, coordinator: &str) -> Result<(), StoreError> {
        self.lock().delete_dj_session(coordinator)
    }

    fn record_feedback(&mut self, feedback: &Feedback) -> Result<(), StoreError> {
        self.lock().record_feedback(feedback)
    }

    fn feedback(
        &self,
        key: FeedbackKey<'_>,
        window: Range<i64>,
    ) -> Result<Vec<Feedback>, StoreError> {
        self.lock().feedback(key, window)
    }

    fn feedback_between(&self, window: Range<i64>) -> Result<Vec<Feedback>, StoreError> {
        self.lock().feedback_between(window)
    }

    fn save_album_tracks(
        &mut self,
        album_uri: &str,
        tracks: &[AlbumTrack],
        fetched_at: i64,
    ) -> Result<(), StoreError> {
        self.lock().save_album_tracks(album_uri, tracks, fetched_at)
    }

    fn album_tracks(&self, album_uri: &str) -> Result<Option<CachedAlbum>, StoreError> {
        self.lock().album_tracks(album_uri)
    }

    fn save_artist_genres(
        &mut self,
        artist_id: &str,
        genres: &[String],
        fetched_at: i64,
    ) -> Result<(), StoreError> {
        self.lock()
            .save_artist_genres(artist_id, genres, fetched_at)
    }

    fn artist_genres(&self, artist_id: &str) -> Result<Option<(Vec<String>, i64)>, StoreError> {
        self.lock().artist_genres(artist_id)
    }

    fn save_album_misses(&mut self, misses: &[(String, i64)]) -> Result<(), StoreError> {
        self.lock().save_album_misses(misses)
    }

    fn album_misses(&self) -> Result<Vec<(String, i64)>, StoreError> {
        self.lock().album_misses()
    }

    fn save_playlists(&mut self, playlists: &[(String, String)]) -> Result<(), StoreError> {
        self.lock().save_playlists(playlists)
    }

    fn playlists(&self) -> Result<Vec<(String, String)>, StoreError> {
        self.lock().playlists()
    }

    fn save_spotify_heard(&mut self, plays: &[(String, i64)]) -> Result<(), StoreError> {
        self.lock().save_spotify_heard(plays)
    }

    fn spotify_heard(&self) -> Result<Vec<(String, i64)>, StoreError> {
        self.lock().spotify_heard()
    }

    fn record_action(&mut self, action: &Action) -> Result<i64, StoreError> {
        self.lock().record_action(action)
    }

    fn recent_actions(&self, filter: &ActionFilter) -> Result<Vec<LoggedAction>, StoreError> {
        self.lock().recent_actions(filter)
    }

    fn last_undoable_action(
        &self,
        client: Option<&str>,
    ) -> Result<Option<LoggedAction>, StoreError> {
        self.lock().last_undoable_action(client)
    }

    fn prune_actions(&mut self, keep: usize, older_than: i64) -> Result<usize, StoreError> {
        self.lock().prune_actions(keep, older_than)
    }

    fn save_scene(&mut self, scene: &StoredScene) -> Result<(), StoreError> {
        self.lock().save_scene(scene)
    }

    fn scene(&self, name: &str) -> Result<Option<StoredScene>, StoreError> {
        self.lock().scene(name)
    }

    fn scenes(&self) -> Result<Vec<StoredScene>, StoreError> {
        self.lock().scenes()
    }

    fn delete_scene(&mut self, name: &str) -> Result<bool, StoreError> {
        self.lock().delete_scene(name)
    }

    fn add_schedule(&mut self, schedule: &StoredSchedule) -> Result<i64, StoreError> {
        self.lock().add_schedule(schedule)
    }

    fn put_schedule(&mut self, schedule: &StoredSchedule) -> Result<bool, StoreError> {
        self.lock().put_schedule(schedule)
    }

    fn schedules(&self) -> Result<Vec<StoredSchedule>, StoreError> {
        self.lock().schedules()
    }

    fn set_schedule_enabled(&mut self, id: i64, enabled: bool) -> Result<bool, StoreError> {
        self.lock().set_schedule_enabled(id, enabled)
    }

    fn mark_schedule_fired(&mut self, id: i64, at: i64) -> Result<bool, StoreError> {
        self.lock().mark_schedule_fired(id, at)
    }

    fn delete_schedule(&mut self, id: i64) -> Result<bool, StoreError> {
        self.lock().delete_schedule(id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::store::MemStore;

    #[test]
    fn a_play_written_a_moment_ago_from_another_follower_is_not_written_again() {
        let store: Mutex<Box<dyn Store + Send>> = Mutex::new(Box::new(MemStore::default()));
        let uri = "spotify:track:0SharedStoreTrack000001";
        let rows = || store.lock().unwrap().recent_plays(None, 10).unwrap().len();
        // The heard history writes the play first...
        store.lock().unwrap().record_play("K", uri, 1_000).unwrap();
        // ...and the DJ, from the same event a moment later, adds nothing.
        SharedStore(&store).record_play("K", uri, 1_005).unwrap();
        assert_eq!(rows(), 1);
        // The same track played again later is a new play.
        let later = 1_000 + super::super::JUST_RECORDED;
        SharedStore(&store).record_play("K", uri, later).unwrap();
        assert_eq!(rows(), 2);
    }
}
