//! The owner's standing DJ preferences on every surface: `fsonos dj prefs`,
//! `GET`/`POST /dj/preferences`, and the `dj_preferences` / `dj_prefer`
//! tools.
//!
//! They live in `preferences.toml` in the data directory, beside
//! `moods.toml`, and hold for every group's DJ until changed: genres,
//! artists, eras and moods to favor or avoid, a default energy, whether
//! explicit tracks may play, and artists, albums or tracks to pin or ban.
//! Steering a group outranks them, and they outrank learned feedback. A ban
//! never relaxes; an avoid relaxes only when nothing else is left.
//!
//! fsonos-spotify keeps the model, behind the DJ engine. A change is
//! checked before the file is written, applies from the DJ's next pick, and
//! is logged. It is not undoable (undo restores speakers and steering); an
//! unset reverses a set.

use fastapi::{JsonSchema, fastapi_openapi};
use fsonos_core::policy::Client;
use serde::{Deserialize, Serialize};
use std::fmt::Write as _;

use super::Surface;
use crate::failure::Failure;

/// The preference keys: two plain ones and the lists.
pub const PREFERENCE_KEYS: &[&str] = &[
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

/// Longest preference value accepted, in characters.
const MAX_VALUE: usize = 256;

/// `preferences.toml` as the surfaces show it: fsonos-spotify's
/// `Preferences`, field for field.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct PreferencesDto {
    /// The energy (0-100) the DJ aims for when no steer sets one, in place
    /// of the time-of-day curve.
    pub energy: Option<u8>,
    /// Whether explicit tracks may play.
    pub explicit: bool,
    /// Lean toward these: a matching work weighs twice as much.
    pub favor: TasteDto,
    /// Never these, unless nothing else is left.
    pub avoid: TasteDto,
    /// Always welcome and favored three times over; a pin outranks an avoid.
    pub pin: ItemsDto,
    /// Never these.
    pub ban: ItemsDto,
}

/// Kinds of music: genres and artists as whole phrases ("jazz" finds "cool
/// jazz"), eras as decades ("1960s") or classical periods ("baroque"), moods
/// by name.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct TasteDto {
    pub genres: Vec<String>,
    pub artists: Vec<String>,
    pub eras: Vec<String>,
    pub moods: Vec<String>,
}

/// Specific things: artists by name, albums and tracks by Spotify URI or
/// exact title.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default)]
pub struct ItemsDto {
    pub artists: Vec<String>,
    pub albums: Vec<String>,
    pub tracks: Vec<String>,
}

/// `POST /dj/preferences` body: set (add to a list) or unset one preference.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct DjPreferRequest {
    /// `energy`, `explicit`, or a list key: favor.genres, favor.artists,
    /// favor.eras, favor.moods, avoid.(same), pin.artists, pin.albums,
    /// pin.tracks, ban.(same).
    pub key: String,
    /// What to add (a list key) or set (energy 0-100, explicit true or
    /// false). With `unset`, the one item to drop; absent, the whole list
    /// goes (energy and explicit go back to their defaults).
    #[serde(default)]
    pub value: Option<String>,
    /// Remove instead of add.
    #[serde(default)]
    pub unset: bool,
}

/// The change a request asks for, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PrefChange {
    Set { key: String, value: String },
    Unset { key: String, value: Option<String> },
}

impl DjPreferRequest {
    /// The change asked for, or why it cannot be made.
    pub fn change(&self) -> Result<PrefChange, Failure> {
        let key = self.key.trim().to_lowercase();
        if !PREFERENCE_KEYS.contains(&key.as_str()) {
            return Err(
                Failure::invalid(format!("no preference {:?}", self.key.trim()))
                    .with_suggestions(PREFERENCE_KEYS.iter().copied()),
            );
        }
        let value = self
            .value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned);
        if value
            .as_ref()
            .is_some_and(|v| v.chars().count() > MAX_VALUE)
        {
            return Err(Failure::invalid(format!(
                "`value` is longer than {MAX_VALUE} characters"
            )));
        }
        if self.unset {
            return Ok(PrefChange::Unset { key, value });
        }
        let Some(value) = value else {
            return Err(Failure::invalid(format!("`value` is needed to set {key}"))
                .with_hint("Give the value to add, or unset true to remove one."));
        };
        Ok(PrefChange::Set { key, value })
    }
}

/// `POST /dj/preferences` answer.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct PreferredDto {
    /// One line: what changed.
    pub done: String,
    pub changed: bool,
    /// The preferences now.
    pub preferences: PreferencesDto,
}

