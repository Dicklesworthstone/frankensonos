//! Steering the DJ: structured constraints, named moods, and the session
//! that carries them.
//!
//! People and agents ask for "something calmer", "no opera", "just piano for
//! the next two hours", "dinner music". The agent maps that language onto
//! [`DjConstraints`] or a named mood ([`Moods`]); the DJ applies them as hard
//! filters before weighting, so its behavior is predictable and testable.
//! When the filters leave too few works they relax in a fixed order —
//! keywords, then periods, then length — and an explicit composer or artist
//! request relaxes only if nothing at all matches; the pick's reason reports
//! every relaxation. The DJ never silently plays nothing. Composers and
//! periods steer the classical part of the library; artists, genres,
//! decades and keywords steer any genre.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;
use std::str::FromStr;

use chrono::{NaiveTime, Timelike, Weekday};
use serde::{Deserialize, Serialize};

use fsonos_core::store::{DjSession as StoredDjSession, Store};

use crate::SpotifyError;
use crate::classical::{Period, composer_matches, has_phrase, normalize};
use crate::library::split_artists;
use crate::works::Work;

/// Hard filters (and one nudge) on what the DJ may pick. Every field is
/// optional; the default constrains nothing.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DjConstraints {
    /// Only these composers ("Bach", "J.S. Bach", "Saint-Saëns", …).
    pub include_composers: Vec<String>,
    /// Never these composers.
    pub exclude_composers: Vec<String>,
    /// Only works crediting one of these artists ("Miles Davis", "Beatles",
    /// "Yo-Yo Ma"), in any genre; see [`artist_matches`].
    pub include_artists: Vec<String>,
    /// Never works crediting one of these artists.
    pub exclude_artists: Vec<String>,
    /// Only works tagged with one of these genres ("jazz", "hip hop",
    /// "classical"); see [`genre_matches`]. Works carry the genre tags the
    /// library read gave, and every classical work is "classical".
    pub include_genres: Vec<String>,
    /// Never works tagged with one of these genres.
    pub exclude_genres: Vec<String>,
    /// Only these periods (classical works).
    pub periods: Vec<Period>,
    /// Only works released in these decades, each named by its first year
    /// (`1960` for the sixties).
    pub decades: Vec<u16>,
    /// At least one of these in the work's title, movements, album or
    /// artists. Categories expand: "piano" also finds nocturnes, "vocal"
    /// finds opera, song and choral works (see [`keyword_matches`]).
    pub include_keywords: Vec<String>,
    /// None of these.
    pub exclude_keywords: Vec<String>,
    pub min_work_minutes: Option<u32>,
    pub max_work_minutes: Option<u32>,
    /// Shift the energy target: −2 (much calmer) ..= 2 (much brighter).
    pub energy_bias: i8,
    /// Let works past the long-work limit (operas, Passions) in.
    pub allow_long_works: bool,
    /// Unix seconds after which these constraints lapse.
    pub expires_at: Option<i64>,
}

/// Energy points per step of [`DjConstraints::energy_bias`].
const BIAS_STEP: i32 = 12;

impl DjConstraints {
    /// Whether they still apply at `now` (without a clock, they do).
    #[must_use]
    pub fn is_active(&self, now: Option<i64>) -> bool {
        match (self.expires_at, now) {
            (Some(expires), Some(now)) => now < expires,
            _ => true,
        }
    }

    /// `self` with `over` laid on top: lists combine, set values win, and
    /// the earlier expiry holds.
    #[must_use]
    pub fn merged(&self, over: &Self) -> Self {
        let union = |a: &[String], b: &[String]| {
            let mut all = a.to_vec();
            all.extend(b.iter().filter(|s| !a.contains(s)).cloned());
            all
        };
        Self {
            include_composers: union(&self.include_composers, &over.include_composers),
            exclude_composers: union(&self.exclude_composers, &over.exclude_composers),
            include_artists: union(&self.include_artists, &over.include_artists),
            exclude_artists: union(&self.exclude_artists, &over.exclude_artists),
            include_genres: union(&self.include_genres, &over.include_genres),
            exclude_genres: union(&self.exclude_genres, &over.exclude_genres),
            periods: if over.periods.is_empty() {
                self.periods.clone()
            } else {
                over.periods.clone()
            },
            decades: if over.decades.is_empty() {
                self.decades.clone()
            } else {
                over.decades.clone()
            },
            include_keywords: union(&self.include_keywords, &over.include_keywords),
            exclude_keywords: union(&self.exclude_keywords, &over.exclude_keywords),
            min_work_minutes: over.min_work_minutes.or(self.min_work_minutes),
            max_work_minutes: over.max_work_minutes.or(self.max_work_minutes),
            energy_bias: if over.energy_bias == 0 {
                self.energy_bias
            } else {
                over.energy_bias
            },
            allow_long_works: self.allow_long_works || over.allow_long_works,
            expires_at: match (self.expires_at, over.expires_at) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            },
        }
    }

    /// Reject values the DJ can't honor.
    pub fn validate(&self) -> Result<(), String> {
        if !(-2..=2).contains(&self.energy_bias) {
            return Err(format!(
                "energy_bias {} is outside -2..=2",
                self.energy_bias
            ));
        }
        if let (Some(min), Some(max)) = (self.min_work_minutes, self.max_work_minutes)
            && min > max
        {
            return Err(format!(
                "min_work_minutes {min} exceeds max_work_minutes {max}"
            ));
        }
        if let Some(decade) = self.decades.iter().find(|&&d| d % 10 != 0) {
            return Err(format!(
                "decade {decade} is not a decade's first year (say {})",
                decade - decade % 10
            ));
        }
        Ok(())
    }

    /// The energy target nudged by the bias (12 points a step), starting
    /// from mid-energy when there is no other target.
    #[must_use]
    pub fn biased_target(&self, target: Option<u8>) -> Option<u8> {
        if self.energy_bias == 0 {
            return target;
        }
        let base = i32::from(target.unwrap_or(50));
        let biased = (base + BIAS_STEP * i32::from(self.energy_bias.clamp(-2, 2))).clamp(5, 95);
        u8::try_from(biased).ok()
    }

    fn constrains(&self, family: Relaxation) -> bool {
        match family {
            Relaxation::Keywords => {
                !self.include_keywords.is_empty() || !self.exclude_keywords.is_empty()
            }
            Relaxation::Periods => !self.periods.is_empty(),
            Relaxation::Decades => !self.decades.is_empty(),
            Relaxation::Genres => {
                !self.include_genres.is_empty() || !self.exclude_genres.is_empty()
            }
            Relaxation::Length => {
                self.min_work_minutes.is_some() || self.max_work_minutes.is_some()
            }
            Relaxation::Composers => {
                !self.include_composers.is_empty() || !self.exclude_composers.is_empty()
            }
            Relaxation::Artists => {
                !self.include_artists.is_empty() || !self.exclude_artists.is_empty()
            }
        }
    }
}

/// A filter family the DJ dropped because too few works passed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Relaxation {
    Keywords,
    Periods,
    Length,
    Composers,
    Artists,
    Decades,
    Genres,
}

impl Relaxation {
    /// The order filters relax in.
    pub const ORDER: [Self; 7] = [
        Self::Keywords,
        Self::Periods,
        Self::Decades,
        Self::Length,
        Self::Genres,
        Self::Composers,
        Self::Artists,
    ];

    /// Families that relax only when nothing at all passes: the owner asked
    /// for this music by name, so a few matching works rotate instead.
    fn by_name(self) -> bool {
        matches!(self, Self::Genres | Self::Composers | Self::Artists)
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Keywords => "keyword",
            Self::Periods => "period",
            Self::Length => "length",
            Self::Composers => "composer",
            Self::Artists => "artist",
            Self::Decades => "decade",
            Self::Genres => "genre",
        }
    }
}

/// Active steering for a pick: the merged constraints, and the mood they
/// came from (for the reason line).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Steer {
    pub mood: Option<String>,
    pub constraints: DjConstraints,
}

/// The works the constraints admit, as indices into `works`. Keywords, then
/// periods, decades and length relax until at least `min` pass (or every
/// work, in a smaller pool); genres, composers and artists relax only when
/// nothing passes, so "just Pärt" rotates the few Pärt works rather than
/// ignoring the request.
/// `haystacks[i]` is [`haystack`]`(&works[i])`.
#[must_use]
pub fn admit(
    works: &[Work],
    haystacks: &[String],
    constraints: &DjConstraints,
    min: usize,
) -> (Vec<usize>, Vec<Relaxation>) {
    let all: Vec<usize> = (0..works.len()).collect();
    admit_among(works, haystacks, &all, constraints, min)
}

