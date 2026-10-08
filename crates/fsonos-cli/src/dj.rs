//! The DJ on the speakers: one fsonos-spotify [`QueueFeed`] per group
//! coordinator, as the surfaces' [`DjEngine`].
//!
//! A start builds the DJ's pool from the library cache (its classical works)
//! and the owner's feedback, queues the first works and plays them. The pool
//! is kept for the playback events that follow, and rebuilt on each start or
//! skip so a library sync in between is picked up.

use fsonos_api::dj::{DjEngine, DjSpeakers};
use fsonos_api::plan::DjAction;
use fsonos_api::{ErrorCode, Failure, OutcomeDto};
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::store::Store;
use fsonos_core::{CoreError, control};
use fsonos_spotify::cache::pool_from_store;
use fsonos_spotify::dj::{DjConfig, WorkPool};
use fsonos_spotify::feed::{FeedError, Planning, QueueFeed, QueuedWork, Speakers};
use fsonos_spotify::feedback::{FeedbackModel, StoreFeedback};
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::sync::{Mutex, PoisonError};

/// See the module docs.
#[derive(Default)]
pub struct SpotifyDj {
    state: Mutex<State>,
}

#[derive(Default)]
struct State {
    feeds: HashMap<PlayerId, QueueFeed>,
    /// What the DJ plans from, as of the last start or skip.
    pool: WorkPool,
    feedback: Option<FeedbackModel>,
}

impl State {
    /// Rebuild the pool and the feedback model from `store`.
    fn reload(&mut self, store: &dyn Store, now: i64) -> Result<(), Failure> {
        let candidates = pool_from_store(store)
            .map_err(|e| Failure::new(ErrorCode::Internal, format!("the DJ's pool: {e}")))?;
        self.pool = WorkPool::new(&candidates);
        // Feedback only weights the picks; without it the DJ still plays.
        self.feedback = FeedbackModel::load(&StoreFeedback(store), now).ok();
        Ok(())
    }
}

fn speakers<'a>(at: DjSpeakers<'a>) -> Speakers<'a, dyn fsonos_proto::Transport + 'a> {
    Speakers {
        transport: at.transport,
        households: at.households,
        coordinator: at.coordinator,
    }
}

fn room(at: DjSpeakers<'_>) -> String {
    control::locate(at.households, at.coordinator)
        .map_or_else(|_| at.coordinator.0.clone(), |p| p.room_name.clone())
}

fn sent(done: String) -> OutcomeDto {
    OutcomeDto {
        done,
        changed: true,
        volume: None,
        notes: Vec::new(),
    }
}

fn describe(work: &QueuedWork) -> String {
    format!("{}: {}", work.composer, work.title)
}

fn no_session(room: &str) -> Failure {
    Failure::new(
        ErrorCode::NoDjSession,
        format!("the DJ isn't running in {room}'s group"),
    )
}

fn failure(err: FeedError) -> Failure {
    let detail = err.to_string();
    match err {
        FeedError::EmptyPool => Failure::new(ErrorCode::SpotifyAuthRequired, detail).with_hint(
            "Sign in to Spotify on the daemon host and sync the library; the DJ plays its classical works.",
        ),
        FeedError::NoRenderParams => Failure::new(ErrorCode::RenderParamsMissing, detail),
        FeedError::Inactive => Failure::new(ErrorCode::NoDjSession, detail),
        FeedError::Core(e) => Failure::from(e),
        FeedError::Proto(e) => Failure::from(CoreError::from(e)),
        FeedError::Store(e) => Failure::new(ErrorCode::Internal, e.to_string()),
    }
}

impl DjEngine for SpotifyDj {
    fn act(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        action: DjAction,
        now: i64,
    ) -> Result<OutcomeDto, Failure> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let room = room(at);
        let speakers = speakers(at);
        match action {
            DjAction::Start => {
                state.reload(store, now)?;
                let State {
                    feeds,
                    pool,
                    feedback,
                } = &mut *state;
                let plan = Planning {
                    pool,
                    steer: None,
                    feedback: feedback.as_ref(),
                    now,
                    local_hour: None,
                };
                let feed = feeds.entry(at.coordinator.clone()).or_insert_with(|| {
                    QueueFeed::new(at.coordinator, DjConfig::default(), now.unsigned_abs())
                });
                let queued = feed.start(&speakers, plan, store).map_err(failure)?;
                let next = queued
                    .get(1)
                    .map_or_else(String::new, |w| format!("; then {}", describe(w)));
                Ok(sent(format!(
                    "the DJ is playing in {room}'s group: {}{next}",
                    describe(&queued[0])
                )))
            }
            DjAction::Skip => {
                if !state
                    .feeds
                    .get(at.coordinator)
                    .is_some_and(QueueFeed::is_active)
                {
                    return Err(no_session(&room));
                }
                state.reload(store, now)?;
                let State {
                    feeds,
                    pool,
                    feedback,
                } = &mut *state;
                let plan = Planning {
                    pool,
                    steer: None,
                    feedback: feedback.as_ref(),
                    now,
                    local_hour: None,
                };
                let feed = feeds
                    .get_mut(at.coordinator)
                    .ok_or_else(|| no_session(&room))?;
                let work = feed.skip(&speakers, plan, store).map_err(failure)?;
                Ok(sent(format!("skipped to {}", describe(work))))
            }
            DjAction::Stop => {
                let mut feed = state
                    .feeds
                    .remove(at.coordinator)
                    .filter(QueueFeed::is_active)
                    .ok_or_else(|| no_session(&room))?;
                feed.stop(&speakers).map_err(failure)?;
                Ok(sent(format!("the DJ stopped in {room}'s group")))
            }
        }
    }

    fn feeds(&self, coordinator: &PlayerId) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .feeds
            .get(coordinator)
            .is_some_and(QueueFeed::is_active)
    }

    fn on_playback(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        playback: &PlayerPlayback,
        now: i64,
    ) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let State {
            feeds,
            pool,
            feedback,
        } = &mut *state;
        let Some(feed) = feeds.get_mut(at.coordinator) else {
            return;
        };
        let plan = Planning {
            pool,
            steer: None,
            feedback: feedback.as_ref(),
            now,
            local_hour: None,
        };
        match feed.on_playback(&speakers(at), plan, store, playback) {
            Ok(0) => {}
            Ok(queued) => tracing::info!(queued, "the DJ topped up the queue"),
            Err(e) => {
                tracing::warn!(error = %e, "the DJ could not top up; retrying on the next change");
            }
        }
        if !feed.is_active() {
            // The owner cleared or replaced the queue: the DJ steps aside.
            feeds.remove(at.coordinator);
        }
    }
}
