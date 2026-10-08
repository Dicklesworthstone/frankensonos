//! The DJ on the speakers: one fsonos-spotify [`QueueFeed`] per group
//! coordinator, as the surfaces' [`DjEngine`].
//!
//! A start builds the DJ's pool from the library cache (its classical works)
//! and the owner's feedback, queues the first works and plays them. The pool
//! is kept for the playback events that follow, and rebuilt on each start or
//! skip so a library sync in between is picked up.
//!
//! Every pick is steered: the zone's DJ session (from the store) names a
//! mood and constraints, or, with no session or no mood, the time-of-day
//! program in `moods.toml` (the built-in moods and programs when there is no
//! file) picks one for the house's local day and time, which also sets the
//! energy target.

use chrono::{Datelike, Timelike};
use fsonos_api::dj::{DjEngine, DjSpeakers};
use fsonos_api::plan::DjAction;
use fsonos_api::{ErrorCode, Failure, OutcomeDto};
use fsonos_core::clock::Clock;
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::store::Store;
use fsonos_core::{CoreError, control};
use fsonos_spotify::SpotifyError;
use fsonos_spotify::cache::pool_from_store;
use fsonos_spotify::dj::{DjConfig, WorkPool};
use fsonos_spotify::feed::{FeedError, Planning, QueueFeed, QueuedWork, Speakers};
use fsonos_spotify::feedback::{FeedbackModel, StoreFeedback};
use fsonos_spotify::steer::{Moods, Steer, steering};
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Mutex, PoisonError};

/// The moods file's name inside the data directory.
pub const MOODS_FILE: &str = "moods.toml";

/// See the module docs.
pub struct SpotifyDj {
    /// `moods.toml`, read on each start and skip; `None`: the built-ins.
    moods_file: Option<PathBuf>,
    state: Mutex<State>,
}

impl SpotifyDj {
    /// A DJ that reads its moods and programs from `moods_file` (the
    /// built-ins when it does not exist).
    #[must_use]
    pub fn new(moods_file: Option<PathBuf>) -> Self {
        Self {
            moods_file,
            state: Mutex::new(State {
                moods: Moods::builtin(),
                ..State::default()
            }),
        }
    }
}

#[derive(Default)]
struct State {
    feeds: HashMap<PlayerId, QueueFeed>,
    /// What the DJ plans from, as of the last start or skip.
    pool: WorkPool,
    feedback: Option<FeedbackModel>,
    moods: Moods,
}

/// What a pick is planned with right now, beyond the pool.
struct Context {
    steer: Option<Steer>,
    now: i64,
    local_hour: u8,
}

impl State {
    /// Rebuild the pool, the feedback model and the moods.
    fn reload(
        &mut self,
        store: &dyn Store,
        moods_file: Option<&PathBuf>,
        now: i64,
    ) -> Result<(), Failure> {
        let candidates = pool_from_store(store)
            .map_err(|e| Failure::new(ErrorCode::Internal, format!("the DJ's pool: {e}")))?;
        self.pool = WorkPool::new(&candidates);
        // Feedback only weights the picks; without it the DJ still plays.
        self.feedback = FeedbackModel::load(&StoreFeedback(store), now).ok();
        if let Some(path) = moods_file {
            self.moods = Moods::load(path).map_err(|e| {
                Failure::invalid(e.to_string()).with_hint("Fix moods.toml in the data directory.")
            })?;
        }
        Ok(())
    }

    /// The steering and local time for a pick in `coordinator`'s group: its
    /// session's mood and constraints while the session lasts, else the
    /// time-of-day program (fsonos-spotify's `steering`).
    fn context(
        &self,
        store: &dyn Store,
        coordinator: &PlayerId,
        clock: &dyn Clock,
    ) -> Result<Context, Failure> {
        let local = clock.now();
        let steer = steering(
            store,
            &self.moods,
            &coordinator.0,
            local.timestamp(),
            Some((local.weekday(), local.time())),
        )
        .map_err(|e| match e {
            // An unknown mood; any other Config error is steering whose
            // bounds clash (a session's with its mood's).
            SpotifyError::Config(why) if why.starts_with("no mood ") => {
                Failure::new(ErrorCode::UnknownMood, why)
                    .with_suggestions(self.moods.names().map(str::to_string))
            }
            SpotifyError::Config(why) => Failure::invalid(why)
                .with_hint("Fix the zone's DJ steering: its bounds clash with the mood's."),
            other => Failure::new(ErrorCode::Internal, format!("DJ steering: {other}")),
        })?;
        Ok(Context {
            steer,
            now: local.timestamp(),
            local_hour: u8::try_from(local.hour()).unwrap_or(0),
        })
    }
}

fn plan<'a>(
    pool: &'a WorkPool,
    feedback: Option<&'a FeedbackModel>,
    cx: &'a Context,
) -> Planning<'a> {
    Planning {
        pool,
        steer: cx.steer.as_ref(),
        feedback,
        now: cx.now,
        local_hour: Some(cx.local_hour),
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

/// " (mood: evening)", when the pick was steered by a mood.
fn mood(cx: &Context) -> String {
    cx.steer
        .as_ref()
        .and_then(|s| s.mood.as_deref())
        .map_or_else(String::new, |m| format!(" (mood: {m})"))
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
        clock: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let room = room(at);
        let speakers = speakers(at);
        let now = clock.now().timestamp();
        match action {
            DjAction::Start => {
                state.reload(store, self.moods_file.as_ref(), now)?;
                let cx = state.context(store, at.coordinator, clock)?;
                let State {
                    feeds,
                    pool,
                    feedback,
                    ..
                } = &mut *state;
                let feed = feeds.entry(at.coordinator.clone()).or_insert_with(|| {
                    QueueFeed::new(at.coordinator, DjConfig::default(), now.unsigned_abs())
                });
                let queued = feed
                    .start(&speakers, plan(pool, feedback.as_ref(), &cx), store)
                    .map_err(failure)?;
                let next = queued
                    .get(1)
                    .map_or_else(String::new, |w| format!("; then {}", describe(w)));
                Ok(sent(format!(
                    "the DJ is playing in {room}'s group{}: {}{next}",
                    mood(&cx),
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
                state.reload(store, self.moods_file.as_ref(), now)?;
                let cx = state.context(store, at.coordinator, clock)?;
                let State {
                    feeds,
                    pool,
                    feedback,
                    ..
                } = &mut *state;
                let feed = feeds
                    .get_mut(at.coordinator)
                    .ok_or_else(|| no_session(&room))?;
                let work = feed
                    .skip(&speakers, plan(pool, feedback.as_ref(), &cx), store)
                    .map_err(failure)?;
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
        clock: &dyn Clock,
    ) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.feeds.contains_key(at.coordinator) {
            return;
        }
        // A session that names a mood no longer defined still plays, unsteered.
        let cx = state
            .context(store, at.coordinator, clock)
            .unwrap_or_else(|f| {
                tracing::warn!(detail = %f.detail, "the DJ plays unsteered");
                Context {
                    steer: None,
                    now: clock.now().timestamp(),
                    local_hour: u8::try_from(clock.now().hour()).unwrap_or(0),
                }
            });
        let State {
            feeds,
            pool,
            feedback,
            ..
        } = &mut *state;
        let Some(feed) = feeds.get_mut(at.coordinator) else {
            return;
        };
        match feed.on_playback(
            &speakers(at),
            plan(pool, feedback.as_ref(), &cx),
            store,
            playback,
        ) {
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
