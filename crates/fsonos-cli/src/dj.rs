//! The DJ on the speakers: one fsonos-spotify [`QueueFeed`] per group
//! coordinator, as the surfaces' [`DjEngine`].
//!
//! A start builds the DJ's pool from the library cache (songs and classical works)
//! and the owner's feedback, queues the first works and plays them. The pool
//! is kept for the playback events that follow, and rebuilt on each start or
//! skip so a library sync in between is picked up.
//!
//! Every pick is steered: the zone's DJ session (from the store) names a
//! mood and constraints, or, with no session or no mood, the time-of-day
//! program in `moods.toml` (the built-in moods and programs when there is no
//! file) picks one for the house's local day and time, which also sets the
//! energy target.
//!
//! The library cache is refreshed by [`sync`] (daily in the daemon, or on
//! `fsonos dj sync`); once a refresh lands, a running DJ rebuilds its pool
//! at its next top-up.
//!
//! A steer replaces the zone's stored session (or a clear deletes it); it is
//! checked against the moods first, and applies from the next pick whether
//! or not the DJ runs. Status and moods read the same session and programs
//! ([`crate::dj_view`]).

use chrono::{Datelike, Timelike};
use fsonos_api::dj::{DjEngine, DjMoodsDto, DjSpeakers, DjStatusDto, DjSteer};
use fsonos_api::plan::DjAction;
use fsonos_api::surface::dj_feedback::{DjFeedback, DjFeedbackDto};
use fsonos_api::surface::dj_prefs::{PrefChange, PreferencesDto, PreferredDto};
use fsonos_api::surface::dj_sync::LibrarySyncDto;
use fsonos_api::{ErrorCode, Failure, OutcomeDto};
use fsonos_core::clock::Clock;
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::store::Store;
use fsonos_core::{CoreError, control};
use fsonos_spotify::cache::works_from_store;
use fsonos_spotify::dj::{DjConfig, WorkPool};
use fsonos_spotify::feed::{FeedError, Planning, QueueFeed, QueuedWork, Speakers};
use fsonos_spotify::feedback::{FeedbackModel, StoreFeedback};
use fsonos_spotify::steer::{DjSession, Moods, Steer, steering};
use fsonos_types::PlayerId;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::dj_view::{self, steer_failure, store_failure};

pub(crate) mod feedback;
pub(crate) mod prefs;
pub(crate) mod sync;

/// The moods file's name inside the data directory.
pub const MOODS_FILE: &str = "moods.toml";

/// See the module docs.
pub struct SpotifyDj {
    /// `moods.toml`, read on each start and skip; `None`: the built-ins.
    moods_file: Option<PathBuf>,
    state: Mutex<State>,
    /// Refreshes the library cache (the daemon's, with a Spotify app).
    library: Option<Arc<sync::LibrarySync>>,
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
            library: None,
        }
    }

    /// Refresh the library cache through `library` (`dj_sync`).
    #[must_use]
    pub fn with_library(mut self, library: Option<Arc<sync::LibrarySync>>) -> Self {
        self.library = library;
        self
    }

    fn library(&self) -> Result<&Arc<sync::LibrarySync>, Failure> {
        self.library.as_ref().ok_or_else(|| {
            Failure::new(
                ErrorCode::NotImplemented,
                "this daemon has no Spotify app to refresh the library with",
            )
            .with_hint(
                "Start fsonos serve with FSONOS_SPOTIFY_CLIENT_ID set (fsonos setup), or run \
                 fsonos dj sync with it set.",
            )
        })
    }

    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

#[derive(Default)]
struct State {
    feeds: HashMap<PlayerId, QueueFeed>,
    /// What the DJ plans from, as of the last start or skip.
    pool: WorkPool,
    feedback: Option<FeedbackModel>,
    moods: Moods,
    /// What each group is hearing, for early skips and full listens.
    listening: HashMap<PlayerId, feedback::Listening>,
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
        // The daemon's DJ plays from works, with partly-held works completed
        // from the album track lists a sync cached (no network here).
        self.pool = works_from_store(store)
            .map_err(|e| Failure::new(ErrorCode::Internal, format!("the DJ's pool: {e}")))?;
        // Feedback only weights the picks; without it the DJ still plays.
        self.feedback = FeedbackModel::load(&StoreFeedback(store), now).ok();
        self.reload_moods(moods_file)?;
        // The owner's standing preferences shape every pick from here.
        prefs::apply(self, moods_file)
    }

    /// Re-read the moods and programs (the built-ins without a file).
    fn reload_moods(&mut self, moods_file: Option<&PathBuf>) -> Result<(), Failure> {
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
        // Bounds that clash are a session's with its mood's.
        .map_err(|e| {
            steer_failure(
                e,
                "Fix the zone's DJ steering: its bounds clash with the mood's.",
            )
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
            "Sign in to Spotify and sync the library: fsonos setup does both, and running it again re-reads a library cached before the DJ played every genre.",
        ),
        FeedError::NoRenderParams => Failure::new(ErrorCode::RenderParamsMissing, detail),
        FeedError::Inactive => Failure::new(ErrorCode::NoDjSession, detail),
        FeedError::Core(e) => Failure::from(e),
        FeedError::Proto(e) => Failure::from(CoreError::from(e)),
        FeedError::Store(e) => Failure::new(ErrorCode::Internal, e.to_string()),
    }
}

