//! The DJ as the surfaces see it. A [`DjEngine`] runs it on the speakers
//! (the daemon's, over fsonos-spotify's queue feed; installed with
//! [`crate::Surface::with_dj`]), and [`crate::surface::follow`] feeds it each
//! coordinator's playback from the live model, which is what keeps a DJ queue
//! topped up. Without the live model a start queues the first works and
//! plays them, and nothing tops the queue up.
//!
//! Steering is one stored session per group coordinator: a mood and
//! constraints, with an optional end. A steer replaces it and a clear deletes
//! it (the DJ then follows the time-of-day programs in `moods.toml`). It
//! applies from the DJ's next pick, whether or not the DJ is running. The
//! daemon never parses language: agents turn the owner's words into a mood
//! and [`SteerConstraints`].

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::HouseholdState;
use fsonos_core::clock::Clock;
use fsonos_core::playback::PlayerPlayback;
use fsonos_core::store::Store;
use fsonos_proto::Transport;
use fsonos_types::PlayerId;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use crate::execute::OutcomeDto;
use crate::failure::{ErrorCode, Failure};
use crate::plan::DjAction;
use crate::surface::dj_feedback::{DjFeedback, DjFeedbackDto};
use crate::surface::dj_prefs::{PrefChange, PreferencesDto, PreferredDto};
use crate::surface::dj_sync::LibrarySyncDto;

/// The speakers a DJ command acts on.
#[derive(Clone, Copy)]
pub struct DjSpeakers<'a> {
    pub transport: &'a dyn Transport,
    pub households: &'a [HouseholdState],
    /// The coordinator of the group the DJ feeds.
    pub coordinator: &'a PlayerId,
}

/// Runs the DJ; see the module docs.
pub trait DjEngine: Send + Sync {
    /// Start, skip or stop the DJ in the group `at.coordinator` leads.
    /// `clock` is the house's: its local day and time pick the time-of-day
    /// program and the energy target.
    fn act(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        action: DjAction,
        clock: &dyn Clock,
    ) -> Result<OutcomeDto, Failure>;

    /// Replace or clear the steering of the group `at.coordinator` leads (its
    /// stored session). An unknown mood is `UNKNOWN_MOOD`; constraints the DJ
    /// can't honor are `INVALID_ARGUMENT`; clearing when there is nothing to
    /// clear changes nothing.
    fn steer(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        steer: &DjSteer,
        clock: &dyn Clock,
    ) -> Result<OutcomeDto, Failure>;

    /// What the DJ plays in the group `at.coordinator` leads, why, what comes
    /// next, and how its next pick is steered. `queue_position` is the
    /// group's current queue position (1-based), which places the movement.
    fn status(
        &self,
        at: DjSpeakers<'_>,
        store: &dyn Store,
        queue_position: Option<u32>,
        clock: &dyn Clock,
    ) -> Result<DjStatusDto, Failure>;

    /// Every mood and time-of-day program, and the steering in effect now:
    /// in the group `at` names, else the house's program.
    fn moods(
        &self,
        at: Option<DjSpeakers<'_>>,
        store: &dyn Store,
        clock: &dyn Clock,
    ) -> Result<DjMoodsDto, Failure>;

    /// Whether the DJ is feeding `coordinator`'s queue.
    fn feeds(&self, coordinator: &PlayerId) -> bool;

    /// The music moved: the group `from` led plays on under `to` now (a
    /// `move` handed it over or replayed it there). The engine carries what
    /// it keeps per group, its feed, across; the stored session is the
    /// surface's to re-key.
    fn moved(&self, from: &PlayerId, to: &PlayerId) {
        let _ = (from, to);
    }

    /// Fold `playback`, the coordinator's latest state, into its feed: record
    /// what plays and top the queue up. Failures are the engine's to log; the
    /// next playback change retries.
    fn on_playback(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        playback: &PlayerPlayback,
        clock: &dyn Clock,
    );

    /// Record the owner's like or dislike of the work playing in the
    /// group `at.coordinator` leads (its composer and performer too).
    fn feedback(
        &self,
        at: DjSpeakers<'_>,
        store: &mut dyn Store,
        signal: DjFeedback,
        clock: &dyn Clock,
    ) -> Result<DjFeedbackDto, Failure> {
        let _ = (at, store, signal, clock);
        Err(Failure::new(
            ErrorCode::NotImplemented,
            "this DJ takes no feedback",
        ))
    }

