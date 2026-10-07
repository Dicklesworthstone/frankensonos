//! Steering the DJ: structured constraints, named moods, and the session
//! that carries them.
//!
//! People and agents ask for "something calmer", "no opera", "just piano for
//! the next two hours", "dinner music". The agent maps that language onto
//! [`DjConstraints`] or a named mood ([`Moods`]); the DJ applies them as hard
//! filters before weighting, so its behavior is predictable and testable.
//! When the filters leave too few works they relax in a fixed order —
//! keywords, then periods, then length — and an explicit composer request
//! relaxes only if nothing at all matches; the pick's reason reports every
//! relaxation. The DJ never silently plays nothing.

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::SpotifyError;
use crate::classical::{Period, composer_matches, has_phrase, normalize};
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
    /// Only these periods.
    pub periods: Vec<Period>,
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
            periods: if over.periods.is_empty() {
                self.periods.clone()
            } else {
                over.periods.clone()
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
            Relaxation::Length => {
                self.min_work_minutes.is_some() || self.max_work_minutes.is_some()
            }
            Relaxation::Composers => {
                !self.include_composers.is_empty() || !self.exclude_composers.is_empty()
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
}

impl Relaxation {
    /// The order filters relax in.
    pub const ORDER: [Self; 4] = [Self::Keywords, Self::Periods, Self::Length, Self::Composers];

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Keywords => "keyword",
            Self::Periods => "period",
            Self::Length => "length",
            Self::Composers => "composer",
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
/// periods, then length relax until at least `min` pass (or every work, in a
/// smaller pool); composers relax only when nothing passes, so "just Pärt"
/// rotates the few Pärt works rather than ignoring the request.
/// `haystacks[i]` is [`haystack`]`(&works[i])`.
#[must_use]
pub fn admit(
    works: &[Work],
    haystacks: &[String],
    constraints: &DjConstraints,
    min: usize,
) -> (Vec<usize>, Vec<Relaxation>) {
    if works.is_empty() {
        return (Vec::new(), Vec::new());
    }
    let want = min.clamp(1, works.len());
    let mut relaxed = Vec::new();
    loop {
        let admitted: Vec<usize> = (0..works.len())
            .filter(|&w| admits(&works[w], &haystacks[w], constraints, &relaxed))
            .collect();
        if admitted.len() >= want {
            return (admitted, relaxed);
        }
        let next = Relaxation::ORDER.into_iter().find(|&r| {
            !relaxed.contains(&r)
                && constraints.constrains(r)
                && (r != Relaxation::Composers || admitted.is_empty())
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
    if on(Relaxation::Periods) && !c.periods.is_empty() && !c.periods.contains(&work.period) {
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
    "melodie",
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

/// Named presets of [`DjConstraints`]: built-ins, overridden by the owner's
/// `moods.toml` in the data dir.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Moods {
    moods: BTreeMap<String, DjConstraints>,
}

/// `moods.toml`: `[moods.<name>]` tables of [`DjConstraints`] fields. Other
/// top-level tables (time-of-day programs) belong to later readers.
#[derive(Deserialize)]
struct MoodsFile {
    #[serde(default)]
    moods: BTreeMap<String, DjConstraints>,
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
            // Chamber music and piano, calm, nothing longer than half an hour.
            (
                "dinner",
                DjConstraints {
                    include_keywords: words(&["chamber", "piano"]),
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
        }
    }

    /// The built-ins overridden (by name) and extended by a `moods.toml`
    /// text. `source` names the file in errors, which give its line.
    pub fn parse(text: &str, source: &str) -> Result<Self, SpotifyError> {
        let file: MoodsFile = toml::from_str(text).map_err(|e| {
            let line = e
                .span()
                .map(|span| text[..span.start.min(text.len())].matches('\n').count() + 1);
            SpotifyError::Config(match line {
                Some(line) => format!("{source} line {line}: {}", e.message().trim()),
                None => format!("{source}: {}", e.message().trim()),
            })
        })?;
        let mut moods = Self::builtin();
        for (name, constraints) in file.moods {
            let name = name.to_lowercase();
            constraints.validate().map_err(|why| {
                let header = |l: &str| {
                    let l = l.trim().to_lowercase();
                    l == format!("[moods.{name}]") || l == format!("[moods.\"{name}\"]")
                };
                match text.lines().position(header) {
                    Some(i) => SpotifyError::Config(format!(
                        "{source} line {}: [moods.{name}] {why}",
                        i + 1
                    )),
                    None => SpotifyError::Config(format!("{source}: mood {name}: {why}")),
                }
            })?;
            moods.moods.insert(name, constraints);
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
        if let (Some(expires), Some(now)) = (self.expires_at, now)
            && now >= expires
        {
            return Ok(None);
        }
        let base = match &self.mood {
            Some(name) => moods
                .get(name)
                .ok_or_else(|| {
                    let known: Vec<&str> = moods.names().collect();
                    SpotifyError::Config(format!("no mood {name:?} (known: {})", known.join(", ")))
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
            mood: self.mood.as_ref().map(|m| m.to_lowercase()),
            constraints,
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classical::composer_matches;
    use crate::dj::{DjConfig, Factor, PickContext, PlannedWork, Rng, WorkPool, pick_next};
    use crate::library::LibraryItem;
    use crate::test_shelf::{
        MIDNIGHT, mean_energy, opera, shelf_items, simulate_steered, transcript, works, works_of,
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
        let err = unknown.steer(&moods, None).unwrap_err().to_string();
        assert!(
            err.contains("no mood \"disco\"") && err.contains("focus"),
            "{err}"
        );
    }
}