/// [`admit`] among some of the works (indices into `works`): those the
/// owner's preferences allow.
#[must_use]
pub fn admit_among(
    works: &[Work],
    haystacks: &[String],
    among: &[usize],
    constraints: &DjConstraints,
    min: usize,
) -> (Vec<usize>, Vec<Relaxation>) {
    if among.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let want = min.clamp(1, among.len());
    let mut relaxed = Vec::new();
    loop {
        let admitted: Vec<usize> = among
            .iter()
            .copied()
            .filter(|&w| admits(&works[w], &haystacks[w], constraints, &relaxed))
            .collect();
        if admitted.len() >= want {
            return (admitted, relaxed);
        }
        let next = Relaxation::ORDER.into_iter().find(|&r| {
            !relaxed.contains(&r)
                && constraints.constrains(r)
                && (!r.by_name() || admitted.is_empty())
        });
        match next {
            Some(family) => relaxed.push(family),
            None => return (admitted, relaxed),
        }
    }
}

/// Whether one work passes every filter family not yet relaxed.
#[must_use]
pub fn admits(
    work: &Work,
    haystack: &str,
    constraints: &DjConstraints,
    relaxed: &[Relaxation],
) -> bool {
    let on = |family| !relaxed.contains(&family);
    let c = constraints;
    if on(Relaxation::Composers) {
        if !c.include_composers.is_empty()
            && !c
                .include_composers
                .iter()
                .any(|q| composer_matches(q, &work.composer))
        {
            return false;
        }
        if c.exclude_composers
            .iter()
            .any(|q| composer_matches(q, &work.composer))
        {
            return false;
        }
    }
    if on(Relaxation::Artists) {
        let credits = credited(work);
        let credits_any = |queries: &[String]| {
            queries
                .iter()
                .any(|q| credits.iter().any(|a| artist_matches(q, a)))
        };
        if !c.include_artists.is_empty() && !credits_any(&c.include_artists) {
            return false;
        }
        if credits_any(&c.exclude_artists) {
            return false;
        }
    }
    if on(Relaxation::Genres) {
        let tags = work.genres();
        let tagged_any = |queries: &[String]| {
            queries
                .iter()
                .any(|q| tags.iter().any(|t| genre_matches(q, t)))
        };
        if !c.include_genres.is_empty() && !tagged_any(&c.include_genres) {
            return false;
        }
        if tagged_any(&c.exclude_genres) {
            return false;
        }
    }
    if on(Relaxation::Periods) && !c.periods.is_empty() && !c.periods.contains(&work.period) {
        return false;
    }
    if on(Relaxation::Decades)
        && !c.decades.is_empty()
        && !work
            .year()
            .is_some_and(|year| c.decades.contains(&(year - year % 10)))
    {
        return false;
    }
    if on(Relaxation::Length) {
        let secs = u64::from(work.total_secs);
        if c.min_work_minutes.is_some_and(|m| secs < u64::from(m) * 60)
            || c.max_work_minutes.is_some_and(|m| secs > u64::from(m) * 60)
        {
            return false;
        }
    }
    if on(Relaxation::Keywords) {
        if !c.include_keywords.is_empty()
            && !c
                .include_keywords
                .iter()
                .any(|k| keyword_matches(haystack, k))
        {
            return false;
        }
        if c.exclude_keywords
            .iter()
            .any(|k| keyword_matches(haystack, k))
        {
            return false;
        }
    }
    true
}

/// Whether the constraints ask for this work by name: an included artist,
/// composer or genre names it. Such a request outranks the owner's standing
/// avoids and bans ([`crate::prefs`]).
#[must_use]
pub fn names(work: &Work, constraints: &DjConstraints) -> bool {
    let c = constraints;
    let credits = || credited(work);
    c.include_composers
        .iter()
        .any(|q| composer_matches(q, &work.composer))
        || (!c.include_artists.is_empty()
            && credits()
                .iter()
                .any(|a| c.include_artists.iter().any(|q| artist_matches(q, a))))
        || (!c.include_genres.is_empty()
            && work
                .genres()
                .iter()
                .any(|t| c.include_genres.iter().any(|q| genre_matches(q, t))))
}

/// Whether an artist query names `artist` (a credited name): a whole phrase
/// of it, normalized, so "Beatles" finds "The Beatles" and "Miles Davis"
/// finds "Miles Davis Quintet". A blank query names no one.
#[must_use]
pub fn artist_matches(query: &str, artist: &str) -> bool {
    let query = normalize(query);
    !query.is_empty() && has_phrase(&normalize(artist), &query)
}

/// Whether a genre query names a genre tag: a whole phrase of it,
/// normalized, so "jazz" finds "cool jazz" and "hip-hop" finds "east coast
/// hip hop". A blank query names none.
#[must_use]
pub fn genre_matches(query: &str, tag: &str) -> bool {
    artist_matches(query, tag)
}

/// Every artist a work credits (its composer and performers, for a
/// classical work), across its movements.
pub(crate) fn credited(work: &Work) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();
    for movement in &work.movements {
        for name in split_artists(movement.track.artist.as_deref().unwrap_or_default()) {
            if !names.contains(&name) {
                names.push(name);
            }
        }
    }
    if !names.contains(&work.composer) {
        names.push(work.composer.clone());
    }
    names
}

/// Everything a keyword is matched against, normalized: the work's title,
/// every movement's full title, the album, and the credited artists.
#[must_use]
pub fn haystack(work: &Work) -> String {
    let mut text = work.title.clone();
    if let Some(first) = work.movements.first() {
        for part in [first.track.album.as_deref(), first.track.artist.as_deref()] {
            text.push(' ');
            text.push_str(part.unwrap_or_default());
        }
    }
    for movement in &work.movements {
        text.push(' ');
        text.push_str(&movement.track.title);
    }
    normalize(&text)
}

const CHORAL: &[&str] = &[
    "choral",
    "choir",
    "chorus",
    "chor",
    "mass",
    "missa",
    "requiem",
    "motet",
    "motets",
    "cantata",
    "magnificat",
    "oratorio",
    "anthem",
    "stabat mater",
    "vespers",
    "antiphon",
    "kyrie",
    "gloria",
    "passion",
    "te deum",
    "psalm",
    "psalms",
];
const OPERA: &[&str] = &[
    "opera",
    "act",
    "aria",
    "arias",
    "recitative",
    "recitativo",
    "libretto",
];
const SONG: &[&str] = &[
    "song",
    "songs",
    "lied",
    "lieder",
    "liederkreis",
    "song cycle",
    "winterreise",
    "mullerin",
    "schwanengesang",
    "dichterliebe",
    "frauenliebe",
    "kindertotenlieder",
    "chanson",
    "chansons",
    "melodie",
    "melodies",
    "soprano",
    "mezzo",
    "alto",
    "tenor",
    "baritone",
    "bass baritone",
    "countertenor",
];
const PIANO: &[&str] = &[
    "piano",
    "pianoforte",
    "klavier",
    "nocturne",
    "nocturnes",
    "impromptu",
    "impromptus",
    "etude",
    "etudes",
    "mazurka",
    "mazurkas",
    "polonaise",
    "ballade",
    "gymnopedie",
    "gnossienne",
    "bagatelle",
    "bagatelles",
    "intermezzo",
];
const CHAMBER: &[&str] = &[
    "chamber", "quartet", "quartets", "quintet", "trio", "trios", "sextet", "septet", "octet",
    "duo",
];
const ORCHESTRAL: &[&str] = &[
    "orchestra",
    "orchestral",
    "symphony",
    "symphonies",
    "symphonic",
    "philharmonic",
    "concerto",
    "concerti",
    "overture",
    "sinfonia",
];

/// The words a keyword stands for: a category's family of terms, or just
/// itself.
fn expand(keyword: &str) -> Vec<&str> {
    match keyword {
        "choral" | "choir" => CHORAL.to_vec(),
        "opera" => OPERA.to_vec(),
        "vocal" | "voice" | "singing" => [CHORAL, OPERA, SONG].concat(),
        "song" | "songs" | "lieder" => SONG.to_vec(),
        "piano" => PIANO.to_vec(),
        "chamber" => CHAMBER.to_vec(),
        "orchestral" | "orchestra" => ORCHESTRAL.to_vec(),
        other => vec![other],
    }
}

/// Whether a keyword (or its category) appears, as whole words, in a
/// normalized [`haystack`].
#[must_use]
pub fn keyword_matches(haystack: &str, keyword: &str) -> bool {
    let keyword = normalize(keyword);
    !keyword.is_empty()
        && expand(&keyword)
            .iter()
            .any(|word| has_phrase(haystack, word))
}

/// Named presets of [`DjConstraints`] and the time-of-day [`Program`]s that
/// pick one: built-ins, overridden by the owner's `moods.toml` in the data
/// dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moods {
    moods: BTreeMap<String, DjConstraints>,
    programs: Vec<Program>,
}