    /// The owner's standing preferences ([`crate::surface::dj_prefs`]).
    fn preferences(&self) -> Result<PreferencesDto, Failure> {
        Err(Failure::new(
            ErrorCode::NotImplemented,
            "this DJ keeps no preferences",
        ))
    }

    /// Set or unset one standing preference; it applies from the next pick.
    fn prefer(&self, change: &PrefChange) -> Result<PreferredDto, Failure> {
        let _ = change;
        Err(Failure::new(
            ErrorCode::NotImplemented,
            "this DJ keeps no preferences",
        ))
    }

    /// Start refreshing the library cache from Spotify, in the
    /// background, unless a refresh is running
    /// ([`crate::surface::dj_sync`]); where it stands.
    fn sync_library(&self) -> Result<LibrarySyncDto, Failure> {
        Err(Failure::new(
            ErrorCode::NotImplemented,
            "this DJ has no library to refresh",
        ))
    }

    /// Where the library refresh stands.
    fn library_sync(&self) -> Result<LibrarySyncDto, Failure> {
        Err(Failure::new(
            ErrorCode::NotImplemented,
            "this DJ has no library to refresh",
        ))
    }
}

/// Hard filters (and one nudge) on what the DJ may pick: fsonos-spotify's
/// `DjConstraints` field for field, without the expiry a steer sets from
/// `for_secs`. Every field is optional; the default constrains nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
pub struct SteerConstraints {
    /// Only these composers ("Bach", "J.S. Bach", "Saint-Saëns").
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include_composers: Vec<String>,
    /// Never these composers.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_composers: Vec<String>,
    /// Only works by these artists: any credited artist, a classical work's
    /// performers included ("Miles Davis", "The Beatles", "Yo-Yo Ma").
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include_artists: Vec<String>,
    /// Nothing by these artists.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_artists: Vec<String>,
    /// Only works tagged with one of these genres, as the library read
    /// tags them ("jazz" finds "cool jazz"; every classical work is
    /// "classical").
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include_genres: Vec<String>,
    /// Nothing tagged with these genres.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_genres: Vec<String>,
    /// Only these periods (classical works only: a song has none):
    /// medieval, renaissance, baroque, classical, romantic, late_romantic,
    /// impressionist, modern, contemporary.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub periods: Vec<String>,
    /// Only works released in these decades, each named by its first year
    /// (1960 for the sixties).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub decades: Vec<u16>,
    /// At least one of these in the work's title, movements, album or
    /// artists. Categories expand (piano, chamber, orchestral, choral,
    /// opera, song, vocal); other words match whole words.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub include_keywords: Vec<String>,
    /// None of these.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub exclude_keywords: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub min_work_minutes: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_work_minutes: Option<u32>,
    /// Shift the energy: -2 (much calmer) ..= 2 (much brighter).
    pub energy_bias: i8,
    /// Let works past the long-work limit (operas, Passions) in.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub allow_long_works: bool,
}

impl SteerConstraints {
    /// Whether they constrain nothing.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// What a steer does to a zone's DJ session.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(clippy::large_enum_variant)] // one per request, never kept in bulk
pub enum DjSteer {
    /// Replace the session: a mood (lowercase) and constraints laid over
    /// it, lapsing `for_secs` from now (`None`: until cleared).
    Set {
        mood: Option<String>,
        constraints: SteerConstraints,
        for_secs: Option<u64>,
    },
    /// Delete the session: the DJ follows the time-of-day program.
    Clear,
}

/// `GET /zones/{room}/dj` / the `dj_status` tool / `fsonos dj status`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DjStatusDto {
    /// The zone: its coordinator's room.
    pub zone: String,
    /// Whether the DJ feeds the zone's queue.
    pub running: bool,
    /// The DJ's work playing now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub now: Option<DjWorkDto>,
    /// The DJ's works queued after it, at most two.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub next: Vec<DjWorkDto>,
    /// How the DJ's next pick is steered.
    pub steering: DjSteeringDto,
}

/// One of the DJ's works: a song, or every movement of one classical
/// recording.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DjWorkDto {
    /// The composer, or a song's lead artist.
    pub composer: String,
    pub title: String,
    /// The recording's artists other than the composer.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub performers: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub album: Option<String>,
    /// How many movements it has.
    pub movements: u32,
    /// The movement playing (1-based), on the work playing now.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub movement: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub movement_title: Option<String>,
    /// Its length, when its movements' lengths are known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub minutes: Option<u32>,
    /// Why the DJ chose it.
    pub reason: DjReasonDto,
}

