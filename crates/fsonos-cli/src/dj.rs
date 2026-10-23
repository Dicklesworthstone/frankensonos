//! The DJ on the speakers: one fsonos-spotify [`QueueFeed`] per group
//! coordinator, as the surfaces' [`DjEngine`].
//!
//! A start builds the DJ's pool from the library cache (songs and classical works)
//! and the owner's feedback, queues the first works and plays them. The pool
//! is kept for the playback events that follow. Each start rebuilds it, so a
//! library `fsonos setup` read in another process is picked up. A skip
//! rebuilds it only when a library sync, preferences.toml or moods.toml
//! changed since (the rebuild reads the whole library cache: seconds, for a
//! large library).
//! Automatic feeding also checks those files at most once every 30 seconds,
//! so external edits apply to future picks without a restart or skip.
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
//!
//! The daemon's DJ keeps its running feeds in the data directory
//! ([`saved`]), so a restarted serve feeds the queues it fed before.

use chrono::{Datelike, Timelike};
use fsonos_api::dj::{DjEngine, DjMoodsDto, DjSpeakers, DjStatusDto, DjSteer};
use fsonos_api::plan::DjAction;
use fsonos_api::surface::dj_feedback::{DjFeedback, DjFeedbackDto};
use fsonos_api::surface::dj_prefs::{PrefChange, PreferencesDto, PreferredDto};
use fsonos_api::surface::dj_rehearse::{DjRehearsal, DjRehearsalDto};
use fsonos_api::surface::dj_sync::LibrarySyncDto;
use fsonos_api::{ErrorCode, Failure, OutcomeDto};
use fsonos_core::clock::Clock;
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::store::{Action, LoggedAction, Store};
use fsonos_core::{CoreError, control};
use fsonos_spotify::cache::works_from_store;
use fsonos_spotify::dj::{DjConfig, PlayRecord, WorkPool};
use fsonos_spotify::feed::{FeedError, Planning, QueueFeed, QueuedWork, Speakers};
use fsonos_spotify::feedback::{FeedbackModel, StoreFeedback};
use fsonos_spotify::steer::{DjSession, Moods, Steer, steering};
use fsonos_types::PlayerId;
use std::collections::{HashMap, HashSet};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use crate::dj_view::{self, steer_failure, store_failure};

pub(crate) mod feedback;
mod handover;
#[cfg(test)]
mod plan_cache_tests;
pub(crate) mod prefs;
#[cfg(test)]
mod problem_tests;
mod rehearsal;
mod retry;
pub(crate) mod saved;
pub(crate) mod sync;

/// The moods file's name inside the data directory.
pub const MOODS_FILE: &str = "moods.toml";

/// Failed plan reads retry separately from speaker refill failures.
const PLAN_RETRY: Duration = Duration::from_secs(30);

/// Share one inexpensive input check across every room and playback event.
const PLAN_CHECK: Duration = Duration::from_secs(30);

/// See the module docs.
pub struct SpotifyDj {
    /// `moods.toml`, read on each start and checked on each skip (an edit
    /// rebuilds the pool); `None`: the built-ins.
    moods_file: Option<PathBuf>,
    state: Mutex<State>,
    /// Refreshes the library cache (the daemon's, with a Spotify app).
    library: Option<Arc<sync::LibrarySync>>,
    /// Where the running feeds are kept across restarts (the daemon's).
    feeds_file: Option<PathBuf>,
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
            feeds_file: None,
        }
    }

    /// Keep the running feeds in `path` across restarts ([`saved`]),
    /// taking up the feeds and their last known groups from one file revision.
    #[must_use]
    pub fn with_saved_feeds(mut self, path: Option<PathBuf>) -> Self {
        if let Some(path) = &path {
            let seed = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_secs());
            let mut state = self.state();
            let (feeds, handover) = saved::load(path, seed);
            state.feeds = feeds;
            state.handover = handover;
            state.saved = saved::encode(&state.feeds, &state.handover);
            if !state.feeds.is_empty() {
                tracing::info!(
                    feeds = state.feeds.len(),
                    "the DJ takes up the queues it fed"
                );
            }
        }
        self.feeds_file = path;
        self
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

    /// Save running feeds and their group evidence, if either changed.
    fn save(&self, state: &mut State) {
        let Some(path) = &self.feeds_file else {
            return;
        };
        let text = saved::encode(&state.feeds, &state.handover);
        if text == state.saved {
            return;
        }
        match saved::write(path, &text) {
            Ok(()) => state.saved = text,
            Err(e) => {
                tracing::warn!(error = %e, "the DJ's feeds weren't saved: a restart may forget them");
            }
        }
    }
}

