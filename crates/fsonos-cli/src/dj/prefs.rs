//! The owner's standing preferences for the DJ: `preferences.toml` beside
//! `moods.toml`, read with fsonos-spotify's `Preferences` and laid on the
//! DJ's pool on each start and skip, and right after a change made through
//! this DJ (so a change through the daemon's API applies from its next
//! pick; one made by another process, from the daemon's next start or
//! skip).

use clap::Subcommand;
use fsonos_api::surface::dj_prefs::{DjPreferRequest, PrefChange, PreferencesDto, PreferredDto};
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::policy::Client;
use fsonos_spotify::prefs::{PREFERENCES_FILE, Preferences};
use std::path::{Path, PathBuf};

use super::State;
use crate::config::GlobalArgs;

/// `fsonos dj prefs …`.
#[derive(Debug, Clone, Subcommand)]
pub enum PrefsAction {
    /// The standing preferences.
    Show,
    /// Add to a list, or set energy (0-100) or explicit (true/false): `set
    /// favor.genres jazz`, `set ban.artists Nickelback`, `set energy 30`.
    Set { key: String, value: String },
    /// Drop one item from a list, or the whole list without a value; energy
    /// and explicit go back to their defaults.
    Unset { key: String, value: Option<String> },
}

/// Where the preferences live: beside `moods.toml`.
fn file(moods_file: Option<&PathBuf>) -> Option<PathBuf> {
    moods_file.map(|m| m.with_file_name(PREFERENCES_FILE))
}

fn no_data_dir() -> Failure {
    Failure::new(
        ErrorCode::NotImplemented,
        "the DJ has no data directory to keep preferences in",
    )
    .with_hint("Set FSONOS_DATA_DIR (or HOME).")
}

fn load(path: &Path, state: &State) -> Result<Preferences, Failure> {
    Preferences::load(path, &state.moods).map_err(|e| {
        Failure::invalid(e.to_string()).with_hint("Fix preferences.toml in the data directory.")
    })
}

/// The preferences as the surfaces show them: the same fields, so serde
/// carries them across.
fn dto(prefs: &Preferences) -> PreferencesDto {
    serde_json::to_value(prefs)
        .and_then(serde_json::from_value)
        .unwrap_or_default()
}

/// Lay the preferences on the DJ's pool (on each start and skip).
pub(super) fn apply(state: &mut State, moods_file: Option<&PathBuf>) -> Result<(), Failure> {
    let Some(path) = file(moods_file) else {
        return Ok(());
    };
    let prefs = load(&path, state)?;
    state.pool = std::mem::take(&mut state.pool).with_preferences(prefs, &state.moods);
    Ok(())
}

/// The preferences now (`dj_preferences`).
pub(super) fn show(
    state: &mut State,
    moods_file: Option<&PathBuf>,
) -> Result<PreferencesDto, Failure> {
    state.reload_moods(moods_file)?;
    let path = file(moods_file).ok_or_else(no_data_dir)?;
    Ok(dto(&load(&path, state)?))
}

/// Set or unset one preference (`dj_prefer`): the file first, then the pool.
pub(super) fn change(
    state: &mut State,
    moods_file: Option<&PathBuf>,
    change: &PrefChange,
) -> Result<PreferredDto, Failure> {
    state.reload_moods(moods_file)?;
    let path = file(moods_file).ok_or_else(no_data_dir)?;
    let mut prefs = load(&path, state)?;
    let (changed, done) = match change {
        PrefChange::Set { key, value } => {
            let before = prefs.clone();
            prefs
                .add(key, value, &state.moods)
                .map_err(Failure::invalid)?;
            if prefs == before {
                (false, format!("{key} already holds {value}"))
            } else {
                (true, format!("{key}: {value}"))
            }
        }
        PrefChange::Unset { key, value } => {
            let what = value
                .as_deref()
                .map_or_else(|| key.clone(), |v| format!("{v} from {key}"));
            if prefs
                .remove(key, value.as_deref())
                .map_err(Failure::invalid)?
            {
                (true, format!("removed {what}"))
            } else {
                (false, format!("nothing to remove: {what}"))
            }
        }
    };
    if changed {
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| {
                Failure::new(ErrorCode::Internal, format!("the data directory: {e}"))
            })?;
        }
        prefs
            .save(&path)
            .map_err(|e| Failure::new(ErrorCode::Internal, format!("preferences.toml: {e}")))?;
        state.pool = std::mem::take(&mut state.pool).with_preferences(prefs.clone(), &state.moods);
    }
    Ok(PreferredDto {
        done: if changed {
            format!("{done}; the DJ follows it from its next pick")
        } else {
            done
        },
        changed,
        preferences: dto(&prefs),
    })
}