/// Why the DJ chose a work.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DjReasonDto {
    /// One line, e.g. "Brahms not heard in 4 days; balancing toward
    /// late-Romantic; gentle evening target".
    pub summary: String,
    /// Every factor that moved its odds, in the order the DJ weighed them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub factors: Vec<DjFactorDto>,
    /// Steering filters dropped because too few works passed them, in the
    /// order they relaxed: keyword, period, length, composer, artist.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub relaxed: Vec<String>,
}

/// One factor of a pick.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DjFactorDto {
    /// e.g. `composer_spacing`, `energy_fit`, `feedback`.
    pub factor: String,
    /// Per mille: 1000 is neutral, above favors the work, below disfavors
    /// it.
    pub weight: i32,
}

/// How the DJ's next pick in a zone is steered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DjSteeringDto {
    /// `session` (set with dj_steer), `program` (the time-of-day program in
    /// moods.toml) or `none`.
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mood: Option<String>,
    /// The constraints in effect: the mood's, with the session's on top.
    #[serde(default, skip_serializing_if = "SteerConstraints::is_empty")]
    pub constraints: SteerConstraints,
    /// What a session adds to its mood: its own constraints (empty for the
    /// program). The summary names the mood and these; the mood's own
    /// filters come with its name.
    #[serde(default, skip_serializing_if = "SteerConstraints::is_empty")]
    pub added: SteerConstraints,
    /// When the session lapses (unix seconds); the program takes over then.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<i64>,
    /// How long until then, in seconds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_in_secs: Option<u64>,
}

/// `GET /dj/moods` / the `dj_moods` tool / `fsonos dj moods`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DjMoodsDto {
    /// Built-in and moods.toml moods, by name.
    pub moods: Vec<DjMoodDto>,
    /// The time-of-day programs: the mood a zone plays when its session
    /// names none (the first that covers the time).
    pub programs: Vec<DjProgramDto>,
    /// The steering in effect now.
    pub now: DjSteeringDto,
}

/// A named mood.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DjMoodDto {
    pub name: String,
    pub constraints: SteerConstraints,
}

impl DjStatusDto {
    /// The status as a few sentences: what plays and why, what comes next,
    /// and the steering.
    #[must_use]
    pub fn summary(&self) -> String {
        let zone = &self.zone;
        let mut text = match (&self.now, self.running) {
            (_, false) => format!("The DJ isn't running in {zone}'s group."),
            (None, true) => format!("The DJ runs in {zone}'s group; none of its works plays now."),
            (Some(work), true) => {
                let movement = match (work.movement, &work.movement_title) {
                    (Some(m), Some(title)) => {
                        format!(", movement {m} of {}: {title}", work.movements)
                    }
                    _ => String::new(),
                };
                format!(
                    "The DJ in {zone}'s group plays {}{movement}. Why: {}.",
                    work.line(),
                    work.reason.summary
                )
            }
        };
        if !self.next.is_empty() {
            let next: Vec<String> = self.next.iter().map(DjWorkDto::line).collect();
            let _ = write!(text, " Next: {}.", next.join("; "));
        }
        let _ = write!(text, " Steering: {}.", self.steering.summary());
        text
    }
}

impl DjWorkDto {
    /// "Brahms: Symphony No. 4 (Sim Ensemble; 40 min)".
    #[must_use]
    pub fn line(&self) -> String {
        let mut about: Vec<String> = Vec::new();
        if !self.performers.is_empty() {
            about.push(self.performers.join(", "));
        }
        if let Some(minutes) = self.minutes {
            about.push(format!("{minutes} min"));
        }
        if about.is_empty() {
            format!("{}: {}", self.composer, self.title)
        } else {
            format!("{}: {} ({})", self.composer, self.title, about.join("; "))
        }
    }
}

impl DjSteeringDto {
    /// "steered: focus mood, Bach only, for another 2 hours", "the
    /// time-of-day program: calm mood", or "none". A mood is named with
    /// only what a session adds to it, not its own filters.
    #[must_use]
    pub fn summary(&self) -> String {
        let shown = if self.mood.is_some() {
            &self.added
        } else {
            &self.constraints
        };
        let steering = describe_steer(self.mood.as_deref(), shown, None);
        match self.source.as_str() {
            "session" => match self.expires_in_secs {
                Some(secs) => format!("steered: {steering}, for another {}", span(secs)),
                None => format!("steered: {steering}, until cleared"),
            },
            "program" => format!("the time-of-day program: {steering}"),
            _ => "none".to_owned(),
        }
    }
}