/// A time-of-day program: the mood to play when a session names none.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Program {
    /// Monday = 0 … Sunday = 6.
    days: [bool; 7],
    /// Minutes after midnight; a window ending before it starts wraps past
    /// midnight, and equal ends cover the whole day.
    from: u16,
    to: u16,
    pub mood: String,
}

impl Program {
    fn new(days: [bool; 7], from: u16, to: u16, mood: &str) -> Self {
        Self {
            days,
            from,
            to,
            mood: mood.to_owned(),
        }
    }

    /// The days it plays on, Monday first.
    #[must_use]
    pub fn days(&self) -> [bool; 7] {
        self.days
    }

    /// Its window, `(from, to)` in minutes after midnight (24:00 is 1440). A
    /// window ending before it starts wraps past midnight; equal ends cover
    /// the whole day.
    #[must_use]
    pub fn window(&self) -> (u16, u16) {
        (self.from, self.to)
    }

    /// Whether it plays at `minute` (after midnight) on `day`. A window past
    /// midnight belongs to the day it starts: Friday 21:00–06:00 covers
    /// Saturday 03:00.
    fn covers(&self, day: Weekday, minute: u16) -> bool {
        let on = |d: Weekday| self.days[d.num_days_from_monday() as usize];
        match self.from.cmp(&self.to) {
            std::cmp::Ordering::Equal => on(day),
            std::cmp::Ordering::Less => on(day) && (self.from..self.to).contains(&minute),
            std::cmp::Ordering::Greater => {
                (on(day) && minute >= self.from) || (on(day.pred()) && minute < self.to)
            }
        }
    }
}

/// As `moods.toml` writes it: `{"days": ["sat", "sun"], "from": "06:00",
/// "to": "11:00", "mood": "sunday-morning"}`.
impl Serialize for Program {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        const NAMES: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
        let hhmm = |minute: u16| format!("{:02}:{:02}", minute / 60, minute % 60);
        let days: Vec<&str> = NAMES
            .iter()
            .zip(self.days)
            .filter_map(|(name, on)| on.then_some(*name))
            .collect();
        let mut program = serializer.serialize_struct("Program", 4)?;
        program.serialize_field("days", &days)?;
        program.serialize_field("from", &hhmm(self.from))?;
        program.serialize_field("to", &hhmm(self.to))?;
        program.serialize_field("mood", &self.mood)?;
        program.end()
    }
}

const EVERY_DAY: [bool; 7] = [true; 7];
const WEEKDAYS: [bool; 7] = [true, true, true, true, true, false, false];
const WEEKENDS: [bool; 7] = [false, false, false, false, false, true, true];

/// `moods.toml`: `[moods.<name>]` tables of [`DjConstraints`] fields and
/// `[[programs]]` entries; anything else is a mistake worth reporting.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct MoodsFile {
    #[serde(default)]
    moods: BTreeMap<String, toml::Spanned<DjConstraints>>,
    #[serde(default)]
    programs: Option<Vec<toml::Spanned<ProgramDef>>>,
}

/// The 1-based line of byte `offset` in `text`.
pub(crate) fn line_of(text: &str, offset: usize) -> usize {
    text.char_indices()
        .take_while(|&(i, _)| i < offset)
        .filter(|&(_, c)| c == '\n')
        .count()
        + 1
}

/// A `[[programs]]` entry: `days` ("daily", "weekdays", "weekends", or a
/// list like `["sat", "sun"]`), `from` and `to` ("HH:MM"), and `mood`.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ProgramDef {
    #[serde(default = "DaysDef::daily")]
    days: DaysDef,
    from: String,
    to: String,
    mood: String,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DaysDef {
    Named(String),
    List(Vec<String>),
}

impl DaysDef {
    fn daily() -> Self {
        Self::Named("daily".to_owned())
    }

    fn mask(&self) -> Result<[bool; 7], String> {
        let names: Vec<&str> = match self {
            Self::Named(name) => match name.to_lowercase().as_str() {
                "daily" | "every day" | "everyday" => return Ok(EVERY_DAY),
                "weekdays" => return Ok(WEEKDAYS),
                "weekends" => return Ok(WEEKENDS),
                _ => vec![name.as_str()],
            },
            Self::List(names) => names.iter().map(String::as_str).collect(),
        };
        let mut mask = [false; 7];
        for name in names {
            let day = Weekday::from_str(name).map_err(|_| format!("unknown day {name:?}"))?;
            mask[day.num_days_from_monday() as usize] = true;
        }
        Ok(mask)
    }
}

/// "HH:MM" as minutes after midnight; "24:00" is the end of the day.
fn minutes(text: &str) -> Result<u16, String> {
    let bad = || format!("time {text:?} is not HH:MM");
    let (h, m) = text.split_once(':').ok_or_else(bad)?;
    let (h, m): (u16, u16) = (h.parse().map_err(|_| bad())?, m.parse().map_err(|_| bad())?);
    if m >= 60 || h > 24 || (h == 24 && m > 0) {
        return Err(bad());
    }
    Ok(h * 60 + m)
}

impl Default for Moods {
    fn default() -> Self {
        Self::builtin()
    }
}

impl Moods {
    /// The built-in moods: focus, dinner, sunday-morning, bright, calm.
    #[must_use]
    pub fn builtin() -> Self {
        let words = |list: &[&str]| list.iter().map(|&w| w.to_owned()).collect::<Vec<_>>();
        let moods = [
            // Instrumental and mid-low energy: nothing sung.
            (
                "focus",
                DjConstraints {
                    exclude_keywords: words(&["vocal"]),
                    energy_bias: -1,
                    ..DjConstraints::default()
                },
            ),
            // Chamber music and solo piano (no concertos or symphonies), calm,
            // nothing longer than half an hour.
            (
                "dinner",
                DjConstraints {
                    include_keywords: words(&["chamber", "piano"]),
                    exclude_keywords: words(&["orchestral"]),
                    max_work_minutes: Some(30),
                    energy_bias: -1,
                    ..DjConstraints::default()
                },
            ),
            // The great choral eras, Renaissance and Baroque, on the bright side.
            (
                "sunday-morning",
                DjConstraints {
                    periods: vec![Period::Renaissance, Period::Baroque],
                    energy_bias: 1,
                    ..DjConstraints::default()
                },
            ),
            (
                "bright",
                DjConstraints {
                    energy_bias: 2,
                    ..DjConstraints::default()
                },
            ),
            (
                "calm",
                DjConstraints {
                    energy_bias: -2,
                    ..DjConstraints::default()
                },
            ),
        ];
        Self {
            moods: moods
                .into_iter()
                .map(|(name, constraints)| (name.to_owned(), constraints))
                .collect(),
            programs: vec![
                Program::new(WEEKENDS, 6 * 60, 11 * 60, "sunday-morning"),
                Program::new(WEEKDAYS, 6 * 60, 11 * 60, "bright"),
                Program::new(EVERY_DAY, 18 * 60, 21 * 60, "dinner"),
                Program::new(EVERY_DAY, 21 * 60, 6 * 60, "calm"),
            ],
        }
    }

    /// The built-ins overridden (by name) and extended by a `moods.toml`
    /// text. `source` names the file in errors, which give its line.
    pub fn parse(text: &str, source: &str) -> Result<Self, SpotifyError> {
        let file: MoodsFile = toml::from_str(text).map_err(|e| {
            let line = e.span().map(|span| line_of(text, span.start));
            SpotifyError::Config(match line {
                Some(line) => format!("{source} line {line}: {}", e.message().trim()),
                None => format!("{source}: {}", e.message().trim()),
            })
        })?;
        let mut moods = Self::builtin();
        for (name, constraints) in file.moods {
            let name = name.to_lowercase();
            let at = constraints.span().start;
            let constraints = constraints.into_inner();
            constraints.validate().map_err(|why| {
                let header = |l: &str| {
                    let l = l.trim().to_lowercase();
                    l == format!("[moods.{name}]") || l == format!("[moods.\"{name}\"]")
                };
                // The table's header, else where its value starts (a dotted
                // key or an inline table).
                let line = text
                    .lines()
                    .position(header)
                    .map_or_else(|| line_of(text, at), |i| i + 1);
                SpotifyError::Config(format!("{source} line {line}: [moods.{name}] {why}"))
            })?;
            moods.moods.insert(name, constraints);
        }
        if let Some(defs) = file.programs {
            let headers: Vec<usize> = text
                .lines()
                .enumerate()
                .filter(|(_, l)| l.trim() == "[[programs]]")
                .map(|(i, _)| i + 1)
                .collect();
            let mut programs = Vec::with_capacity(defs.len());
            for (k, def) in defs.into_iter().enumerate() {
                let at = def.span().start;
                let def = def.into_inner();
                let program = (|| -> Result<Program, String> {
                    let mood = def.mood.to_lowercase();
                    if moods.get(&mood).is_none() {
                        return Err(format!("no mood {mood:?}"));
                    }
                    Ok(Program::new(
                        def.days.mask()?,
                        minutes(&def.from)?,
                        minutes(&def.to)?,
                        &mood,
                    ))
                })()
                .map_err(|why| {
                    let line = headers.get(k).copied().unwrap_or_else(|| line_of(text, at));
                    SpotifyError::Config(format!("{source} line {line}: [[programs]] {why}"))
                })?;
                programs.push(program);
            }
            moods.programs = programs;
        }
        Ok(moods)
    }

