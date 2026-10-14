//! The owner's standing DJ preferences: `preferences.toml` in the data dir.
//!
//! What the owner states by hand, for every group's DJ: genres, artists,
//! eras and moods to favor or avoid, a default energy, whether explicit
//! tracks may play, and artists, albums or tracks to pin or ban. Steering
//! ([`crate::steer`]) is per group and for a while; these hold everywhere
//! until changed.
//!
//! Precedence, strongest first:
//!
//! 1. **Steering.** An active `dj steer` that asks for an artist, composer or
//!    genre by name plays it even when avoided or banned; one that excludes
//!    something excludes it even when favored or pinned.
//! 2. **Preferences.** Avoided and banned works never play, whatever the
//!    feedback or the account says; favored and pinned ones weigh more, and
//!    feedback can't push them below neutral or sit them out.
//! 3. **Learned feedback** ([`crate::feedback`]): likes, dislikes, skips and
//!    full listens.
//! 4. **The account's own signals**: a track the owner liked on Spotify
//!    weighs a little more than one on a saved album.
//!
//! A ban never relaxes. Avoids relax only when every work left is avoided,
//! so the DJ never silently plays nothing; the reason says so.

use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::SpotifyError;
use crate::classical::{Period, normalize};
use crate::steer::{DjConstraints, Moods, admits, artist_matches, credited, genre_matches};
use crate::works::Work;

/// The file in the data directory.
pub const PREFERENCES_FILE: &str = "preferences.toml";

/// The weight of a favored work (per mille).
pub const FAVOR_PM: u64 = 2000;
/// The weight of a pinned work (per mille).
pub const PIN_PM: u64 = 3000;

/// `preferences.toml`. Every field is optional; the default states nothing
/// (and keeps explicit tracks out).
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Preferences {
    /// The energy (0–100) the DJ aims for when no request or steer sets one,
    /// in place of the time-of-day curve.
    pub energy: Option<u8>,
    /// Whether explicit tracks may play (default: no).
    pub explicit: bool,
    /// Lean toward these: a work matching any weighs ×2.
    pub favor: Taste,
    /// Never these, whatever feedback or the account says.
    pub avoid: Taste,
    /// Always welcome and strongly favored (×3); a pin outranks an avoid.
    pub pin: Items,
    /// Never these.
    pub ban: Items,
}

/// Kinds of music, matched loosely: a genre or artist as a whole phrase
/// ("jazz" finds "cool jazz", "Beatles" finds "The Beatles"), an era as a
/// decade (`"1960s"`, `"60s"`) or a classical period (`"baroque"`), a mood
/// by its name in `moods.toml` or the built-ins.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Taste {
    pub genres: Vec<String>,
    pub artists: Vec<String>,
    pub eras: Vec<String>,
    pub moods: Vec<String>,
}

/// Specific things: artists by name, albums and tracks by Spotify URI
/// (`spotify:album:…`, `spotify:track:…`) or by exact title.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Items {
    pub artists: Vec<String>,
    pub albums: Vec<String>,
    pub tracks: Vec<String>,
}

/// An era: a decade, named by its first year, or a classical period.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Era {
    Decade(u16),
    Period(Period),
}

impl Era {
    /// `"1960s"`, `"60s"`, `"'60s"` and `"1960"` are the sixties (two digits
    /// below 30 are this century: `"20s"` is the 2020s); a period by name,
    /// `"late romantic"` or `"late_romantic"`.
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let norm = normalize(text);
        let digits = norm.trim_end_matches('s').trim_start_matches('\'');
        if !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit()) {
            let year: u16 = digits.parse().ok()?;
            let year = match digits.len() {
                2 if year < 30 => 2000 + year,
                2 => 1900 + year,
                4 => year,
                _ => return None,
            };
            return (year % 10 == 0).then_some(Self::Decade(year));
        }
        PERIODS
            .iter()
            .find(|(name, _)| *name == norm)
            .map(|&(_, period)| Self::Period(period))
    }

    fn holds(self, work: &Work) -> bool {
        match self {
            Self::Decade(decade) => work.year().is_some_and(|y| y - y % 10 == decade),
            Self::Period(period) => work.period == period,
        }
    }
}