#[derive(Default)]
struct State {
    feeds: HashMap<PlayerId, QueueFeed>,
    /// The groups whose queues the feeds owned before a coordinator changed.
    handover: handover::Tracker,
    /// Failed automatic top-ups, bounded independently for each group.
    refill_retries: HashMap<PlayerId, retry::Retry>,
    /// What the DJ last ran into feeding each group, until it feeds it again
    /// or is told to do something there: shown in the status.
    problems: HashMap<PlayerId, String>,
    /// What the DJ plans from, as of the last successful rebuild.
    pool: WorkPool,
    feedback: Option<FeedbackModel>,
    moods: Moods,
    /// What each group is hearing, for early skips and full listens.
    listening: HashMap<PlayerId, feedback::Listening>,
    /// Groups the music is moving away from: their playback is not acted on
    /// until the move settles (`DjEngine::moving`).
    held: HashSet<PlayerId>,
    /// The current library and preferences were rebuilt successfully.
    /// False keeps a failed refresh pending while the last good plan plays.
    planned: bool,
    /// What the plan was built from beyond the library cache: a hash of
    /// moods.toml and preferences.toml as they read then (see
    /// [`State::refresh`]).
    plan_inputs: Option<u64>,
    /// When automatic feeding next checks the owner's files for edits.
    plan_check_at: Option<Instant>,
    /// When a failed plan rebuild can retry, independent of speaker backoff.
    plan_retry_at: Option<Instant>,
    /// The feeds as last saved.
    saved: String,
    /// Action-log rows written since the surface last took them
    /// ([`DjEngine::take_logged`]).
    logged: Vec<LoggedAction>,
    /// Feeds carried to a new coordinator since the surface last took them
    /// ([`DjEngine::take_handovers`]).
    handed_over: Vec<(PlayerId, PlayerId)>,
}

/// What a pick is planned with right now, beyond the pool.
struct Context {
    steer: Option<Steer>,
    now: i64,
    local_hour: u8,
}

impl State {
    /// Rebuild the pool, feedback and moods without losing the last good
    /// preferences if any read or validation fails.
    fn reload(
        &mut self,
        store: &dyn Store,
        moods_file: Option<&PathBuf>,
        now: i64,
    ) -> Result<(), Failure> {
        let inputs = plan_inputs(moods_file);
        let candidate = (|| -> Result<Self, Failure> {
            // Complete partly-held works from cached album tracks, then
            // validate the owner's constraints before publishing the pool.
            let mut next = Self {
                pool: works_from_store(store).map_err(|e| {
                    Failure::new(ErrorCode::Internal, format!("the DJ's pool: {e}"))
                })?,
                // Feedback only weights the picks; without it the DJ still plays.
                feedback: FeedbackModel::load(&StoreFeedback(store), now).ok(),
                moods: read_moods(moods_file)?,
                ..Self::default()
            };
            prefs::apply(&mut next, moods_file)?;
            Ok(next)
        })();
        self.plan_check_at = Some(Instant::now() + PLAN_CHECK);
        match candidate {
            Ok(next) => {
                self.pool = next.pool;
                self.feedback = next.feedback;
                self.moods = next.moods;
                self.planned = true;
                self.plan_inputs = Some(inputs);
                self.plan_retry_at = None;
                self.refill_retries.clear();
                Ok(())
            }
            Err(error) => {
                // A consumed library-sync notification must not lose this
                // rebuild: maintenance retries it after the owner repairs it.
                self.planned = false;
                self.plan_retry_at = Some(Instant::now() + PLAN_RETRY);
                Err(error)
            }
        }
    }