impl PreferencesDto {
    /// The preferences in a few lines ("No preferences stated." for none).
    #[must_use]
    pub fn text(&self) -> String {
        if *self == Self::default() {
            return "No preferences stated: the DJ follows the library, the time of day and \
                    your feedback.\n"
                .to_owned();
        }
        let mut text = String::new();
        if let Some(energy) = self.energy {
            let _ = writeln!(text, "Energy: {energy} (in place of the time of day).");
        }
        let _ = writeln!(
            text,
            "Explicit tracks: {}.",
            if self.explicit { "allowed" } else { "kept out" }
        );
        for (label, taste) in [("Favor", &self.favor), ("Avoid", &self.avoid)] {
            let lists = [
                ("genres", &taste.genres),
                ("artists", &taste.artists),
                ("eras", &taste.eras),
                ("moods", &taste.moods),
            ];
            line(&mut text, label, &lists);
        }
        for (label, items) in [("Pin", &self.pin), ("Ban", &self.ban)] {
            let lists = [
                ("artists", &items.artists),
                ("albums", &items.albums),
                ("tracks", &items.tracks),
            ];
            line(&mut text, label, &lists);
        }
        text
    }
}

/// "Favor: genres jazz, soul; artists Miles Davis." when any list has items.
fn line(text: &mut String, label: &str, lists: &[(&str, &Vec<String>)]) {
    let parts: Vec<String> = lists
        .iter()
        .filter(|(_, items)| !items.is_empty())
        .map(|(name, items)| format!("{name} {}", items.join(", ")))
        .collect();
    if !parts.is_empty() {
        let _ = writeln!(text, "{label}: {}.", parts.join("; "));
    }
}

impl Surface {
    /// The owner's standing DJ preferences (`dj_preferences`).
    pub fn dj_preferences(&self, client: &Client) -> Result<PreferencesDto, Failure> {
        self.guard(client).authorize("dj_preferences", true)?;
        self.dj_engine()?.preferences()
    }

    /// Set or unset one standing preference (`dj_prefer`), logged.
    pub fn dj_prefer(&self, client: &Client, req: &DjPreferRequest) -> Result<PreferredDto, Failure> {
        self.authorize_write(client, "dj_prefer")?;
        let change = req.change()?;
        let result = self.dj_engine()?.prefer(&change);
        let text = match &result {
            Ok(done) => done.done.clone(),
            Err(f) => format!("failed: {}", f.detail),
        };
        self.record(
            client,
            format!("dj_prefer: {change:?}"),
            "allow".to_owned(),
            text,
            None,
        );
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ErrorCode;

    fn change(key: &str, value: Option<&str>, unset: bool) -> Result<PrefChange, Failure> {
        DjPreferRequest {
            key: key.into(),
            value: value.map(str::to_owned),
            unset,
        }
        .change()
    }

    #[test]
    fn a_change_names_a_known_key_and_a_value_to_set() {
        assert_eq!(
            change(" Favor.Genres ", Some(" jazz "), false).unwrap(),
            PrefChange::Set {
                key: "favor.genres".into(),
                value: "jazz".into()
            }
        );
        assert_eq!(
            change("ban.artists", None, true).unwrap(),
            PrefChange::Unset {
                key: "ban.artists".into(),
                value: None
            }
        );
        let unknown = change("favour.genres", Some("jazz"), false).unwrap_err();
        assert_eq!(unknown.code, ErrorCode::InvalidArgument);
        assert!(unknown.suggestions.iter().any(|s| s == "favor.genres"));
        let empty = change("energy", Some("  "), false).unwrap_err();
        assert!(empty.detail.contains("`value` is needed"), "{}", empty.detail);
        assert!(
            serde_json::from_value::<DjPreferRequest>(serde_json::json!({ "key": "energy", "set": 3 }))
                .is_err()
        );
    }

    #[test]
    fn preferences_read_as_lines() {
        assert!(PreferencesDto::default().text().starts_with("No preferences stated"));
        let shown = PreferencesDto {
            energy: Some(30),
            favor: TasteDto {
                genres: vec!["jazz".into(), "soul".into()],
                ..TasteDto::default()
            },
            ban: ItemsDto {
                artists: vec!["Johannes Brahms".into()],
                ..ItemsDto::default()
            },
            ..PreferencesDto::default()
        };
        assert_eq!(
            shown.text(),
            "Energy: 30 (in place of the time of day).\n\
             Explicit tracks: kept out.\n\
             Favor: genres jazz, soul.\n\
             Ban: artists Johannes Brahms.\n"
        );
    }
}