const PERIODS: &[(&str, Period)] = &[
    ("medieval", Period::Medieval),
    ("renaissance", Period::Renaissance),
    ("baroque", Period::Baroque),
    ("classical", Period::Classical),
    ("classical era", Period::Classical),
    ("romantic", Period::Romantic),
    ("late romantic", Period::LateRomantic),
    ("impressionist", Period::Impressionist),
    ("modern", Period::Modern),
    ("20th century", Period::Modern),
    ("contemporary", Period::Contemporary),
];

/// The list keys [`Preferences::add`] and [`Preferences::remove`] take, and
/// the two plain ones.
pub const KEYS: &[&str] = &[
    "energy",
    "explicit",
    "favor.genres",
    "favor.artists",
    "favor.eras",
    "favor.moods",
    "avoid.genres",
    "avoid.artists",
    "avoid.eras",
    "avoid.moods",
    "pin.artists",
    "pin.albums",
    "pin.tracks",
    "ban.artists",
    "ban.albums",
    "ban.tracks",
];

impl Preferences {
    /// Parse a `preferences.toml` text and check it against the moods it may
    /// name. `source` names the file in errors, which give the line.
    pub fn parse(text: &str, source: &str, moods: &Moods) -> Result<Self, SpotifyError> {
        let prefs: Self = toml::from_str(text).map_err(|e| {
            let line = e.span().map(|span| crate::steer::line_of(text, span.start));
            SpotifyError::Config(match line {
                Some(line) => format!("{source} line {line}: {}", e.message().trim()),
                None => format!("{source}: {}", e.message().trim()),
            })
        })?;
        prefs
            .validate(moods)
            .map_err(|why| SpotifyError::Config(format!("{source}: {why}")))?;
        Ok(prefs)
    }