    /// Bring the plan up to date for an explicit request. The pool is rebuilt only when
    /// what it is built from changed: a library sync (`planned` false), or an
    /// edit to moods.toml or preferences.toml. Otherwise it is kept and only
    /// the feedback, which moves with every listen, is read again. A rebuild
    /// reads the whole library cache under the DJ's lock: seconds, for a
    /// library of tens of thousands of tracks. A start always rebuilds.
    fn refresh(
        &mut self,
        store: &dyn Store,
        moods_file: Option<&PathBuf>,
        now: i64,
    ) -> Result<(), Failure> {
        if self.planned && self.plan_inputs == Some(plan_inputs(moods_file)) {
            // Feedback only weights the picks; without it the DJ still plays.
            self.feedback = FeedbackModel::load(&StoreFeedback(store), now).ok();
            self.plan_check_at = Some(Instant::now() + PLAN_CHECK);
            return Ok(());
        }
        self.reload(store, moods_file, now)
    }

    fn reload_due(&self, now: Instant) -> bool {
        !self.planned && self.plan_retry_at.is_none_or(|due| now >= due)
    }

    /// Invalidate a changed plan without re-reading files for every volume
    /// delta or rebuilding the library during the retry backoff.
    fn check_plan_inputs(&mut self, moods_file: Option<&PathBuf>, now: Instant) {
        if self.plan_check_at.is_some_and(|due| now < due) {
            return;
        }
        self.plan_check_at = Some(now + PLAN_CHECK);
        if self.planned && self.plan_inputs != Some(plan_inputs(moods_file)) {
            self.planned = false;
            self.plan_retry_at = None;
        }
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

/// Read without changing a live plan: moods and preferences are published
/// together only after both validate successfully.
fn read_moods(moods_file: Option<&PathBuf>) -> Result<Moods, Failure> {
    moods_file.map_or_else(
        || Ok(Moods::builtin()),
        |path| {
            Moods::load(path).map_err(|e| {
                Failure::invalid(e.to_string()).with_hint("Fix moods.toml in the data directory.")
            })
        },
    )
}

/// A hash of what the plan is built from beyond the library cache:
/// moods.toml and preferences.toml as they read now (an absent file hashes
/// as absent).
fn plan_inputs(moods_file: Option<&PathBuf>) -> u64 {
    let mut hasher = DefaultHasher::new();
    for path in [moods_file.cloned(), prefs::file(moods_file)] {
        path.and_then(|p| std::fs::read(p).ok()).hash(&mut hasher);
    }
    hasher.finish()
}

fn plan<'a>(
    pool: &'a WorkPool,
    feedback: Option<&'a FeedbackModel>,
    cx: &'a Context,
    queued_elsewhere: &'a [PlayRecord],
) -> Planning<'a> {
    Planning {
        pool,
        steer: cx.steer.as_ref(),
        feedback,
        now: cx.now,
        local_hour: Some(cx.local_hour),
        queued_elsewhere,
    }
}

/// What the house's other active feeds play now or have queued: a work
/// playing or waiting in another room is not picked here too.
fn queued_elsewhere(
    feeds: &HashMap<PlayerId, QueueFeed>,
    here: &PlayerId,
    now: i64,
) -> Vec<PlayRecord> {
    feeds
        .iter()
        .filter(|(coordinator, feed)| *coordinator != here && feed.is_active())
        .flat_map(|(_, feed)| feed.on_or_ahead(now))
        .collect()
}