    /// Load `path` over the built-ins; a missing file means just the
    /// built-ins.
    pub fn load(path: &Path) -> Result<Self, SpotifyError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, &path.display().to_string()),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::builtin()),
            Err(e) => Err(e.into()),
        }
    }

    /// A mood by name (case-insensitive).
    #[must_use]
    pub fn get(&self, name: &str) -> Option<&DjConstraints> {
        self.moods.get(&name.to_lowercase())
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.moods.keys().map(String::as_str)
    }

    /// The mood the programs pick at a local day and time (the first
    /// program that covers it), if any.
    #[must_use]
    pub fn program_at(&self, day: Weekday, time: NaiveTime) -> Option<&str> {
        let minute = u16::try_from(time.hour() * 60 + time.minute()).unwrap_or(0);
        self.programs
            .iter()
            .find(|p| p.covers(day, minute))
            .map(|p| p.mood.as_str())
    }

    #[must_use]
    pub fn programs(&self) -> &[Program] {
        &self.programs
    }
}

/// A zone's DJ session as the store keeps it: an optional mood, explicit
/// constraints on top, and when the session ends.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DjSession {
    pub zone: String,
    #[serde(default)]
    pub mood: Option<String>,
    #[serde(default)]
    pub constraints: DjConstraints,
    #[serde(default)]
    pub expires_at: Option<i64>,
}

impl DjSession {
    /// The steering this session asks for at `now`: its mood's constraints
    /// with its own laid on top, or `None` once it has expired. Naming a mood
    /// that doesn't exist is an error.
    pub fn steer(&self, moods: &Moods, now: Option<i64>) -> Result<Option<Steer>, SpotifyError> {
        self.steer_at(moods, now, None)
    }

    /// [`Self::steer`], with the local day and time: a session that names no
    /// mood takes the one its time-of-day program picks.
    pub fn steer_at(
        &self,
        moods: &Moods,
        now: Option<i64>,
        local: Option<(Weekday, NaiveTime)>,
    ) -> Result<Option<Steer>, SpotifyError> {
        if let (Some(expires), Some(now)) = (self.expires_at, now)
            && now >= expires
        {
            return Ok(None);
        }
        let mood = self.mood.clone().or_else(|| {
            local
                .and_then(|(day, time)| moods.program_at(day, time))
                .map(str::to_owned)
        });
        let base = match &mood {
            Some(name) => moods
                .get(name)
                .ok_or_else(|| SpotifyError::UnknownMood {
                    name: name.clone(),
                    known: moods.names().map(str::to_owned).collect(),
                })?
                .clone(),
            None => DjConstraints::default(),
        };
        let mut constraints = base.merged(&self.constraints);
        constraints.validate().map_err(SpotifyError::Config)?;
        if let Some(expires) = self.expires_at {
            constraints.expires_at =
                Some(constraints.expires_at.map_or(expires, |e| e.min(expires)));
        }
        Ok(Some(Steer {
            mood: mood.map(|m| m.to_lowercase()),
            constraints,
        }))
    }

    /// A session for `coordinator`'s group with no mood, no constraints and
    /// no end: it follows the time-of-day programs.
    #[must_use]
    pub fn open(coordinator: &str) -> Self {
        Self {
            zone: coordinator.to_owned(),
            mood: None,
            constraints: DjConstraints::default(),
            expires_at: None,
        }
    }

    /// Whether the session has ended by `now`.
    #[must_use]
    pub fn expired(&self, now: i64) -> bool {
        self.expires_at.is_some_and(|expires| now >= expires)
    }

    /// The store row: `zone` is the group coordinator's id, the constraints
    /// go as JSON (none when they constrain nothing), and a session without
    /// an end lapses at the end of time.
    pub fn to_store(&self) -> Result<StoredDjSession, SpotifyError> {
        let constraints = if self.constraints == DjConstraints::default() {
            None
        } else {
            Some(
                serde_json::to_string(&self.constraints)
                    .map_err(|e| SpotifyError::Decode(format!("DJ session constraints: {e}")))?,
            )
        };
        Ok(StoredDjSession {
            coordinator: self.zone.clone(),
            mood: self.mood.clone(),
            constraints,
            expires: self.expires_at.unwrap_or(NEVER),
        })
    }

    /// A store row as a session. Constraints this crate can't read are an
    /// error rather than silently dropped.
    pub fn from_store(row: &StoredDjSession) -> Result<Self, SpotifyError> {
        let constraints = match &row.constraints {
            Some(json) => serde_json::from_str(json)
                .map_err(|e| SpotifyError::Decode(format!("DJ session constraints: {e}")))?,
            None => DjConstraints::default(),
        };
        Ok(Self {
            zone: row.coordinator.clone(),
            mood: row.mood.clone(),
            constraints,
            expires_at: (row.expires != NEVER).then_some(row.expires),
        })
    }
}

/// The store row's `expires` for a session that never ends.
const NEVER: i64 = i64::MAX;

