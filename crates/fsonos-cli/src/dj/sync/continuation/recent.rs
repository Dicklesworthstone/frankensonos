//! A lightweight listening read runs before the normal enrichment selection.
//! It shares the worker slot and server cooldown, but never consumes an
//! enrichment turn or changes the last full-library publication timestamp.

use super::{Checkpoint, LibrarySync, Running, load, store_failure, write};
use crate::dj::sync::{STACK, failure};
use asupersync::{Cx, http::Client};
use fsonos_api::surface::dj_sync::RecentListeningDto;
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::store::{SpotifyLibraryOwner, SqliteStore, Store};
use fsonos_spotify::SpotifyError;
use fsonos_spotify::client::TokenCache;
use fsonos_spotify::recent::{RecentListening, refresh_recent_listening};
use fsonos_spotify::session::Session;
use serde::{Deserialize, Serialize};
use std::thread;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub(super) struct RecentCheckpoint {
    pub(super) report: Option<RecentListening>,
    pub(super) retry_at: i64,
    stalled: usize,
    error: Option<String>,
}

impl RecentCheckpoint {
    fn owner_bound(&self) -> bool {
        self.report.as_ref().is_some_and(|report| {
            report.owner.is_some()
                || report.generation.is_some()
                || report.checked_at.is_some()
                || report.received > 0
                || report.accepted > 0
                || report.skipped > 0
        })
    }

    /// Counts and freshness belong to one exact account adoption. Missing
    /// grants and an unadopted account contain no personal observations and
    /// can remain visible without a receipt. An unreadable receipt cannot
    /// prove that a previously accepted window still describes this cache.
    fn retain_owner(&mut self, owner: Option<&SpotifyLibraryOwner>) -> bool {
        if !self.owner_bound() {
            return false;
        }
        if let (Some(report), Some(owner)) = (&self.report, owner)
            && report.owner.as_deref() == Some(owner.owner_id.as_str())
            && report.generation == Some(owner.generation)
        {
            return false;
        }
        self.report = None;
        self.error = None;
        true
    }

    fn reserve(&mut self, now: i64) {
        self.stalled = self.stalled.saturating_add(1);
        self.retry_at = now.saturating_add(Checkpoint::wait(self.stalled));
    }

    fn accept(&mut self, report: RecentListening, now: i64) {
        self.report = Some(report);
        self.stalled = 0;
        self.error = None;
        self.retry_at = now.saturating_add(Checkpoint::wait(0));
    }

    /// A full refresh may adopt a different account. Its listening read is
    /// separate evidence; discard this old status and recheck next tick.
    pub(super) fn after_full_refresh(&mut self, now: i64) {
        *self = Self {
            retry_at: now,
            ..Self::default()
        };
    }
}

pub(super) fn recent_status(
    saved: &Checkpoint,
    running: &Running,
    now: i64,
) -> Option<RecentListeningDto> {
    let state = &saved.recent;
    if state.report.is_none() && state.error.is_none() && state.retry_at == 0 && !running.recent {
        return None;
    }
    let report = state.report.clone().unwrap_or_default();
    Some(RecentListeningDto {
        running: running.recent,
        checked_at: report.checked_at,
        received: report.received,
        accepted: report.accepted,
        skipped: report.skipped,
        more_available: report.more_available,
        missing_scope: report.missing_scope,
        requires_refresh: report.requires_refresh,
        retry_at: Some(state.retry_at.max(saved.not_before)),
        not_before: (saved.not_before > now).then_some(saved.not_before),
        error: state.error.clone(),
    })
}

struct Read {
    result: Result<RecentListening, SpotifyError>,
    retry_after: u64,
}

impl LibrarySync {
    /// Worker boundaries observe the local receipt, including during a
    /// cooldown and after failed reads. Public status remains stamp-only.
    /// This changes no history and preserves admission/server deadlines.
    pub(in crate::dj::sync) fn fence_recent_status(&self, saved: &mut Checkpoint) -> bool {
        if !saved.recent.owner_bound() {
            return false;
        }
        let owner = SqliteStore::open(&self.data_dir.join(crate::daemon::DB_FILE))
            .and_then(|store| store.spotify_library_owner())
            .ok()
            .flatten();
        saved.recent.retain_owner(owner.as_ref())
    }