impl DjMoodsDto {
    /// Every mood and program in a few lines, and the steering now.
    #[must_use]
    pub fn summary(&self) -> String {
        let moods: Vec<String> = self
            .moods
            .iter()
            .map(
                |m| match describe_steer(None, &m.constraints, None).as_str() {
                    "no steering" => m.name.clone(),
                    about => format!("{} ({about})", m.name),
                },
            )
            .collect();
        let programs: Vec<String> = self
            .programs
            .iter()
            .map(|p| format!("{} {}–{} {}", p.days.join(","), p.from, p.to, p.mood))
            .collect();
        let mut text = format!("Moods: {}.", moods.join("; "));
        if !programs.is_empty() {
            let _ = write!(text, " Programs: {}.", programs.join("; "));
        }
        let _ = write!(text, " Now: {}.", self.now.summary());
        text
    }
}

/// A steer in words: "focus mood, Bach only, without vocal, calmer, for 2
/// hours" ("no steering" when it names nothing).
#[must_use]
pub fn describe_steer(mood: Option<&str>, c: &SteerConstraints, for_secs: Option<u64>) -> String {
    let mut parts: Vec<String> = Vec::new();
    if let Some(mood) = mood {
        parts.push(format!("{mood} mood"));
    }
    let mut list = |items: &[String], says: fn(&str) -> String| {
        if !items.is_empty() {
            parts.push(says(&items.join(" or ")));
        }
    };
    list(&c.include_composers, |l| format!("{l} only"));
    list(&c.exclude_composers, |l| format!("no {l}"));
    list(&c.include_artists, |l| format!("by {l}"));
    list(&c.exclude_artists, |l| format!("nothing by {l}"));
    list(&c.include_genres, |l| format!("{l} only"));
    list(&c.exclude_genres, |l| format!("no {l}"));
    list(&c.periods, |l| format!("{} works", l.replace('_', "-")));
    let decades: Vec<String> = c.decades.iter().map(|d| format!("the {d}s")).collect();
    list(&decades, |l| format!("from {l}"));
    list(&c.include_keywords, |l| format!("with {l}"));
    list(&c.exclude_keywords, |l| format!("without {l}"));
    match (c.min_work_minutes, c.max_work_minutes) {
        (Some(min), Some(max)) => parts.push(format!("works of {min} to {max} minutes")),
        (Some(min), None) => parts.push(format!("works of {min} minutes or more")),
        (None, Some(max)) => parts.push(format!("works up to {max} minutes")),
        (None, None) => {}
    }
    let energy = match c.energy_bias {
        i8::MIN..=-2 => "much calmer",
        -1 => "calmer",
        0 => "",
        1 => "brighter",
        2..=i8::MAX => "much brighter",
    };
    if !energy.is_empty() {
        parts.push(energy.to_owned());
    }
    if c.allow_long_works {
        parts.push("long works allowed".to_owned());
    }
    if let Some(secs) = for_secs {
        parts.push(format!("for {}", span(secs)));
    }
    if parts.is_empty() {
        "no steering".to_owned()
    } else {
        parts.join(", ")
    }
}

/// "2 hours", "90 minutes", "1 day" (whole minutes, rounded up).
#[must_use]
pub fn span(secs: u64) -> String {
    let minutes = secs.div_ceil(60);
    let count = |n: u64, unit: &str| {
        if n == 1 {
            format!("1 {unit}")
        } else {
            format!("{n} {unit}s")
        }
    };
    if minutes > 0 && minutes.is_multiple_of(24 * 60) {
        count(minutes / (24 * 60), "day")
    } else if minutes > 0 && minutes.is_multiple_of(60) {
        count(minutes / 60, "hour")
    } else {
        count(minutes, "minute")
    }
}

/// A time-of-day program.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct DjProgramDto {
    pub mood: String,
    /// `mon` … `sun`.
    pub days: Vec<String>,
    /// `HH:MM`, local time; a window ending before it starts runs past
    /// midnight.
    pub from: String,
    pub to: String,
}