/// `fsonos dj prefs show|set|unset`, through the same surface the daemon's
/// API uses. The file is shared, so a running daemon's DJ follows a change
/// from its next start or skip.
pub fn run(global: &GlobalArgs, action: &PrefsAction) -> anyhow::Result<()> {
    let dir = crate::daemon::data_dir(global)?;
    let surface = crate::daemon::surface(global, crate::daemon::policy(&dir)?)?;
    let surface = crate::daemon::with_action_log(surface, &dir, "cli");
    let req = |key: &String, value: Option<&String>, unset: bool| DjPreferRequest {
        key: key.clone(),
        value: value.cloned(),
        unset,
    };
    let preferred = match action {
        PrefsAction::Show => {
            let shown = surface.dj_preferences(&Client::Cli)?;
            return crate::emit(global.json, &shown, PreferencesDto::text);
        }
        PrefsAction::Set { key, value } => {
            surface.dj_prefer(&Client::Cli, &req(key, Some(value), false))?
        }
        PrefsAction::Unset { key, value } => {
            surface.dj_prefer(&Client::Cli, &req(key, value.as_ref(), true))?
        }
    };
    crate::emit(global.json, &preferred, |p: &PreferredDto| {
        format!("{}\n", p.done)
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_spotify::steer::Moods;

    /// A state with the built-in moods, and a fresh data directory (its
    /// moods.toml absent, so the built-ins).
    fn fresh(name: &str) -> (State, PathBuf) {
        let dir = std::env::temp_dir().join(format!(
            "fsonos-prefs-{}-{name}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let state = State {
            moods: Moods::builtin(),
            ..State::default()
        };
        (state, dir.join("moods.toml"))
    }

    fn set(key: &str, value: &str) -> PrefChange {
        PrefChange::Set {
            key: key.into(),
            value: value.into(),
        }
    }

    #[test]
    fn every_field_crosses_to_the_surfaces() {
        let mut prefs = Preferences::default();
        let moods = Moods::builtin();
        for (key, value) in [
            ("energy", "30"),
            ("explicit", "true"),
            ("favor.genres", "jazz"),
            ("favor.eras", "1960s"),
            ("avoid.moods", "calm"),
            ("pin.albums", "spotify:album:0123456789ABCDEFabcdef"),
            ("ban.artists", "Nickelback"),
        ] {
            prefs.add(key, value, &moods).unwrap();
        }
        let shown = dto(&prefs);
        assert_eq!(shown.energy, Some(30));
        assert!(shown.explicit);
        assert_eq!(shown.favor.genres, ["jazz"]);
        assert_eq!(shown.favor.eras, ["1960s"]);
        assert_eq!(shown.avoid.moods, ["calm"]);
        assert_eq!(shown.pin.albums, ["spotify:album:0123456789ABCDEFabcdef"]);
        assert_eq!(shown.ban.artists, ["Nickelback"]);
    }

    #[test]
    fn a_change_is_written_and_shown_and_an_unset_reverses_it() {
        let (mut state, moods_file) = fresh("change");
        let moods = Some(&moods_file);
        assert_eq!(show(&mut state, moods).unwrap(), PreferencesDto::default());

        let done = change(&mut state, moods, &set("ban.artists", "Johannes Brahms")).unwrap();
        assert!(done.changed, "{}", done.done);
        assert!(
            done.done.starts_with("ban.artists: Johannes Brahms"),
            "{}",
            done.done
        );
        assert!(moods_file.with_file_name(PREFERENCES_FILE).exists());
        assert_eq!(
            show(&mut state, moods).unwrap().ban.artists,
            ["Johannes Brahms"]
        );
        assert_eq!(
            state.pool.preferences().ban.artists,
            ["Johannes Brahms"],
            "the pool follows the change"
        );

        let again = change(&mut state, moods, &set("ban.artists", "johannes brahms")).unwrap();
        assert!(!again.changed, "{}", again.done);

        let unset = PrefChange::Unset {
            key: "ban.artists".into(),
            value: Some("Johannes Brahms".into()),
        };
        assert!(change(&mut state, moods, &unset).unwrap().changed);
        let twice = change(&mut state, moods, &unset).unwrap();
        assert!(!twice.changed && twice.done.starts_with("nothing to remove"));
        assert_eq!(show(&mut state, moods).unwrap(), PreferencesDto::default());
    }

    #[test]
    fn a_value_the_model_refuses_is_invalid_and_changes_nothing() {
        let (mut state, moods_file) = fresh("refused");
        let moods = Some(&moods_file);
        let err = change(&mut state, moods, &set("energy", "150")).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{}", err.detail);
        let err = change(&mut state, moods, &set("favor.moods", "no-such-mood")).unwrap_err();
        assert_eq!(err.code, ErrorCode::InvalidArgument, "{}", err.detail);
        assert!(!moods_file.with_file_name(PREFERENCES_FILE).exists());
        assert!(matches!(show(&mut state, None), Err(f) if f.code == ErrorCode::NotImplemented));
    }
}