    /// The caller owns the common worker slot. Persist admission before HTTP
    /// so a restart retains the retry deadline, including a failed attempt.
    pub(in crate::dj::sync) fn refresh_recent_if_due(&self) -> Result<bool, Failure> {
        let now = self.now();
        let mut saved = load(&self.data_dir).unwrap_or_default();
        let before = self.last();
        let fenced = self.fence_recent_status(&mut saved);
        if now < saved.not_before || now < saved.recent.retry_at || !self.signed_in() {
            if fenced {
                write(&self.data_dir, before.as_ref(), &saved).map_err(store_failure)?;
            }
            return Ok(fenced);
        }
        saved.recent.reserve(now);
        saved.recent.retry_at = saved.recent.retry_at.max(saved.not_before);
        write(&self.data_dir, before.as_ref(), &saved).map_err(store_failure)?;
        self.running().recent = true;
        let read = self.read_recent(now);
        self.running().recent = false;
        let at = self.now();
        let (result, retry_after) = match read {
            Ok(read) => (read.result.map_err(failure), read.retry_after),
            Err(error) => (Err(error), 0),
        };
        match result {
            Ok(report) => {
                if report.requires_refresh && !saved.requires_refresh {
                    saved.requires_refresh = true;
                    saved.retry_at = at;
                }
                saved.cooldown(at, report.retry_after_secs.max(retry_after));
                saved.recent.accept(report, at);
            }
            Err(error) => {
                // The old accepted report stays visible with its original
                // timestamp. A failed/empty response never fabricates freshness.
                let authentication_wait = if error.code == ErrorCode::SpotifyAuthRequired {
                    u64::try_from(Checkpoint::wait(saved.recent.stalled)).unwrap_or(u64::MAX)
                } else {
                    0
                };
                saved.recent.error = Some(error.detail);
                saved.cooldown(at, retry_after.max(authentication_wait));
            }
        }
        self.fence_recent_status(&mut saved);
        saved.recent.retry_at = saved.recent.retry_at.max(saved.not_before);
        write(&self.data_dir, before.as_ref(), &saved).map_err(store_failure)?;
        Ok(true)
    }

    fn read_recent(&self, now: i64) -> Result<Read, Failure> {
        let (config, dir) = (self.config.clone(), self.data_dir.clone());
        thread::Builder::new()
            .stack_size(STACK)
            .spawn(move || {
                let runtime = crate::runtime().map_err(|e| SpotifyError::Http(format!("{e:#}")))?;
                runtime.block_on(async move {
                    let cx = Cx::current()
                        .ok_or_else(|| SpotifyError::Http("no async context".into()))?;
                    let mut session = Session::open(
                        config,
                        TokenCache::in_data_dir(&dir),
                        Client::default_for_runtime(&cx),
                    )?;
                    if !session.is_authorized() {
                        return Err(SpotifyError::Auth("not signed in to Spotify".into()));
                    }
                    let mut store = SqliteStore::open(&dir.join(crate::daemon::DB_FILE))?;
                    let result = refresh_recent_listening(&mut session, &cx, &mut store, now).await;
                    Ok(Read {
                        result,
                        retry_after: session.retry_after_secs(),
                    })
                })
            })
            .map_err(|error| {
                Failure::new(
                    ErrorCode::Internal,
                    format!("start recent listening: {error}"),
                )
            })?
            .join()
            .map_err(|_| Failure::new(ErrorCode::Internal, "the recent-listening reader panicked"))?
            .map_err(failure)
    }
}

#[cfg(test)]
mod ownership_tests;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepted_windows_and_interrupted_admission_have_separate_deadlines() {
        let mut saved = RecentCheckpoint::default();
        saved.accept(
            RecentListening {
                checked_at: Some(100),
                accepted: 2,
                ..RecentListening::default()
            },
            100,
        );
        assert_eq!(saved.retry_at, 1_000);
        saved.reserve(1_000);
        assert_eq!(saved.retry_at, 2_800);
        assert_eq!(saved.report.as_ref().unwrap().checked_at, Some(100));
        let encoded = serde_json::to_string(&saved).unwrap();
        let restored: RecentCheckpoint = serde_json::from_str(&encoded).unwrap();
        assert_eq!(restored, saved);
    }

    #[test]
    fn status_retains_last_accepted_window_beside_failure_and_shared_cooldown() {
        let mut saved = Checkpoint::default();
        saved.recent.accept(
            RecentListening {
                checked_at: Some(100),
                received: 3,
                accepted: 2,
                skipped: 1,
                ..RecentListening::default()
            },
            100,
        );
        saved.recent.error = Some("unavailable".into());
        saved.cooldown(200, 3_600);
        let status = recent_status(&saved, &Running::default(), 200).unwrap();
        assert_eq!(status.checked_at, Some(100));
        assert_eq!(status.not_before, Some(3_800));
        assert_eq!(status.retry_at, Some(3_800));
        assert_eq!(status.error.as_deref(), Some("unavailable"));
        saved.recent.after_full_refresh(400);
        let status = recent_status(&saved, &Running::default(), 400).unwrap();
        assert_eq!(
            status.checked_at, None,
            "another account's full refresh must not retain old listening status"
        );
    }

    #[test]
    fn listening_checkpoint_preserves_full_stamp_and_unfinished_intake() {
        let directory = std::env::temp_dir().join(format!(
            "fsonos-recent-stamp-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).unwrap();
        let full = serde_json::from_value::<crate::dj::sync::SyncedDto>(serde_json::json!({
            "at":100,"tracks":0,"candidates":0,"classical":0,"retired":0
        }))
        .unwrap();
        let mut saved = Checkpoint {
            intake_pending: true,
            requires_refresh: true,
            ..Checkpoint::default()
        };
        saved.recent.accept(
            RecentListening {
                checked_at: Some(500),
                ..RecentListening::default()
            },
            500,
        );
        write(&directory, Some(&full), &saved).unwrap();
        assert_eq!(crate::dj::sync::last_synced(&directory), Some(full));
        assert_eq!(load(&directory).unwrap(), saved);
    }
}