    /// Load `path`; a missing file states nothing.
    pub fn load(path: &Path, moods: &Moods) -> Result<Self, SpotifyError> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text, &path.display().to_string(), moods),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e.into()),
        }
    }

    /// Write `path` (through a temporary file beside it, so a crash never
    /// leaves half a file).
    pub fn save(&self, path: &Path) -> Result<(), SpotifyError> {
        let text = self.to_toml()?;
        let tmp = path.with_extension("toml.tmp");
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, path)?;
        Ok(())
    }

    /// As `preferences.toml` text.
    pub fn to_toml(&self) -> Result<String, SpotifyError> {
        toml::to_string(self).map_err(|e| SpotifyError::Config(e.to_string()))
    }

    /// Reject what the DJ can't honor: an energy past 100, a blank entry, an
    /// era or mood it doesn't know, a thing both pinned and banned.
    pub fn validate(&self, moods: &Moods) -> Result<(), String> {
        if let Some(energy) = self.energy.filter(|&e| e > 100) {
            return Err(format!("energy {energy} is outside 0..=100"));
        }
        for (key, list) in self.lists() {
            if list.iter().any(|v| normalize(v).is_empty()) {
                return Err(format!("{key} has a blank entry"));
            }
        }
        for era in self.favor.eras.iter().chain(&self.avoid.eras) {
            if Era::parse(era).is_none() {
                return Err(format!(
                    "unknown era {era:?}: name a decade (\"1960s\", \"90s\") or a period \
                     (\"baroque\", \"late romantic\")"
                ));
            }
        }
        for mood in self.favor.moods.iter().chain(&self.avoid.moods) {
            if moods.get(mood).is_none() {
                let known: Vec<&str> = moods.names().collect();
                return Err(format!("no mood {mood:?} (known: {})", known.join(", ")));
            }
        }
        for ((kind, pinned), banned) in [
            ("artist", &self.pin.artists),
            ("album", &self.pin.albums),
            ("track", &self.pin.tracks),
        ]
        .into_iter()
        .zip([&self.ban.artists, &self.ban.albums, &self.ban.tracks])
        {
            if let Some(both) = pinned
                .iter()
                .find(|p| banned.iter().any(|b| normalize(b) == normalize(p)))
            {
                return Err(format!("{kind} {both:?} is both pinned and banned"));
            }
        }
        Ok(())
    }

    /// Every list, by its key.
    fn lists(&self) -> [(&'static str, &Vec<String>); 14] {
        [
            ("favor.genres", &self.favor.genres),
            ("favor.artists", &self.favor.artists),
            ("favor.eras", &self.favor.eras),
            ("favor.moods", &self.favor.moods),
            ("avoid.genres", &self.avoid.genres),
            ("avoid.artists", &self.avoid.artists),
            ("avoid.eras", &self.avoid.eras),
            ("avoid.moods", &self.avoid.moods),
            ("pin.artists", &self.pin.artists),
            ("pin.albums", &self.pin.albums),
            ("pin.tracks", &self.pin.tracks),
            ("ban.artists", &self.ban.artists),
            ("ban.albums", &self.ban.albums),
            ("ban.tracks", &self.ban.tracks),
        ]
    }

    fn list_mut(&mut self, key: &str) -> Option<&mut Vec<String>> {
        Some(match key {
            "favor.genres" => &mut self.favor.genres,
            "favor.artists" => &mut self.favor.artists,
            "favor.eras" => &mut self.favor.eras,
            "favor.moods" => &mut self.favor.moods,
            "avoid.genres" => &mut self.avoid.genres,
            "avoid.artists" => &mut self.avoid.artists,
            "avoid.eras" => &mut self.avoid.eras,
            "avoid.moods" => &mut self.avoid.moods,
            "pin.artists" => &mut self.pin.artists,
            "pin.albums" => &mut self.pin.albums,
            "pin.tracks" => &mut self.pin.tracks,
            "ban.artists" => &mut self.ban.artists,
            "ban.albums" => &mut self.ban.albums,
            "ban.tracks" => &mut self.ban.tracks,
            _ => return None,
        })
    }

    /// Set a preference: append `value` to a list key (once), or set
    /// `energy` (`0`–`100`) or `explicit` (`true`/`false`). The result is
    /// validated against `moods`; on an error nothing changes.
    pub fn add(&mut self, key: &str, value: &str, moods: &Moods) -> Result<(), String> {
        let mut next = self.clone();
        match key {
            "energy" => {
                next.energy = Some(
                    value
                        .trim()
                        .parse()
                        .map_err(|_| format!("energy {value:?} is not a number 0-100"))?,
                );
            }
            "explicit" => {
                next.explicit = value
                    .trim()
                    .parse()
                    .map_err(|_| format!("explicit {value:?} is not true or false"))?;
            }
            _ => {
                let list = next.list_mut(key).ok_or_else(|| unknown_key(key))?;
                let value = value.trim();
                if !list.iter().any(|v| normalize(v) == normalize(value)) {
                    list.push(value.to_owned());
                }
            }
        }
        next.validate(moods)?;
        *self = next;
        Ok(())
    }

    /// Unset a preference: drop `value` from a list key, or the whole list
    /// with `None`; `energy` and `explicit` go back to their defaults.
    /// Returns whether anything changed.
    pub fn remove(&mut self, key: &str, value: Option<&str>) -> Result<bool, String> {
        let before = self.clone();
        match key {
            "energy" => self.energy = None,
            "explicit" => self.explicit = false,
            _ => {
                let list = self.list_mut(key).ok_or_else(|| unknown_key(key))?;
                match value {
                    Some(value) => list.retain(|v| normalize(v) != normalize(value)),
                    None => list.clear(),
                }
            }
        }
        Ok(*self != before)
    }

    /// How these preferences treat `work`, whose steering haystack is
    /// `haystack` (moods are matched like steering).
    #[must_use]
    pub fn verdict(&self, work: &Work, haystack: &str, moods: &Moods) -> Verdict {
        Verdict {
            banned: self.ban.holds(work),
            avoided: self.avoid.holds(work, haystack, moods),
            pinned: self.pin.holds(work),
            favored: self.favor.holds(work, haystack, moods),
            explicit: work.movements.iter().any(|m| m.explicit),
        }
    }
}

fn unknown_key(key: &str) -> String {
    format!("no preference {key:?} (known: {})", KEYS.join(", "))
}