/// The steering for `coordinator`'s group at `now`: its stored session's mood
/// and constraints while the session lasts; otherwise (no session, one that
/// names no mood, or one that has ended) the mood its time-of-day program
/// picks at the `local` day and time. What the daemon passes as
/// `Planning::steer` before each pick.
pub fn steering<S: Store + ?Sized>(
    store: &S,
    moods: &Moods,
    coordinator: &str,
    now: i64,
    local: Option<(Weekday, NaiveTime)>,
) -> Result<Option<Steer>, SpotifyError> {
    let session = match store.dj_session(coordinator)? {
        Some(row) => Some(DjSession::from_store(&row)?),
        None => None,
    }
    .filter(|session| !session.expired(now))
    .unwrap_or_else(|| DjSession::open(coordinator));
    session.steer_at(moods, Some(now), local)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classical::composer_matches;
    use crate::dj::{DjConfig, Factor, PickContext, PlannedWork, Rng, WorkPool, pick_next};
    use crate::library::{LibraryItem, Origin};
    use crate::test_shelf::{
        MIDNIGHT, item, mean_energy, opera, shelf_items, simulate_steered, transcript, works,
        works_of,
    };

    /// What a pick must satisfy under one constraint.
    type Check = Box<dyn Fn(&Work) -> bool>;

    fn library() -> Vec<LibraryItem> {
        let mut items = shelf_items(1);
        items.extend(opera());
        items
    }

    fn steer(constraints: DjConstraints) -> Steer {
        Steer {
            mood: None,
            constraints,
        }
    }

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|&w| w.to_owned()).collect()
    }

    /// Plan under `steer`, requiring every pick to pass `ok` with nothing
    /// relaxed; a failure prints the full transcript.
    fn honored<'p>(
        pool: &'p WorkPool,
        steer: &Steer,
        seed: u64,
        count: usize,
        ok: impl Fn(&Work) -> bool,
    ) -> Vec<PlannedWork<'p>> {
        let picks = simulate_steered(
            pool,
            &DjConfig::default(),
            seed,
            count,
            Some(15),
            Some(steer),
        );
        for pick in &picks {
            assert!(
                pick.reason.relaxed.is_empty() && ok(pick.work),
                "{steer:?} violated by {}:\n{}",
                pick.work.title,
                transcript(seed, pool, &picks)
            );
        }
        picks
    }

    #[test]
    fn each_constraint_kind_filters_every_pick() {
        let pool = works_of(&library());
        let bach = "Johann Sebastian Bach";
        let cases: Vec<(DjConstraints, Check)> = vec![
            (
                DjConstraints {
                    include_composers: words(&["Bach", "Mozart"]),
                    ..DjConstraints::default()
                },
                Box::new(move |w: &Work| {
                    [bach, "Wolfgang Amadeus Mozart"].contains(&w.composer.as_str())
                }),
            ),
            (
                DjConstraints {
                    exclude_composers: words(&["J.S. Bach"]),
                    ..DjConstraints::default()
                },
                Box::new(move |w: &Work| w.composer != bach),
            ),
            (
                DjConstraints {
                    periods: vec![Period::Romantic, Period::LateRomantic],
                    ..DjConstraints::default()
                },
                Box::new(|w: &Work| matches!(w.period, Period::Romantic | Period::LateRomantic)),
            ),
            (
                DjConstraints {
                    include_keywords: words(&["piano"]),
                    ..DjConstraints::default()
                },
                Box::new(|w: &Work| keyword_matches(&haystack(w), "piano")),
            ),
            (
                DjConstraints {
                    exclude_keywords: words(&["vocal"]),
                    ..DjConstraints::default()
                },
                Box::new(|w: &Work| !keyword_matches(&haystack(w), "vocal")),
            ),
            (
                DjConstraints {
                    max_work_minutes: Some(10),
                    ..DjConstraints::default()
                },
                Box::new(|w: &Work| w.total_secs <= 600),
            ),
            (
                DjConstraints {
                    min_work_minutes: Some(25),
                    ..DjConstraints::default()
                },
                Box::new(|w: &Work| w.total_secs >= 1500),
            ),
        ];
        for (seed, (constraints, ok)) in (21..).zip(cases) {
            honored(&pool, &steer(constraints), seed, 25, ok);
        }
    }

    #[test]
    fn keyword_categories_and_composer_queries() {
        let pool = works_of(&library());
        let find = |title: &str| {
            pool.works()
                .iter()
                .find(|w| w.title.starts_with(title))
                .unwrap()
        };
        let hay = |title: &str| haystack(find(title));
        assert!(keyword_matches(&hay("Nocturne No. 1"), "piano"));
        assert!(keyword_matches(&hay("Piano Sonata No. 1"), "Piano"));
        assert!(keyword_matches(&hay("Cantata No. 1"), "choral"));
        assert!(keyword_matches(&hay("Cantata No. 1"), "vocal"));
        assert!(keyword_matches(&hay("La traviata"), "opera"));
        assert!(keyword_matches(&hay("String Quartet No. 1"), "chamber"));
        assert!(keyword_matches(&hay("Symphony No. 1"), "orchestral"));
        assert!(!keyword_matches(&hay("Symphony No. 1"), "vocal"));
        assert!(!keyword_matches(&hay("Nocturne No. 1"), "harpsichord"));
        // Passions and song cycles are sung, whatever their titles say.
        for sung in [
            "Matthäus-Passion, BWV 244: Kommt, ihr Töchter",
            "Winterreise, D. 911: Gute Nacht",
            "Die schöne Müllerin, D. 795: Das Wandern",
            "Dichterliebe, Op. 48: Im wunderschönen Monat Mai",
            "Liederkreis, Op. 39: In der Fremde",
        ] {
            assert!(keyword_matches(&normalize(sung), "vocal"), "{sung}");
        }
        // Dinner's piano is solo piano: concertos are out.
        let moods = Moods::builtin();
        let dinner = moods.get("dinner").unwrap();
        let fits = |title: &str| admits(find(title), &hay(title), dinner, &[]);
        assert!(fits("Nocturne No. 1") && fits("String Quartet No. 1"));
        assert!(!fits("Piano Concerto No. 1"));

        assert!(composer_matches("Bach", "Johann Sebastian Bach"));
        assert!(composer_matches("J.S. Bach", "Johann Sebastian Bach"));
        assert!(!composer_matches("C.P.E. Bach", "Johann Sebastian Bach"));
        assert!(composer_matches("Saint-Saens", "Camille Saint-Saëns"));
        assert!(composer_matches("Gould", "Glenn Gould"));
        assert!(!composer_matches("Mozart", "Johann Sebastian Bach"));
    }

    #[test]
    fn long_works_by_request_and_narrow_composer_requests() {
        let pool = works_of(&library());
        // "Just Verdi": one work, a long opera; the request holds (no relaxing
        // composers while anything matches) and the length limit lifts.
        let verdi = steer(DjConstraints {
            include_composers: words(&["Verdi"]),
            allow_long_works: true,
            ..DjConstraints::default()
        });
        let picks = honored(&pool, &verdi, 3, 3, |w| w.title == "La traviata");
        assert!(picks.iter().all(|p| p.reason.has(Factor::LongWork)));
        assert!(picks[1].reason.has(Factor::Rotation), "one work rotates");

        // With a short Verdi work too, the limit decides: without the
        // request the opera never plays; with it, it does.
        let mut items = library();
        let album = || {
            (
                "Verdi: String Quartet".to_owned(),
                "spotify:album:verdi-quartet".to_owned(),
            )
        };
        for (m, movement) in [
            "I. Allegro",
            "II. Andantino",
            "III. Prestissimo",
            "IV. Scherzo Fuga",
        ]
        .iter()
        .enumerate()
        {
            let uri = format!("spotify:track:verdi-quartet-{m}");
            let title = format!("String Quartet in E Minor: {movement}");
            items.push(item(
                uri,
                title,
                "Giuseppe Verdi",
                album(),
                420,
                Origin::SavedAlbum,
            ));
        }
        let pool = works_of(&items);
        let just_verdi = steer(DjConstraints {
            include_composers: words(&["Verdi"]),
            ..DjConstraints::default()
        });
        for seed in 0..4 {
            let short = honored(&pool, &just_verdi, seed, 4, |w| w.title != "La traviata");
            assert!(short.iter().all(|p| !p.reason.has(Factor::LongWork)));
            let both = honored(&pool, &verdi, seed, 2, |w| w.composer == "Giuseppe Verdi");
            let opera = both.iter().find(|p| p.work.title == "La traviata");
            assert!(
                opera.is_some_and(|p| p.reason.has(Factor::LongWork)),
                "{}",
                transcript(seed, &pool, &both)
            );
        }
    }

    #[test]
    fn energy_bias_steers_the_target() {
        let pool = works_of(&library());
        let session = |bias: i8| {
            let s = steer(DjConstraints {
                energy_bias: bias,
                ..DjConstraints::default()
            });
            (30..33)
                .flat_map(|seed| {
                    works(&simulate_steered(
                        &pool,
                        &DjConfig::default(),
                        seed,
                        20,
                        Some(14),
                        Some(&s),
                    ))
                })
                .collect::<Vec<_>>()
        };
        let (calm, bright) = (session(-2), session(2));
        assert!(
            mean_energy(&calm) + 15.0 < mean_energy(&bright),
            "calm {} vs bright {}",
            mean_energy(&calm),
            mean_energy(&bright)
        );
        let calmer = steer(DjConstraints {
            energy_bias: -1,
            ..DjConstraints::default()
        });
        let ctx = PickContext {
            local_hour: Some(14),
            steer: Some(&calmer),
            ..PickContext::default()
        };
        let planned = pick_next(&pool, &ctx, &DjConfig::default(), &mut Rng::new(1)).unwrap();
        assert!(
            planned
                .reason
                .summary
                .contains("steady afternoon target; steered calmer")
        );
        assert_eq!(calmer.constraints.biased_target(Some(58)), Some(46));
        assert_eq!(calmer.constraints.biased_target(None), Some(38));
    }

    #[test]
    fn artists_steer_any_genre() {
        let pool = works_of(&crate::test_shelf::mixed_items());
        let all = pool.works();
        let hays: Vec<String> = all.iter().map(haystack).collect();
        let relax = |c: DjConstraints| admit(all, &hays, &c, 5);
        let only = |names: &[&str]| DjConstraints {
            include_artists: words(names),
            ..DjConstraints::default()
        };

        // Just Nina Marsh: her quartet's four songs, though fewer than the
        // minimum — an artist request holds while anything matches.
        let (admitted, relaxed) = relax(only(&["nina marsh"]));
        assert!(relaxed.is_empty());
        assert_eq!(admitted.len(), 4);
        assert!(
            admitted
                .iter()
                .all(|&w| all[w].composer == "Nina Marsh Quartet")
        );
        // A featured credit counts: the trio's three and the single he is on.
        assert_eq!(relax(only(&["Otis Fairweather"])).0.len(), 4);
        // So do a classical work's performers.
        let (admitted, _) = relax(only(&["Test Ensemble"]));
        assert_eq!(admitted.len(), 27);
        assert!(admitted.iter().all(|&w| all[w].movements[0].classical));

        let without = DjConstraints {
            exclude_artists: words(&["Juniper Vale", "mc halcyon"]),
            ..DjConstraints::default()
        };
        let (admitted, relaxed) = relax(without);
        assert!(relaxed.is_empty());
        assert_eq!(admitted.len(), all.len() - 11);
        assert!(admitted.iter().all(|&w| {
            !credited(&all[w])
                .iter()
                .any(|a| a == "Juniper Vale" || a == "MC Halcyon")
        }));

        // No one by that name: the filter relaxes, and the reason says so.
        let nobody = only(&["Nobody At All"]);
        let (admitted, relaxed) = relax(nobody.clone());
        assert_eq!(relaxed, [Relaxation::Artists]);
        assert_eq!(admitted.len(), all.len());
        let picks = simulate_steered(
            &pool,
            &DjConfig::default(),
            5,
            1,
            Some(15),
            Some(&steer(nobody)),
        );
        assert!(
            picks[0]
                .reason
                .summary
                .ends_with("relaxed the artist filter (too few matching works)"),
            "{}",
            picks[0].reason.summary
        );

        assert!(artist_matches("Beatles", "The Beatles"));
        assert!(!artist_matches("Beat", "The Beatles"));
        assert!(!artist_matches("  ", "The Beatles"));
        let merged = only(&["Nina Marsh"]).merged(&only(&["Ada Brightwell", "Nina Marsh"]));
        assert_eq!(merged.include_artists, ["Nina Marsh", "Ada Brightwell"]);
    }

    #[test]
    fn genres_and_decades_steer_any_genre() {
        let pool = works_of(&crate::test_shelf::mixed_items());
        let all = pool.works();
        assert_eq!(all.len(), 25 + 27, "songs and classical works");
        let hays: Vec<String> = all.iter().map(haystack).collect();
        let relax = |c: DjConstraints| admit(all, &hays, &c, 5);
        let genres = |include: &[&str], exclude: &[&str]| DjConstraints {
            include_genres: words(include),
            exclude_genres: words(exclude),
            ..DjConstraints::default()
        };

        // "jazz" finds both jazz albums; "pop" finds indie pop too.
        let (admitted, relaxed) = relax(genres(&["jazz"], &[]));
        assert!(relaxed.is_empty());
        assert_eq!(admitted.len(), 7);
        assert!(admitted.iter().all(|&w| all[w].genres() == ["jazz"]));
        assert_eq!(relax(genres(&["pop"], &[])).0.len(), 10);
        // Every classical work is "classical", tags or not.
        let (admitted, _) = relax(genres(&["Classical"], &[]));
        assert_eq!(admitted.len(), 27);
        assert!(admitted.iter().all(|&w| all[w].is_classical()));
        let (admitted, relaxed) = relax(genres(&[], &["hip-hop", "soundtrack"]));
        assert!(relaxed.is_empty());
        assert_eq!(admitted.len(), all.len() - 5 - 3);
        // A genre asked for by name holds while anything matches; with
        // nothing tagged that way, it relaxes, and the reason says so.
        let (admitted, relaxed) = relax(genres(&["polka"], &[]));
        assert_eq!(relaxed, [Relaxation::Genres]);
        assert_eq!(admitted.len(), all.len());
        let picks = simulate_steered(
            &pool,
            &DjConfig::default(),
            5,
            1,
            Some(15),
            Some(&steer(genres(&["polka"], &[]))),
        );
        assert!(
            picks[0]
                .reason
                .summary
                .ends_with("relaxed the genre filter (too few matching works)"),
            "{}",
            picks[0].reason.summary
        );

        // Decades: the fifties and sixties hold the two jazz albums.
        let decades = |d: &[u16]| DjConstraints {
            decades: d.to_vec(),
            ..DjConstraints::default()
        };
        let (admitted, relaxed) = relax(decades(&[1950, 1960]));
        assert!(relaxed.is_empty());
        assert_eq!(admitted.len(), 7);
        assert!(admitted.iter().all(|&w| all[w].year() < Some(1970)));
        // Four songs from the 2020s are too few: the decade relaxes like a
        // period, before length.
        let (admitted, relaxed) = relax(decades(&[2020]));
        assert_eq!(relaxed, [Relaxation::Decades]);
        assert_eq!(admitted.len(), all.len());
        let err = decades(&[1975]).validate().unwrap_err();
        assert!(err.contains("say 1970"), "{err}");

        assert!(genre_matches("hip-hop", "east coast hip hop"));
        assert!(!genre_matches("jaz", "jazz"));
        let merged = genres(&["jazz"], &[]).merged(&DjConstraints {
            decades: vec![1960],
            ..genres(&["soul"], &["pop"])
        });
        assert_eq!(merged.include_genres, ["jazz", "soul"]);
        assert_eq!(merged.exclude_genres, ["pop"]);
        assert_eq!(merged.decades, [1960]);
    }

    #[test]
    fn relaxation_runs_in_order_and_is_reported() {
        let pool = works_of(&library());
        let all = pool.works();
        let hays: Vec<String> = all.iter().map(haystack).collect();
        let relax = |c: DjConstraints| admit(all, &hays, &c, 5);

        // No harpsichord anywhere: keywords go, the period holds.
        let c = DjConstraints {
            include_keywords: words(&["harpsichord"]),
            periods: vec![Period::Baroque],
            ..DjConstraints::default()
        };
        let (admitted, relaxed) = relax(c.clone());
        assert_eq!(relaxed, [Relaxation::Keywords]);
        assert!(admitted.iter().all(|&w| all[w].period == Period::Baroque));
        let picks = simulate_steered(&pool, &DjConfig::default(), 5, 4, Some(15), Some(&steer(c)));
        assert_eq!(picks[0].reason.relaxed, [Relaxation::Keywords]);
        assert!(
            picks[0]
                .reason
                .summary
                .ends_with("relaxed the keyword filter (too few matching works)"),
            "{}",
            picks[0].reason.summary
        );

        // Keywords, then periods, then length — in that order.
        let c = DjConstraints {
            include_keywords: words(&["organ"]),
            periods: vec![Period::Contemporary],
            max_work_minutes: Some(2),
            ..DjConstraints::default()
        };
        let (admitted, relaxed) = relax(c.clone());
        assert_eq!(
            relaxed,
            [
                Relaxation::Keywords,
                Relaxation::Periods,
                Relaxation::Length
            ]
        );
        assert_eq!(admitted.len(), all.len());
        let picks = simulate_steered(&pool, &DjConfig::default(), 5, 1, Some(15), Some(&steer(c)));
        assert!(
            picks[0].reason.summary.ends_with(
                "relaxed the keyword, period and length filters (too few matching works)"
            )
        );

        // A narrow composer request holds while it matches anything…
        let (admitted, relaxed) = relax(DjConstraints {
            include_composers: words(&["Hildegard"]),
            periods: vec![Period::Baroque],
            ..DjConstraints::default()
        });
        assert_eq!(relaxed, [Relaxation::Periods]);
        assert_eq!(admitted.len(), 2);
        // …and relaxes only when nothing does: the DJ never plays nothing.
        let (admitted, relaxed) = relax(DjConstraints {
            include_composers: words(&["Palestrina"]),
            ..DjConstraints::default()
        });
        assert_eq!(
            (relaxed.as_slice(), admitted.len()),
            ([Relaxation::Composers].as_slice(), all.len())
        );
    }

    #[test]
    fn lapsed_constraints_and_sessions_stop_steering() {
        let pool = works_of(&library());
        let now = MIDNIGHT + 12 * 3600;
        let only_bach = |expires_at| {
            steer(DjConstraints {
                include_composers: words(&["Bach"]),
                expires_at: Some(expires_at),
                ..DjConstraints::default()
            })
        };
        let composers = |s: &Steer| {
            (0..40)
                .map(|seed| {
                    let ctx = PickContext {
                        now: Some(now),
                        steer: Some(s),
                        ..PickContext::default()
                    };
                    let p =
                        pick_next(&pool, &ctx, &DjConfig::default(), &mut Rng::new(seed)).unwrap();
                    p.work.composer.clone()
                })
                .collect::<std::collections::BTreeSet<_>>()
        };
        assert_eq!(composers(&only_bach(now + 60)).len(), 1, "still active");
        assert!(composers(&only_bach(now)).len() > 1, "lapsed at its expiry");
        assert!(
            only_bach(now).constraints.is_active(None),
            "no clock, no expiry"
        );

        let moods = Moods::builtin();
        let session = DjSession {
            zone: "Zone A".into(),
            mood: Some("calm".into()),
            constraints: DjConstraints::default(),
            expires_at: Some(now),
        };
        assert!(session.steer(&moods, Some(now - 1)).unwrap().is_some());
        assert!(session.steer(&moods, Some(now)).unwrap().is_none());
    }

    #[test]
    fn moods_builtins_overrides_and_errors() {
        let builtin = Moods::builtin();
        let names: Vec<&str> = builtin.names().collect();
        assert_eq!(
            names,
            ["bright", "calm", "dinner", "focus", "sunday-morning"]
        );

        let text = r#"
[moods.focus]
exclude_keywords = ["opera"]

[moods.Late-Night]
energy_bias = -2
periods = ["baroque", "late_romantic"]
"#;
        let moods = Moods::parse(text, "moods.toml").unwrap();
        assert_eq!(moods.get("focus").unwrap().exclude_keywords, ["opera"]);
        assert_eq!(
            moods.get("focus").unwrap().energy_bias,
            0,
            "the file replaces the built-in"
        );
        let late = moods.get("late-night").unwrap();
        assert_eq!(late.periods, [Period::Baroque, Period::LateRomantic]);
        assert!(moods.get("dinner").is_some(), "other built-ins stay");

        let err = |text: &str| Moods::parse(text, "moods.toml").unwrap_err().to_string();
        let typo = err("[moods.focus]\nenergy_bias = -1\nenergy_bais = 2\n");
        assert!(
            typo.contains("moods.toml line 3") && typo.contains("energy_bais"),
            "{typo}"
        );
        let syntax = err("[moods.focus\nenergy_bias = 1\n");
        assert!(syntax.contains("moods.toml line 1"), "{syntax}");
        let period = err("[moods.x]\nperiods = [\"rococo\"]\n");
        assert!(
            period.contains("moods.toml line 2") && period.contains("rococo"),
            "{period}"
        );
        let range = err("\n\n[moods.loud]\nenergy_bias = 5\n");
        assert!(
            range.contains("moods.toml line 3: [moods.loud] energy_bias 5 is outside -2..=2"),
            "{range}"
        );
        let bounds = err("[moods.x]\nmin_work_minutes = 30\nmax_work_minutes = 10\n");
        assert!(
            bounds.contains("min_work_minutes 30 exceeds max_work_minutes 10"),
            "{bounds}"
        );
        // Without a [moods.x] header the line comes from the value itself.
        let inline = err("\n[moods]\nloud = { energy_bias = 5 }\n");
        assert!(
            inline.contains("moods.toml line 3: [moods.loud] energy_bias 5"),
            "{inline}"
        );
        let dotted = err("[moods]\nquiet.energy_bias = -1\nloud.energy_bias = 5\n");
        assert!(
            dotted.contains("moods.toml line 3: [moods.loud] energy_bias 5"),
            "{dotted}"
        );
        // A misspelled table is an error, not silently ignored.
        let table = err("[mood.focus]\nenergy_bias = 1\n");
        assert!(
            table.contains("moods.toml line 1") && table.contains("mood"),
            "{table}"
        );
    }

    #[test]
    fn moods_load_from_the_data_dir() {
        let dir = std::env::temp_dir().join(format!("fsonos-spotify-moods-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("moods.toml");
        assert_eq!(
            Moods::load(&path).unwrap(),
            Moods::builtin(),
            "no file: built-ins"
        );
        std::fs::write(&path, "[moods.study]\nexclude_keywords = [\"vocal\"]\n").unwrap();
        let moods = Moods::load(&path).unwrap();
        assert!(moods.get("study").is_some() && moods.get("calm").is_some());
        std::fs::write(&path, "[moods.study]\nexclude_keyword = 1\n").unwrap();
        let err = Moods::load(&path).unwrap_err().to_string();
        assert!(err.contains("moods.toml line 2"), "{err}");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    fn at(h: u32, m: u32) -> NaiveTime {
        NaiveTime::from_hms_opt(h, m, 0).unwrap()
    }

    #[test]
    fn builtin_programs_follow_the_week_and_the_clock() {
        let moods = Moods::builtin();
        let cases = [
            (Weekday::Mon, at(7, 0), Some("bright")),
            (Weekday::Fri, at(10, 59), Some("bright")),
            (Weekday::Sat, at(7, 0), Some("sunday-morning")),
            (Weekday::Sun, at(6, 0), Some("sunday-morning")),
            (Weekday::Sun, at(11, 0), None),
            (Weekday::Wed, at(12, 0), None),
            (Weekday::Tue, at(19, 0), Some("dinner")),
            (Weekday::Sat, at(18, 0), Some("dinner")),
            (Weekday::Thu, at(23, 0), Some("calm")),
            // Past midnight the evening program carries on, on any day…
            (Weekday::Sat, at(3, 0), Some("calm")),
            (Weekday::Mon, at(5, 59), Some("calm")),
            // …until the morning one takes over.
            (Weekday::Mon, at(6, 0), Some("bright")),
        ];
        for (day, time, want) in cases {
            assert_eq!(moods.program_at(day, time), want, "{day} {time}");
        }
    }

    #[test]
    fn programs_show_their_days_and_window_as_moods_toml_writes_them() {
        #[derive(Serialize)]
        struct File<'a> {
            programs: &'a [Program],
        }

        let builtin = Moods::builtin();
        let programs = builtin.programs();
        assert_eq!(
            programs[0].days(),
            [false, false, false, false, false, true, true]
        );
        assert_eq!(programs[0].window(), (6 * 60, 11 * 60));
        assert_eq!(
            programs[3].window(),
            (21 * 60, 6 * 60),
            "wraps past midnight"
        );
        assert_eq!(
            serde_json::to_value(&programs[0]).unwrap(),
            serde_json::json!({
                "days": ["sat", "sun"], "from": "06:00", "to": "11:00", "mood": "sunday-morning"
            })
        );
        assert_eq!(
            serde_json::to_value(&programs[3]).unwrap()["days"]
                .as_array()
                .unwrap()
                .len(),
            7
        );
        // Written out as moods.toml, the programs read back the same.
        let text = toml::to_string(&File { programs }).unwrap();
        let back = Moods::parse(&text, "moods.toml").unwrap();
        assert_eq!(back.programs(), programs, "{text}");
        let end_of_day = Moods::parse(
            "[[programs]]\nfrom = \"18:00\"\nto = \"24:00\"\nmood = \"calm\"\n",
            "moods.toml",
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&end_of_day.programs()[0]).unwrap()["to"],
            "24:00"
        );
    }

    #[test]
    fn programs_from_moods_toml_replace_the_builtins() {
        let text = r#"
[moods.late-night]
energy_bias = -2

[[programs]]
days = ["fri", "sat"]
from = "22:30"
to = "02:00"
mood = "Late-Night"

[[programs]]
days = "weekends"
from = "08:00"
to = "12:00"
mood = "sunday-morning"

[[programs]]
from = "00:00"
to = "00:00"
mood = "focus"
"#;
        let moods = Moods::parse(text, "moods.toml").unwrap();
        assert_eq!(moods.programs().len(), 3);
        let cases = [
            (Weekday::Fri, at(23, 0), "late-night"),
            (Weekday::Sat, at(1, 30), "late-night"),
            (Weekday::Sun, at(1, 30), "late-night"),
            // Thursday isn't listed, so Friday 01:30 is past Thursday's window.
            (Weekday::Fri, at(1, 30), "focus"),
            (Weekday::Sun, at(2, 0), "focus"),
            (Weekday::Sat, at(9, 0), "sunday-morning"),
            // The built-in weekday mornings are gone; the all-day one is first
            // to cover them.
            (Weekday::Mon, at(7, 0), "focus"),
            (Weekday::Wed, at(19, 0), "focus"),
        ];
        for (day, time, want) in cases {
            assert_eq!(moods.program_at(day, time), Some(want), "{day} {time}");
        }

        let none = Moods::parse("programs = []\n", "moods.toml").unwrap();
        assert_eq!(none.program_at(Weekday::Mon, at(7, 0)), None, "no programs");
        let kept = Moods::parse("[moods.x]\nenergy_bias = 1\n", "moods.toml").unwrap();
        assert_eq!(
            kept.programs(),
            Moods::builtin().programs(),
            "no [[programs]]: built-ins"
        );

        let err = |text: &str| Moods::parse(text, "moods.toml").unwrap_err().to_string();
        let program = |body: &str| {
            format!(
                "[[programs]]\nfrom = \"06:00\"\nto = \"09:00\"\nmood = \"calm\"\n\n[[programs]]\n{body}"
            )
        };
        let mood = err(&program(
            "from = \"06:00\"\nto = \"09:00\"\nmood = \"disco\"\n",
        ));
        assert!(
            mood.contains("moods.toml line 6: [[programs]] no mood \"disco\""),
            "{mood}"
        );
        let day = err(&program(
            "days = [\"mon\", \"funday\"]\nfrom = \"06:00\"\nto = \"09:00\"\nmood = \"calm\"\n",
        ));
        assert!(
            day.contains("moods.toml line 6") && day.contains("unknown day \"funday\""),
            "{day}"
        );
        for bad in ["25:00", "7:60", "seven", "24:01"] {
            let time = err(&program(&format!(
                "from = \"{bad}\"\nto = \"09:00\"\nmood = \"calm\"\n"
            )));
            assert!(
                time.contains("moods.toml line 6")
                    && time.contains(&format!("time \"{bad}\" is not HH:MM")),
                "{time}"
            );
        }
        let typo = err("[[programs]]\nfrom = \"06:00\"\nto = \"09:00\"\nmod = \"calm\"\n");
        assert!(typo.contains("moods.toml line"), "{typo}");
        let inline = err(
            "programs = [\n  { from = \"06:00\", to = \"09:00\", mood = \"calm\" },\n  { from = \"6\", to = \"09:00\", mood = \"calm\" },\n]\n",
        );
        assert!(
            inline.contains("moods.toml line 3: [[programs]] time \"6\""),
            "{inline}"
        );
    }

    #[test]
    fn a_session_without_a_mood_follows_the_program() {
        let moods = Moods::builtin();
        let session = DjSession {
            zone: "Zone A".into(),
            mood: None,
            constraints: DjConstraints {
                include_composers: words(&["Bach"]),
                ..DjConstraints::default()
            },
            expires_at: None,
        };
        let morning = Some((Weekday::Sat, at(8, 0)));
        let steer = session.steer_at(&moods, None, morning).unwrap().unwrap();
        assert_eq!(steer.mood.as_deref(), Some("sunday-morning"));
        assert_eq!(
            steer.constraints.periods,
            [Period::Renaissance, Period::Baroque]
        );
        assert_eq!(
            steer.constraints.include_composers,
            ["Bach"],
            "its own constraints stay"
        );

        let noon = Some((Weekday::Sat, at(12, 0)));
        let steer = session.steer_at(&moods, None, noon).unwrap().unwrap();
        assert_eq!(steer.mood, None, "no program at noon");
        assert_eq!(
            session.steer(&moods, None).unwrap().unwrap().mood,
            None,
            "no clock, no program"
        );

        let explicit = DjSession {
            mood: Some("Focus".into()),
            ..session.clone()
        };
        let steer = explicit.steer_at(&moods, None, morning).unwrap().unwrap();
        assert_eq!(
            steer.mood.as_deref(),
            Some("focus"),
            "an explicit mood wins"
        );

        let expired = DjSession {
            expires_at: Some(MIDNIGHT),
            ..session
        };
        assert_eq!(
            expired.steer_at(&moods, Some(MIDNIGHT), morning).unwrap(),
            None
        );
    }

    #[test]
    fn seeded_plans_under_every_builtin_mood_honor_it() {
        let pool = works_of(&library());
        let moods = Moods::builtin();
        for (seed, name) in (40..).zip(moods.names()) {
            let session = DjSession {
                zone: "Zone A".into(),
                mood: Some(name.to_owned()),
                constraints: DjConstraints::default(),
                expires_at: None,
            };
            let steer = session.steer(&moods, None).unwrap().unwrap();
            let c = &steer.constraints;
            let picks = honored(&pool, &steer, seed, 12, |w| admits(w, &haystack(w), c, &[]));
            assert!(
                picks
                    .iter()
                    .all(|p| p.reason.summary.contains(&format!("{name} mood"))),
                "{}",
                transcript(seed, &pool, &picks)
            );
        }
    }

    #[test]
    fn sessions_merge_their_mood_and_round_trip() {
        let pool = works_of(&library());
        let moods = Moods::builtin();
        // Focus excludes everything sung; asking for Bach (all cantatas here)
        // on top leaves nothing, so the keyword filter relaxes and Bach plays.
        let session = DjSession {
            zone: "Zone A".into(),
            mood: Some("Focus".into()),
            constraints: DjConstraints {
                include_composers: words(&["Bach"]),
                ..DjConstraints::default()
            },
            expires_at: Some(MIDNIGHT + 7200),
        };
        let steer = session.steer(&moods, Some(MIDNIGHT)).unwrap().unwrap();
        assert_eq!(steer.mood.as_deref(), Some("focus"));
        assert_eq!(steer.constraints.exclude_keywords, ["vocal"]);
        assert_eq!(steer.constraints.include_composers, ["Bach"]);
        assert_eq!(steer.constraints.energy_bias, -1);
        assert_eq!(steer.constraints.expires_at, Some(MIDNIGHT + 7200));
        let picks = simulate_steered(&pool, &DjConfig::default(), 9, 5, Some(10), Some(&steer));
        assert!(
            picks
                .iter()
                .all(|p| p.work.composer == "Johann Sebastian Bach")
        );
        assert_eq!(picks[0].reason.relaxed, [Relaxation::Keywords]);

        let json = serde_json::to_string(&session).unwrap();
        assert_eq!(serde_json::from_str::<DjSession>(&json).unwrap(), session);
        let minimal: DjSession = serde_json::from_str(r#"{"zone":"Zone A"}"#).unwrap();
        assert_eq!(
            minimal.steer(&moods, None).unwrap().unwrap(),
            Steer::default()
        );

        let unknown = DjSession {
            mood: Some("disco".into()),
            ..minimal
        };
        let err = unknown.steer(&moods, None).unwrap_err();
        assert!(
            matches!(&err, SpotifyError::UnknownMood { name, known }
                if name == "disco" && known.iter().any(|k| k == "focus")),
            "{err:?}"
        );
        assert_eq!(
            err.to_string(),
            "no mood \"disco\" (known: bright, calm, dinner, focus, sunday-morning)"
        );
        // Bounds that clash once the session lays its own on the mood's are
        // a Config error, not an unknown mood.
        let clash = DjSession {
            mood: Some("dinner".into()),
            constraints: DjConstraints {
                min_work_minutes: Some(40),
                ..DjConstraints::default()
            },
            ..unknown
        };
        let err = clash.steer(&moods, None).unwrap_err();
        assert!(
            matches!(&err, SpotifyError::Config(why) if why.contains("exceeds max_work_minutes 30")),
            "{err:?}"
        );
    }

    #[test]
    fn sessions_round_trip_through_both_stores_and_steer_by_program() {
        use fsonos_core::store::{MemStore, SqliteStore};

        let moods = Moods::builtin();
        let saturday_morning = Some((Weekday::Sat, at(8, 0)));
        let steered = DjSession {
            zone: "RINCON_TEST0000000000001400".into(),
            mood: Some("focus".into()),
            constraints: DjConstraints {
                include_composers: words(&["Bach"]),
                ..DjConstraints::default()
            },
            expires_at: Some(MIDNIGHT + 3600),
        };
        let open = DjSession::open("RINCON_TEST0000000000002400");
        let row = open.to_store().unwrap();
        assert_eq!((row.constraints.as_deref(), row.expires), (None, i64::MAX));

        let stores: [Box<dyn Store>; 2] = [
            Box::new(MemStore::default()),
            Box::new(SqliteStore::open_in_memory().unwrap()),
        ];
        for mut store in stores {
            store.save_dj_session(&steered.to_store().unwrap()).unwrap();
            store.save_dj_session(&open.to_store().unwrap()).unwrap();
            let back = store.dj_session(&steered.zone).unwrap().unwrap();
            assert_eq!(DjSession::from_store(&back).unwrap(), steered);
            let back = store.dj_session(&open.zone).unwrap().unwrap();
            assert_eq!(DjSession::from_store(&back).unwrap(), open);

            // While it lasts, the session's own mood and constraints steer.
            let steer = steering(&*store, &moods, &steered.zone, MIDNIGHT, saturday_morning)
                .unwrap()
                .unwrap();
            assert_eq!(steer.mood.as_deref(), Some("focus"));
            assert_eq!(steer.constraints.include_composers, ["Bach"]);
            // Once it has ended, the program does — not nothing.
            let steer = steering(
                &*store,
                &moods,
                &steered.zone,
                MIDNIGHT + 3600,
                saturday_morning,
            )
            .unwrap()
            .unwrap();
            assert_eq!(steer.mood.as_deref(), Some("sunday-morning"));
            assert!(steer.constraints.include_composers.is_empty());
            // A session without a mood, or none at all, follows the program;
            // with no local clock there is nothing to follow.
            for zone in [open.zone.as_str(), "RINCON_TEST0000000000003400"] {
                let steer = steering(&*store, &moods, zone, MIDNIGHT, saturday_morning)
                    .unwrap()
                    .unwrap();
                assert_eq!(steer.mood.as_deref(), Some("sunday-morning"), "{zone}");
                let plain = steering(&*store, &moods, zone, MIDNIGHT, None)
                    .unwrap()
                    .unwrap();
                assert_eq!(plain, Steer::default(), "{zone}");
            }
        }

        let garbled = StoredDjSession {
            constraints: Some("{\"energy_bais\": 1}".into()),
            ..steered.to_store().unwrap()
        };
        let err = DjSession::from_store(&garbled).unwrap_err().to_string();
        assert!(err.contains("energy_bais"), "{err}");
    }
}