/// Replace or clear the stored session of `at`'s group (see the module
/// docs).
fn steer(
    state: &State,
    at: DjSpeakers<'_>,
    store: &mut dyn Store,
    steer: &DjSteer,
    clock: &dyn Clock,
) -> Result<OutcomeDto, Failure> {
    let room = room(at);
    let key = &at.coordinator.0;
    let when = if state
        .feeds
        .get(at.coordinator)
        .is_some_and(QueueFeed::is_active)
    {
        "from the next piece; the one queued ahead stays"
    } else {
        "when the DJ plays there"
    };
    match steer {
        DjSteer::Clear => {
            if store.dj_session(key).map_err(store_failure)?.is_none() {
                return Ok(OutcomeDto {
                    changed: false,
                    ..sent(format!("{room}'s group has no DJ steering to clear"))
                });
            }
            store.delete_dj_session(key).map_err(store_failure)?;
            Ok(sent(format!(
                "cleared the DJ steering in {room}'s group: it follows the time-of-day program ({when})"
            )))
        }
        DjSteer::Set {
            mood,
            constraints,
            for_secs,
        } => {
            let now = clock.now().timestamp();
            let session = DjSession {
                zone: key.clone(),
                mood: mood.clone(),
                constraints: dj_view::to_dj(constraints)?,
                expires_at: for_secs.map(|secs| now.saturating_add_unsigned(secs)),
            };
            // An unknown mood, or bounds that clash with the mood's.
            session.steer(&state.moods, None).map_err(|e| {
                steer_failure(
                    e,
                    "These constraints clash with the mood's own: change one, or pick another mood.",
                )
            })?;
            let row = session.to_store().map_err(store_failure)?;
            store.save_dj_session(&row).map_err(store_failure)?;
            Ok(sent(format!(
                "steered {room}'s group: {} ({when})",
                dj_view::describe(mood.as_deref(), constraints, *for_secs)
            )))
        }
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
                state.listening.remove(at.coordinator);
                Ok(sent(format!("the DJ stopped in {room}'s group")))
            }
        }
    }

    fn steer(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        steer_by: &DjSteer,
        clock: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        let mut state = self.state();
        state.reload_moods(self.moods_file.as_ref())?;
        steer(&state, at, store, steer_by, clock)
    }

    fn status(
        &self,
        at: DjSpeakers<'_>,
        store: &dyn Store,
        queue_position: Option<u32>,
        clock: &dyn Clock,
    ) -> Result<DjStatusDto, Failure> {
        let mut state = self.state();
        state.reload_moods(self.moods_file.as_ref())?;
        let steering = dj_view::steering_of(store, &state.moods, at.coordinator, clock)?;
        Ok(dj_view::status(
            room(at),
            state.feeds.get(at.coordinator),
            &state.pool,
            queue_position,
            steering,
        ))
    }

    fn moods(
        &self,
        at: Option<DjSpeakers<'_>>,
        store: &dyn Store,
        clock: &dyn Clock,
    ) -> Result<DjMoodsDto, Failure> {
        let mut state = self.state();
        state.reload_moods(self.moods_file.as_ref())?;
        let now = match at {
            Some(at) => dj_view::steering_of(store, &state.moods, at.coordinator, clock)?,
            None => dj_view::program_now(&state.moods, clock),
        };
        Ok(dj_view::moods_of(&state.moods, now))
    }

    fn feeds(&self, coordinator: &PlayerId) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .feeds
            .get(coordinator)
            .is_some_and(QueueFeed::is_active)
    }

    fn moved(&self, from: &PlayerId, to: &PlayerId) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let State {
            feeds, listening, ..
        } = &mut *state;
        if let Some(mut feed) = feeds.remove(from) {
            feed.rekey(to);
            feeds.insert(to.clone(), feed);
        }
        if let Some(heard) = listening.remove(from) {
            listening.insert(to.clone(), heard);
        }
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
        // A library refresh landed: plan from what it found from here.
        if self.library.as_ref().is_some_and(|l| l.take_fresh())
            && let Err(f) = state.reload(store, self.moods_file.as_ref(), clock.now().timestamp())
        {
            tracing::warn!(detail = %f.detail, "the DJ keeps its pool from before the refresh");
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
            listening,
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
        // An early skip or a full listen, as the movement heard ends.
        if feedback::implicit(
            listening.entry(at.coordinator.clone()).or_default(),
            feed,
            pool,
            store,
            playback,
            cx.now,
        ) {
            // From the DJ's next pick on.
            *feedback = FeedbackModel::load(&StoreFeedback(&*store), cx.now).ok();
        }
        if !feed.is_active() {
            // The owner cleared or replaced the queue: the DJ steps aside.
            feeds.remove(at.coordinator);
            listening.remove(at.coordinator);
        }
    }

    fn feedback(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        signal: DjFeedback,
        clock: &dyn Clock,
    ) -> Result<DjFeedbackDto, Failure> {
        feedback::explicit(
            &mut self.state(),
            self.moods_file.as_ref(),
            at,
            store,
            signal,
            clock,
        )
    }

    fn preferences(&self) -> Result<PreferencesDto, Failure> {
        prefs::show(&mut self.state(), self.moods_file.as_ref())
    }

    fn prefer(&self, change: &PrefChange) -> Result<PreferredDto, Failure> {
        prefs::change(&mut self.state(), self.moods_file.as_ref(), change)
    }

    fn sync_library(&self) -> Result<LibrarySyncDto, Failure> {
        self.library()?.start()
    }

    fn library_sync(&self) -> Result<LibrarySyncDto, Failure> {
        Ok(self.library()?.status())
    }
}