impl Taste {
    fn holds(&self, work: &Work, haystack: &str, moods: &Moods) -> bool {
        let tags = work.genres();
        self.genres
            .iter()
            .any(|g| tags.iter().any(|t| genre_matches(g, t)))
            || names_any(&self.artists, work)
            || self
                .eras
                .iter()
                .filter_map(|e| Era::parse(e))
                .any(|era| era.holds(work))
            || self
                .moods
                .iter()
                .filter_map(|m| moods.get(m))
                .any(|mood| mood_holds(mood, work, haystack))
    }
}

impl Items {
    fn holds(&self, work: &Work) -> bool {
        let album = work
            .movements
            .first()
            .and_then(|m| m.track.album.as_deref())
            .map(normalize);
        names_any(&self.artists, work)
            || self.albums.iter().any(|a| {
                work.album_uri.as_deref() == Some(a.trim())
                    || album.as_deref().is_some_and(|name| normalize(a) == name)
            })
            || self.tracks.iter().any(|t| {
                let title = normalize(t);
                normalize(&work.title) == title
                    || work.movements.iter().any(|m| {
                        m.track.source_uri == t.trim() || normalize(&m.track.title) == title
                    })
            })
    }
}

fn names_any(artists: &[String], work: &Work) -> bool {
    if artists.is_empty() {
        return false;
    }
    let credits = credited(work);
    artists
        .iter()
        .any(|q| credits.iter().any(|a| artist_matches(q, a)))
}

/// Whether a work fits a mood: its filters admit the work and, when it
/// leans calmer or brighter, the work's energy is on that side. A mood with
/// neither (no filters, no lean) fits nothing.
fn mood_holds(mood: &DjConstraints, work: &Work, haystack: &str) -> bool {
    let filters = DjConstraints {
        energy_bias: 0,
        allow_long_works: false,
        expires_at: None,
        ..mood.clone()
    };
    let filtered = filters != DjConstraints::default();
    if !filtered && mood.energy_bias == 0 {
        return false;
    }
    let energy = work.energy();
    let leans = match mood.energy_bias {
        bias if bias < 0 => energy <= 40,
        bias if bias > 0 => energy >= 60,
        _ => true,
    };
    leans && (!filtered || admits(work, haystack, &filters, &[]))
}

/// How the preferences treat one work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
#[allow(clippy::struct_excessive_bools)] // independent verdicts, not a state machine
pub struct Verdict {
    pub banned: bool,
    pub avoided: bool,
    pub pinned: bool,
    pub favored: bool,
    /// It has an explicit track (out unless `explicit = true`).
    pub explicit: bool,
}

impl Verdict {
    /// The preference weight, per mille: a pin ×3, a favor ×2.
    #[must_use]
    pub fn weight_pm(self) -> u64 {
        if self.pinned {
            PIN_PM
        } else if self.favored {
            FAVOR_PM
        } else {
            1000
        }
    }

    /// Kept out by an avoid (a pin outranks it).
    #[must_use]
    pub fn avoids(self) -> bool {
        self.avoided && !self.pinned
    }

    /// Feedback can't push it below neutral or sit it out.
    #[must_use]
    pub fn shields_feedback(self) -> bool {
        self.pinned || self.favored
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::classical::analyze_song;
    use crate::dj::{DjConfig, Factor, PickContext, PlannedWork, Rng, WorkPool, pick_next};
    use crate::feedback::{FeedbackModel, FeedbackSignal, Signal};
    use crate::library::LibraryItem;
    use crate::steer::Steer;
    use crate::test_shelf::{MIDNIGHT, mixed_items, simulate_with, song_items, works_of};
    use crate::works::group_works;

    const FILE: &str = r#"
energy = 40
explicit = false

[favor]
genres = ["jazz"]
eras = ["1990s"]

[avoid]
artists = ["MC Halcyon"]
moods = ["calm"]

[pin]
tracks = ["spotify:track:song-1-2"]

[ban]
albums = ["Starfall (Original Soundtrack)"]
"#;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|&w| w.to_owned()).collect()
    }