/// Act on what a feed made of playback in the group `at` leads: clear or
/// set the retry backoff and what the DJ ran into (its status shows it).
/// Answers whether the listening may still teach the DJ: not once it let
/// the queue go over an add it couldn't confirm.
fn settle(
    result: Result<usize, FeedError>,
    trim_error: Option<String>,
    at: &PlayerId,
    playback: &PlayerPlayback,
    refill_retries: &mut HashMap<PlayerId, retry::Retry>,
    problems: &mut HashMap<PlayerId, String>,
) -> bool {
    let mut observe_listening = true;
    match result {
        Ok(queued) => {
            refill_retries.remove(at);
            problems.remove(at);
            if queued > 0 {
                tracing::info!(queued, "the DJ topped up the queue");
            }
        }
        Err(FeedError::UncertainEnqueue(detail)) => {
            // Relinquishing a queue is not a listener skipping the music.
            // Do not learn negative feedback or schedule another add.
            observe_listening = false;
            tracing::error!(%detail, "DJ feeding stopped after an indeterminate enqueue");
            problems.insert(
                at.clone(),
                format!(
                    "The DJ stopped feeding the queue: it couldn't confirm an add ({detail}), \
                     so it left the queue as it is"
                ),
            );
        }
        Err(e) => {
            let retry = retry::Retry::failed(refill_retries.get(at), playback, Instant::now());
            tracing::warn!(
                error = %e,
                retry_in_s = retry.delay().as_secs(),
                "the DJ could not top up; retrying with backoff"
            );
            problems.insert(
                at.clone(),
                format!(
                    "The DJ couldn't top up the queue ({e}); it tries again in {} s",
                    retry.delay().as_secs()
                ),
            );
            refill_retries.insert(at.clone(), retry);
        }
    }
    if let Some(error) = trim_error {
        tracing::warn!(%error, "the DJ could not trim the queue; retrying on the next track");
        problems.insert(
            at.clone(),
            format!(
                "The DJ couldn't take played works off the queue ({error}); it tries again at \
                 the next track"
            ),
        );
    }
    observe_listening
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

/// Log a top-up as the DJ's own action (client and surface `dj`, not
/// undoable), so `fsonos log` and the event stream show what it queued
/// where. A log that cannot be written never stops the feed.
fn record_top_up(
    logged: &mut Vec<LoggedAction>,
    store: &mut dyn Store,
    at: DjSpeakers<'_>,
    feed: &QueueFeed,
    result: &Result<usize, FeedError>,
    now: i64,
) {
    let &Ok(queued @ 1..) = result else {
        return;
    };
    let room = at
        .households
        .iter()
        .find_map(|h| h.player(at.coordinator))
        .map_or_else(|| at.coordinator.0.clone(), |p| p.room_name.clone());
    let works = feed.queued();
    let added: Vec<String> = works[works.len().saturating_sub(queued)..]
        .iter()
        .map(describe)
        .collect();
    let action = Action {
        at: now,
        client: "dj".into(),
        surface: "dj".into(),
        intent: format!("dj: top up {room}'s queue"),
        decision: "allow".into(),
        result: format!("queued {}", added.join("; ")),
        before_state: None,
        undo_of: None,
    };
    match fsonos_core::actions::record(store, &action) {
        Ok(id) => logged.push(LoggedAction { id, action }),
        Err(e) => tracing::warn!(error = %e, "the DJ's top-up was not logged"),
    }
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
        FeedError::NothingAllowed(_) => Failure::new(ErrorCode::DjPoolEmpty, detail).with_hint(
            "fsonos dj prefs show lists the bans and the explicit setting; fsonos dj prefs unset \
             ban.artists (or ban.albums, ban.tracks) or fsonos dj prefs set explicit true lets \
             works back in.",
        ),
        FeedError::NoRenderParams => Failure::new(ErrorCode::RenderParamsMissing, detail),
        FeedError::Inactive => Failure::new(ErrorCode::NoDjSession, detail),
        FeedError::UncertainEnqueue(_) => Failure::new(ErrorCode::OutcomeUncertain, detail).with_hint(
            "Inspect the speaker's queue before starting the DJ again. The add was not \
             replayed, and existing playback was left alone.",
        ),
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

impl SpotifyDj {
    /// A library sync since the last look makes the plan due for a rebuild.
    fn take_library_fresh(&self, state: &mut State) {
        if self.library.as_ref().is_some_and(|l| l.take_fresh()) {
            state.planned = false;
            state.plan_retry_at = None;
        }
    }

    /// A start, skip or stop (see [`DjEngine::act`]).
    fn carry_out(
        &self,
        state: &mut State,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        action: DjAction,
        clock: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        let room = room(at);
        let speakers = speakers(at);
        let now = clock.now().timestamp();
        match action {
            DjAction::Start => {
                // Always: a library written by another process (`fsonos
                // setup`) raises no notice here.
                self.take_library_fresh(state);
                state.reload(store, self.moods_file.as_ref(), now)?;
                let cx = state.context(store, at.coordinator, clock)?;
                let State {
                    feeds,
                    pool,
                    feedback,
                    ..
                } = &mut *state;
                let elsewhere = queued_elsewhere(feeds, at.coordinator, cx.now);
                let feed = feeds.entry(at.coordinator.clone()).or_insert_with(|| {
                    QueueFeed::new(at.coordinator, DjConfig::default(), now.unsigned_abs())
                });
                let planning = plan(pool, feedback.as_ref(), &cx, &elsewhere);
                let queued = match feed.start(&speakers, planning, store) {
                    Ok(queued) => queued,
                    Err(e) => {
                        // A start that queued nothing let go: no feed is
                        // left to retry (and save) until a pick is possible.
                        if !feed.is_active() {
                            feeds.remove(at.coordinator);
                        }
                        return Err(failure(e));
                    }
                };
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
                self.take_library_fresh(state);
                state.refresh(store, self.moods_file.as_ref(), now)?;
                let cx = state.context(store, at.coordinator, clock)?;
                let State {
                    feeds,
                    pool,
                    feedback,
                    ..
                } = &mut *state;
                let elsewhere = queued_elsewhere(feeds, at.coordinator, cx.now);
                let feed = feeds
                    .get_mut(at.coordinator)
                    .ok_or_else(|| no_session(&room))?;
                let work = feed
                    .skip(
                        &speakers,
                        plan(pool, feedback.as_ref(), &cx, &elsewhere),
                        store,
                    )
                    .map_err(failure)?;
                Ok(sent(format!("skipped to {}", describe(work))))
            }
            DjAction::Stop => {
                let feed = state
                    .feeds
                    .get_mut(at.coordinator)
                    .filter(|feed| feed.is_active())
                    .ok_or_else(|| no_session(&room))?;
                // A failed stop must remain controllable and recoverable,
                // including in the saved feeds after a daemon restart.
                feed.stop(&speakers).map_err(failure)?;
                state.feeds.remove(at.coordinator);
                state.listening.remove(at.coordinator);
                Ok(sent(format!("the DJ stopped in {room}'s group")))
            }
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
        let mut state = self.state();
        // Explicit commands are never held behind automatic retry backoff.
        state.refill_retries.remove(at.coordinator);
        state.problems.remove(at.coordinator);
        let done = self.carry_out(&mut state, at, store, action, clock);
        state.handover.remember(at.households, at.coordinator);
        // Even a failed start can leave a feed running, part-queued.
        self.save(&mut state);
        done
    }

    fn steer(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        steer_by: &DjSteer,
        clock: &dyn Clock,
    ) -> Result<OutcomeDto, Failure> {
        let mut state = self.state();
        if matches!(steer_by, DjSteer::Set { .. }) {
            self.take_library_fresh(&mut state);
            // A session must resolve against the complete accepted plan, not a
            // newly added mood whose preferences failed validation. Clearing a
            // session remains available even while the owner repairs an edit.
            state.refresh(store, self.moods_file.as_ref(), clock.now().timestamp())?;
        }
        let done = steer(&state, at, store, steer_by, clock);
        if done.is_ok() {
            state.refill_retries.remove(at.coordinator);
        }
        done
    }

    fn status(
        &self,
        at: DjSpeakers<'_>,
        store: &dyn Store,
        queue_position: Option<u32>,
        clock: &dyn Clock,
    ) -> Result<DjStatusDto, Failure> {
        let state = self.state();
        // Status is published after every live event. Use the validated
        // plan once one exists, including during an invalid edit's retry,
        // rather than re-reading files or reporting unaccepted programs.
        let initial;
        let moods = if state.plan_inputs.is_some() {
            &state.moods
        } else {
            initial = read_moods(self.moods_file.as_ref())?;
            &initial
        };
        let steering = dj_view::steering_of(store, moods, at.coordinator, clock)?;
        Ok(dj_view::status(
            room(at),
            state.feeds.get(at.coordinator),
            &state.pool,
            queue_position,
            steering,
            state.problems.get(at.coordinator).cloned(),
        ))
    }

    fn rehearse(
        &self,
        at: DjSpeakers<'_>,
        store: &dyn Store,
        rehearsal: &DjRehearsal,
        clock: &dyn Clock,
    ) -> Result<DjRehearsalDto, Failure> {
        let mut state = self.state();
        // The same current plan as a real skip, without advancing a feed,
        // changing a speaker or recording any hypothetical listening.
        self.take_library_fresh(&mut state);
        state.refresh(store, self.moods_file.as_ref(), clock.now().timestamp())?;
        rehearsal::rehearse(&state, at, store, rehearsal, clock)
    }

    fn moods(
        &self,
        at: Option<DjSpeakers<'_>>,
        store: &dyn Store,
        clock: &dyn Clock,
    ) -> Result<DjMoodsDto, Failure> {
        let moods = read_moods(self.moods_file.as_ref())?;
        let now = match at {
            Some(at) => dj_view::steering_of(store, &moods, at.coordinator, clock)?,
            None => dj_view::program_now(&moods, clock),
        };
        Ok(dj_view::moods_of(&moods, now))
    }

    fn until_program_changes(&self, clock: &dyn Clock) -> Option<u64> {
        let moods = read_moods(self.moods_file.as_ref()).ok()?;
        let now = clock.now();
        let minutes = moods.minutes_until_program_changes(now.weekday(), now.time())?;
        // Counted from the minute's start; never under the minute a mood
        // must last.
        Some(
            (u64::from(minutes) * 60)
                .saturating_sub(u64::from(now.second()))
                .max(60),
        )
    }

    fn feeds(&self, coordinator: &PlayerId) -> bool {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .feeds
            .get(coordinator)
            .is_some_and(QueueFeed::is_active)
    }

    fn take_logged(&self) -> Vec<LoggedAction> {
        std::mem::take(&mut self.state().logged)
    }

    fn take_handovers(&self) -> Vec<(PlayerId, PlayerId)> {
        std::mem::take(&mut self.state().handed_over)
    }

    fn moving(&self, from: &PlayerId) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.held.insert(from.clone());
    }

    fn moved(&self, from: &PlayerId, to: &PlayerId) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        state.refill_retries.remove(from);
        state.refill_retries.remove(to);
        let State {
            feeds,
            listening,
            held,
            problems,
            ..
        } = &mut *state;
        held.remove(from);
        if from == to {
            return;
        }
        if let Some(mut feed) = feeds.remove(from) {
            feed.rekey(to);
            feeds.insert(to.clone(), feed);
        }
        if let Some(problem) = problems.remove(from) {
            problems.insert(to.clone(), problem);
        }
        if let Some(heard) = listening.remove(from) {
            listening.insert(to.clone(), heard);
        }
        self.save(&mut state);
    }

    fn on_topology(
        &self,
        transport: &dyn fsonos_proto::Transport,
        snapshot: &fsonos_core::live::Snapshot,
        store: &mut dyn Store,
    ) {
        self.reconcile_topology(transport, snapshot, store);
    }

    fn on_playback(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        playback: &PlayerPlayback,
        clock: &dyn Clock,
    ) {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        if !state.feeds.contains_key(at.coordinator) || state.held.contains(at.coordinator) {
            return;
        }
        // Keep a failed rebuild pending even after consuming the library's
        // freshness notification. A repaired plan can unblock a failed feed.
        self.take_library_fresh(&mut state);
        let checked_at = Instant::now();
        state.check_plan_inputs(self.moods_file.as_ref(), checked_at);
        if state.reload_due(checked_at)
            && let Err(f) = state.reload(store, self.moods_file.as_ref(), clock.now().timestamp())
        {
            tracing::warn!(detail = %f.detail, "the DJ retains its last validated plan; rebuild pending");
        }
        if state
            .refill_retries
            .get(at.coordinator)
            .is_some_and(|retry| !retry.ready(playback, Instant::now()))
        {
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
            refill_retries,
            pool,
            feedback,
            listening,
            problems,
            logged,
            ..
        } = &mut *state;
        let elsewhere = queued_elsewhere(feeds, at.coordinator, cx.now);
        let Some(feed) = feeds.get_mut(at.coordinator) else {
            return;
        };
        let result = feed.on_playback(
            &speakers(at),
            plan(pool, feedback.as_ref(), &cx, &elsewhere),
            store,
            playback,
        );
        record_top_up(logged, store, at, feed, &result, cx.now);
        let trim_error = feed.take_trim_error();
        let observe_listening = settle(
            result,
            trim_error,
            at.coordinator,
            playback,
            refill_retries,
            problems,
        );
        // An early skip or a full listen, as the movement heard ends.
        if observe_listening
            && feedback::implicit(
                listening.entry(at.coordinator.clone()).or_default(),
                feed,
                pool,
                store,
                playback,
                clock.elapsed_secs(),
                cx.now,
            )
        {
            // From the DJ's next pick on.
            *feedback = FeedbackModel::load(&StoreFeedback(&*store), cx.now).ok();
        }
        if !feed.is_active() {
            // The owner cleared or replaced the queue: the DJ steps aside.
            problems.entry(at.coordinator.clone()).or_insert_with(|| {
                "The DJ stepped aside: the queue was cleared or replaced".to_owned()
            });
            feeds.remove(at.coordinator);
            refill_retries.remove(at.coordinator);
            listening.remove(at.coordinator);
        }
        self.save(&mut state);
    }

    fn feedback(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        signal: DjFeedback,
        clock: &dyn Clock,
    ) -> Result<DjFeedbackDto, Failure> {
        let mut state = self.state();
        self.take_library_fresh(&mut state);
        feedback::explicit(
            &mut state,
            self.moods_file.as_ref(),
            at,
            store,
            signal,
            clock,
        )
    }

    fn preferences(&self) -> Result<PreferencesDto, Failure> {
        prefs::show(self.moods_file.as_ref())
    }

    fn prefer(&self, change: &PrefChange) -> Result<PreferredDto, Failure> {
        let mut state = self.state();
        let done = prefs::change(&mut state, self.moods_file.as_ref(), change);
        if done.is_ok() {
            state.refill_retries.clear();
            state.plan_retry_at = None;
        }
        done
    }

    fn sync_library(&self) -> Result<LibrarySyncDto, Failure> {
        self.library()?.start()
    }

    fn library_sync(&self) -> Result<LibrarySyncDto, Failure> {
        Ok(self.library()?.status())
    }
}