    fn steer(constraints: DjConstraints) -> Steer {
        Steer {
            mood: None,
            constraints,
        }
    }

    #[test]
    fn preferences_parse_validate_edit_and_save() {
        let moods = Moods::builtin();
        let prefs = Preferences::parse(FILE, "preferences.toml", &moods).unwrap();
        assert_eq!(prefs.energy, Some(40));
        assert_eq!(prefs.favor.genres, ["jazz"]);
        assert_eq!(prefs.ban.albums, ["Starfall (Original Soundtrack)"]);
        let again = Preferences::parse(&prefs.to_toml().unwrap(), "again", &moods).unwrap();
        assert_eq!(again, prefs);

        let err = |text: &str| {
            Preferences::parse(text, "preferences.toml", &moods)
                .unwrap_err()
                .to_string()
        };
        let typo = err("[favor]\ngenre = [\"jazz\"]\n");
        assert!(typo.contains("preferences.toml line 2"), "{typo}");
        assert!(err("energy = 140\n").contains("energy 140 is outside 0..=100"));
        assert!(err("[avoid]\neras = [\"jurassic\"]\n").contains("unknown era \"jurassic\""));
        assert!(err("[favor]\nmoods = [\"party\"]\n").contains("no mood \"party\" (known: bright"));
        assert!(
            err("[pin]\nartists = [\"Nina Marsh\"]\n[ban]\nartists = [\"nina marsh\"]\n")
                .contains("artist \"Nina Marsh\" is both pinned and banned")
        );
        assert!(err("[favor]\nartists = [\" \"]\n").contains("favor.artists has a blank entry"));

        let mut edited = Preferences::default();
        edited.add("favor.artists", "Nina Marsh", &moods).unwrap();
        edited.add("favor.artists", " nina marsh ", &moods).unwrap();
        assert_eq!(edited.favor.artists, ["Nina Marsh"], "added once");
        edited.add("energy", "35", &moods).unwrap();
        edited.add("explicit", "true", &moods).unwrap();
        assert_eq!((edited.energy, edited.explicit), (Some(35), true));
        assert!(
            edited
                .add("energy", "loud", &moods)
                .unwrap_err()
                .contains("not a number")
        );
        assert!(
            edited
                .add("favor.colors", "blue", &moods)
                .unwrap_err()
                .contains("no preference \"favor.colors\" (known: energy, explicit")
        );
        assert!(edited.add("avoid.eras", "jurassic", &moods).is_err());
        assert!(
            edited.avoid.eras.is_empty(),
            "a rejected value changes nothing"
        );
        assert_eq!(edited.remove("favor.artists", Some("NINA MARSH")), Ok(true));
        assert_eq!(edited.remove("favor.artists", None), Ok(false));
        assert_eq!(edited.remove("energy", None), Ok(true));
        assert_eq!(edited.remove("explicit", None), Ok(true));
        assert_eq!(edited, Preferences::default());

        let dir = crate::fake_spotify::scratch_dir("prefs");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(PREFERENCES_FILE);
        assert_eq!(
            Preferences::load(&path, &moods).unwrap(),
            Preferences::default()
        );
        prefs.save(&path).unwrap();
        assert_eq!(Preferences::load(&path, &moods).unwrap(), prefs);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn eras_are_decades_or_periods() {
        assert_eq!(Era::parse("1960s"), Some(Era::Decade(1960)));
        assert_eq!(Era::parse("60s"), Some(Era::Decade(1960)));
        assert_eq!(Era::parse("'90s"), Some(Era::Decade(1990)));
        assert_eq!(Era::parse("20s"), Some(Era::Decade(2020)));
        assert_eq!(Era::parse("1960"), Some(Era::Decade(1960)));
        assert_eq!(Era::parse("1965"), None);
        assert_eq!(
            Era::parse("Late-Romantic"),
            Some(Era::Period(Period::LateRomantic))
        );
        assert_eq!(Era::parse("baroque"), Some(Era::Period(Period::Baroque)));
        assert_eq!(Era::parse("jurassic"), None);
    }

    #[test]
    fn verdicts_cover_genres_artists_eras_moods_and_items() {
        let moods = Moods::builtin();
        let prefs = Preferences::parse(FILE, "preferences.toml", &moods).unwrap();
        let pool = works_of(&mixed_items()).with_preferences(prefs, &moods);
        let verdict = |title: &str| {
            let w = pool.works().iter().position(|w| w.title == title).unwrap();
            pool.verdict(w)
        };
        assert!(verdict("Blue Hours").favored, "jazz");
        let skyline = verdict("Skyline");
        assert!(
            skyline.favored && skyline.avoids(),
            "the 1990s, but MC Halcyon"
        );
        assert!(verdict("Every Window").pinned, "by its track URI");
        assert!(verdict("Starfall: The Chase").banned, "by its album's name");
        let plain = verdict("Paper Moons");
        assert_eq!(plain, Verdict::default());
        assert_eq!(plain.weight_pm(), 1000);
        // "calm" leans calmer: the quiet classical openings fit it.
        let quiet = pool.works().iter().position(|w| w.energy() <= 40).unwrap();
        assert!(pool.verdict(quiet).avoids());
        let lively = pool
            .works()
            .iter()
            .position(|w| w.is_classical() && w.energy() > 40)
            .unwrap();
        assert!(!pool.verdict(lively).avoided);
    }

    #[test]
    #[allow(clippy::too_many_lines)] // one scenario per rung of the precedence ladder
    fn steer_outranks_preferences_which_outrank_feedback_which_outranks_the_account() {
        let moods = Moods::builtin();
        let songs = song_items();
        let plain = works_of(&songs);
        let nina = |p: &PlannedWork<'_>| p.work.composer == "Nina Marsh Quartet";
        let love: Vec<FeedbackSignal> = plain
            .works()
            .iter()
            .filter(|w| w.composer == "Nina Marsh Quartet")
            .flat_map(|w| (0..4).map(move |i| FeedbackSignal::about(w, Signal::Like, MIDNIGHT - i)))
            .collect();
        let loved = FeedbackModel::from_signals(&love, MIDNIGHT);
        let config = DjConfig::default();

        // Learned feedback alone: the loved quartet plays often.
        let picks = simulate_with(&plain, &config, 1, 60, Some(20), None, Some(&loved));
        assert!(picks.iter().filter(|p| nina(p)).count() >= 3);
        // Preferences outrank it: avoided, the quartet never plays.
        let avoiding = Preferences {
            avoid: Taste {
                artists: words(&["Nina Marsh"]),
                ..Taste::default()
            },
            ..Preferences::default()
        };
        let pool = works_of(&songs).with_preferences(avoiding, &moods);
        for seed in 1..=4 {
            let picks = simulate_with(&pool, &config, seed, 100, Some(20), None, Some(&loved));
            assert!(!picks.iter().any(nina), "seed {seed}");
        }
        // Steering outranks preferences: asked for by name, only she plays.
        let ask = steer(DjConstraints {
            include_artists: words(&["Nina Marsh"]),
            ..DjConstraints::default()
        });
        let picks = simulate_with(&pool, &config, 1, 12, Some(20), Some(&ask), None);
        assert!(picks.iter().all(nina));
        // …and a steer that excludes a pinned artist keeps her out.
        let pinning = Preferences {
            pin: Items {
                artists: words(&["Juniper Vale"]),
                ..Items::default()
            },
            ..Preferences::default()
        };
        let pinned = works_of(&songs).with_preferences(pinning, &moods);
        let without = steer(DjConstraints {
            exclude_artists: words(&["Juniper Vale"]),
            ..DjConstraints::default()
        });
        let picks = simulate_with(&pinned, &config, 1, 40, Some(20), Some(&without), None);
        assert!(
            picks
                .iter()
                .all(|p| credited(p.work).iter().all(|a| a != "Juniper Vale"))
        );

        // In the weights of one pick: the single the owner liked on Spotify
        // (the account's signal), then twice disliked (feedback), alone under
        // a steer that finds it.
        let single = plain
            .works()
            .iter()
            .find(|w| w.title.starts_with("Kite Season"))
            .unwrap();
        let hated = FeedbackModel::from_signals(
            &[
                FeedbackSignal::about(single, Signal::Dislike, MIDNIGHT - 10),
                FeedbackSignal::about(single, Signal::Dislike, MIDNIGHT - 5),
            ],
            MIDNIGHT,
        );
        let only = steer(DjConstraints {
            include_keywords: words(&["kite season"]),
            ..DjConstraints::default()
        });
        let narrow = DjConfig {
            min_steered_works: 1,
            ..DjConfig::default()
        };
        let factors = |pool: &WorkPool| {
            let ctx = PickContext {
                steer: Some(&only),
                feedback: Some(&hated),
                ..PickContext::default()
            };
            let pick = pick_next(pool, &ctx, &narrow, &mut Rng::new(1)).unwrap();
            assert!(pick.work.title.starts_with("Kite Season"));
            pick.reason.factors
        };
        let pm = |factors: &[(Factor, i32)], factor: Factor| {
            factors
                .iter()
                .find(|(f, _)| *f == factor)
                .map_or(1000, |&(_, pm)| i64::from(pm))
        };
        let account = factors(&plain);
        assert_eq!(pm(&account, Factor::Liked), 1300);
        assert!(
            pm(&account, Factor::Liked) * pm(&account, Factor::Feedback) < 1000 * 1000,
            "feedback outweighs the like: {account:?}"
        );
        let favoring = Preferences {
            favor: Taste {
                artists: words(&["Juniper Vale"]),
                ..Taste::default()
            },
            ..Preferences::default()
        };
        let favored = factors(&works_of(&songs).with_preferences(favoring, &moods));
        assert_eq!(pm(&favored, Factor::Preference), 2000);
        assert!(
            pm(&favored, Factor::Feedback) >= 1000,
            "feedback can't push a favored work below neutral: {favored:?}"
        );
    }

    #[test]
    fn bans_hold_explicit_waits_avoids_relax_and_energy_is_preferred() {
        let moods = Moods::builtin();
        let mut items = song_items();
        let explicit: Vec<LibraryItem> = items.iter().filter(|i| i.explicit).cloned().collect();
        items.retain(|i| !i.explicit && i.artists[0] == "MC Halcyon");
        let mut tracks: Vec<_> = items.iter().map(analyze_song).collect();
        tracks.extend(explicit.iter().map(analyze_song));
        let works = group_works(&tracks);
        let config = DjConfig::default();
        let played_explicit = |pool: &WorkPool| {
            simulate_with(pool, &config, 2, 30, Some(20), None, None)
                .iter()
                .any(|p| p.work.title == "Back Block")
        };
        assert!(!played_explicit(&WorkPool::from_works(works.clone())));
        let allowing = Preferences {
            explicit: true,
            ..Preferences::default()
        };
        assert!(played_explicit(
            &WorkPool::from_works(works.clone()).with_preferences(allowing, &moods)
        ));

        // A ban never relaxes, even when it leaves nothing.
        let banning = Preferences {
            ban: Items {
                artists: words(&["MC Halcyon"]),
                ..Items::default()
            },
            ..Preferences::default()
        };
        let banned = WorkPool::from_works(works.clone()).with_preferences(banning, &moods);
        let ctx = PickContext::default();
        assert!(pick_next(&banned, &ctx, &config, &mut Rng::new(1)).is_none());
        // An avoid relaxes rather than play nothing, and says so.
        let avoiding = Preferences {
            avoid: Taste {
                genres: words(&["hip hop"]),
                ..Taste::default()
            },
            energy: Some(25),
            ..Preferences::default()
        };
        let avoided = WorkPool::from_works(works).with_preferences(avoiding, &moods);
        let ctx = PickContext {
            local_hour: Some(10),
            ..PickContext::default()
        };
        let pick = pick_next(&avoided, &ctx, &config, &mut Rng::new(1)).unwrap();
        assert!(pick.reason.has(Factor::Avoided));
        let summary = &pick.reason.summary;
        assert!(summary.contains("calm energy, as you prefer"), "{summary}");
        assert!(
            summary.contains("everything else left is avoided in your preferences"),
            "{summary}"
        );
    }
}
