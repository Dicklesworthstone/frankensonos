//! The owner's standing preferences for the DJ: `preferences.toml` beside
//! `moods.toml`, read with fsonos-spotify's `Preferences` and laid on the
//! DJ's pool on each start and skip, and right after a change made through
//! this DJ (so a change through the daemon's API, `fsonos dj prefs` with
//! the daemon up included, applies from its next pick). Automatic feeding
//! checks external edits every 30 seconds; explicit commands check at once.

use clap::Subcommand;
use fsonos_api::surface::dj_prefs::{DjPreferRequest, PrefChange, PreferencesDto, PreferredDto};
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::policy::Client;
use fsonos_spotify::SpotifyError;
use fsonos_spotify::prefs::{PREFERENCES_FILE, PreferenceWriteError, Preferences};
use fsonos_spotify::steer::Moods;
use std::path::{Path, PathBuf};

use super::{State, read_moods};
use crate::config::GlobalArgs;
use crate::remote::Daemon;

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
pub(super) fn file(moods_file: Option<&PathBuf>) -> Option<PathBuf> {
    moods_file.map(|m| m.with_file_name(PREFERENCES_FILE))
}

fn no_data_dir() -> Failure {
    Failure::new(
        ErrorCode::NotImplemented,
        "the DJ has no data directory to keep preferences in",
    )
    .with_hint("Set FSONOS_DATA_DIR (or HOME).")
}

fn load(path: &Path, moods: &Moods) -> Result<Preferences, Failure> {
    Preferences::load(path, moods).map_err(|e| {
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

/// Lay the preferences on the DJ's pool (on each rebuild of it).
pub(super) fn apply(state: &mut State, moods_file: Option<&PathBuf>) -> Result<(), Failure> {
    let Some(path) = file(moods_file) else {
        return Ok(());
    };
    let prefs = load(&path, &state.moods)?;
    state.pool = std::mem::take(&mut state.pool).with_preferences(prefs, &state.moods);
    Ok(())
}

/// The preferences now (`dj_preferences`).
pub(super) fn show(moods_file: Option<&PathBuf>) -> Result<PreferencesDto, Failure> {
    let moods = read_moods(moods_file)?;
    let path = file(moods_file).ok_or_else(no_data_dir)?;
    Ok(dto(&load(&path, &moods)?))
}

/// Set or unset one preference (`dj_prefer`): the file first, then the pool.
pub(super) fn change(
    state: &mut State,
    moods_file: Option<&PathBuf>,
    change: &PrefChange,
) -> Result<PreferredDto, Failure> {
    let moods = read_moods(moods_file)?;
    let path = file(moods_file).ok_or_else(no_data_dir)?;
    let (prefs, (changed, done)) = Preferences::edit(&path, &moods, |prefs| {
        Ok(match change {
            PrefChange::Set { key, value } => {
                let before = prefs.clone();
                prefs.add(key, value, &moods)?;
                if *prefs == before {
                    (false, format!("{key} already holds {value}"))
                } else {
                    (true, format!("{key}: {value}"))
                }
            }
            PrefChange::Unset { key, value } => {
                let what = value
                    .as_deref()
                    .map_or_else(|| key.clone(), |v| format!("{v} from {key}"));
                if prefs.remove(key, value.as_deref())? {
                    (true, format!("removed {what}"))
                } else {
                    (false, format!("nothing to remove: {what}"))
                }
            }
        })
    })
    .map_err(|error| write_failure(state, error))?;
    state.pool = std::mem::take(&mut state.pool).with_preferences(prefs.clone(), &moods);
    state.moods = moods;
    // Even a no-op edit installs freshly read configuration in this pool.
    // Its old fingerprint cannot certify those inputs if another process
    // restores the old file before the next start or readiness check.
    state.planned = false;
    state.plan_inputs = None;
    state.plan_check_at = None;
    state.plan_retry_at = None;
    state.refill_retries.clear();
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

fn write_failure(state: &mut State, error: PreferenceWriteError) -> Failure {
    match error {
        PreferenceWriteError::Spotify(SpotifyError::Config(detail)) => Failure::invalid(detail)
            .with_hint("Check the preference value and preferences.toml in the data directory."),
        error @ PreferenceWriteError::Publish {
            published: true, ..
        } => {
            state.planned = false;
            state.plan_inputs = None;
            state.plan_check_at = None;
            state.plan_retry_at = None;
            state.refill_retries.clear();
            Failure::new(ErrorCode::OutcomeUncertain, error.to_string()).with_hint(
                "The preferences were published but their durability could not be confirmed. Read /dj/preferences before deciding on another edit; do not replay this write.",
            )
        }
        error => Failure::new(ErrorCode::Internal, error.to_string()).with_hint(
            "Check access to the data directory, then read /dj/preferences before editing again.",
        ),
    }
}

/// `fsonos dj prefs show|set|unset`, through the same surface the daemon's
/// API uses: through the daemon when one answers, so its DJ follows a
/// change from its next pick, else right here (the file is shared, so a
/// running daemon's next automatic file check picks it up).
pub fn run(global: &GlobalArgs, action: &PrefsAction) -> anyhow::Result<()> {
    let req = |key: &String, value: Option<&String>, unset: bool| DjPreferRequest {
        key: key.clone(),
        value: value.cloned(),
        unset,
    };
    let change = match action {
        PrefsAction::Show => None,
        PrefsAction::Set { key, value } => Some(req(key, Some(value), false)),
        PrefsAction::Unset { key, value } => Some(req(key, value.as_ref(), true)),
    };
    let text = |p: &PreferredDto| format!("{}\n", p.done);
    if let Some(daemon) = Daemon::find(global, global.daemon)? {
        return match &change {
            None => {
                let shown: PreferencesDto = daemon.get("/dj/preferences")?;
                crate::emit(global.json, &shown, PreferencesDto::text)
            }
            Some(change) => {
                let preferred: PreferredDto = daemon.post("/dj/preferences", change)?;
                crate::emit(global.json, &preferred, text)
            }
        };
    }
    let dir = crate::daemon::data_dir(global)?;
    let surface = crate::daemon::surface(global, crate::daemon::policy(&dir)?)?;
    let surface = crate::daemon::with_action_log(surface, &dir, "cli");
    match &change {
        None => {
            let shown = surface.dj_preferences(&Client::Cli)?;
            crate::emit(global.json, &shown, PreferencesDto::text)
        }
        Some(change) => {
            let preferred = surface.dj_prefer(&Client::Cli, change)?;
            crate::emit(global.json, &preferred, text)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fsonos_core::store::MemStore;
    use fsonos_spotify::steer::Moods;
    use std::time::{Duration, Instant};

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
        assert_eq!(show(moods).unwrap(), PreferencesDto::default());

        let done = change(&mut state, moods, &set("ban.artists", "Johannes Brahms")).unwrap();
        assert!(done.changed, "{}", done.done);
        assert!(
            done.done.starts_with("ban.artists: Johannes Brahms"),
            "{}",
            done.done
        );
        assert!(moods_file.with_file_name(PREFERENCES_FILE).exists());
        assert_eq!(show(moods).unwrap().ban.artists, ["Johannes Brahms"]);
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
        assert_eq!(show(moods).unwrap(), PreferencesDto::default());
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
        assert!(matches!(show(None), Err(f) if f.code == ErrorCode::NotImplemented));
    }

    #[test]
    fn failed_plan_rebuild_keeps_bans_and_retries_after_configuration_is_repaired() {
        for broken in [PREFERENCES_FILE, "moods.toml"] {
            let (mut state, moods_file) = fresh(broken);
            let moods = Some(&moods_file);
            let store = MemStore::default();
            assert!(state.reload_due(Instant::now()));
            change(&mut state, moods, &set("ban.artists", "Blocked Artist")).unwrap();
            state.reload(&store, moods, 100).unwrap();
            let preferences = state.pool.preferences().clone();
            let presets = state.moods.clone();
            assert!(state.planned);
            assert!(!state.reload_due(Instant::now()));

            let path = moods_file.with_file_name(broken);
            std::fs::write(&path, "not = [valid").unwrap();
            assert!(state.reload(&store, moods, 101).is_err());
            assert_eq!(state.pool.preferences(), &preferences);
            assert_eq!(state.moods, presets);
            assert!(!state.planned);
            let due = state
                .plan_retry_at
                .expect("the rebuild must remain pending");
            let before = due.checked_sub(Duration::from_secs(1)).unwrap();
            assert!(!state.reload_due(before));
            assert!(state.reload_due(due));

            if broken == PREFERENCES_FILE {
                preferences.save(&path).unwrap();
            } else {
                std::fs::write(&path, "").unwrap();
            }
            state.reload(&store, moods, 102).unwrap();
            assert_eq!(state.pool.preferences(), &preferences);
            assert_eq!(state.moods, presets);
            assert!(state.planned);
            assert_eq!(state.plan_retry_at, None);
            assert!(!state.reload_due(due));
        }
    }

    #[test]
    fn invalid_preferences_do_not_partially_publish_new_moods_on_first_reload() {
        let (mut state, moods_file) = fresh("first-reload");
        let before = state.moods.clone();
        std::fs::write(
            &moods_file,
            "[moods.fixture]\ninclude_genres = [\"jazz\"]\n",
        )
        .unwrap();
        assert_ne!(Moods::load(&moods_file).unwrap(), before);
        std::fs::write(
            moods_file.with_file_name(PREFERENCES_FILE),
            "explicit = \"not a boolean\"\n",
        )
        .unwrap();
        let store = MemStore::default();
        assert!(state.reload(&store, Some(&moods_file), 100).is_err());
        assert_eq!(state.moods, before);
        assert_eq!(state.pool.preferences(), &Preferences::default());
        assert!(!state.planned);
        assert!(state.plan_retry_at.is_some());
    }

    #[test]
    fn successful_plan_rebuild_keeps_runtime_session_bookkeeping() {
        use fsonos_spotify::dj::DjConfig;
        use fsonos_spotify::feed::QueueFeed;
        use fsonos_types::PlayerId;

        let (mut state, moods_file) = fresh("runtime-state");
        let id = PlayerId("test-plan-player".into());
        let feed = QueueFeed::new(&id, DjConfig::default(), 1);
        state.feeds.insert(id.clone(), feed);
        state.held.insert(id.clone());
        state.listening.entry(id.clone()).or_default();
        state.saved = "saved-feed-sentinel".into();
        let store = MemStore::default();
        state.reload(&store, Some(&moods_file), 100).unwrap();
        assert!(state.feeds.contains_key(&id));
        assert!(state.held.contains(&id));
        assert!(state.listening.contains_key(&id));
        assert_eq!(state.saved, "saved-feed-sentinel");
        assert!(state.planned);
    }

    #[test]
    fn independent_engines_keep_unrelated_preference_edits() {
        use std::collections::BTreeSet;
        use std::sync::{Arc, Barrier};

        let (_, moods_file) = fresh("concurrent-edits");
        let path = moods_file.with_file_name(PREFERENCES_FILE);
        std::fs::write(&path, "energy = 25\n[ban]\nartists = ['Fixture Blocked']\n").unwrap();
        let ready = Arc::new(Barrier::new(4));
        let workers: Vec<_> = (0..4)
            .map(|index| {
                let moods_file = moods_file.clone();
                let ready = Arc::clone(&ready);
                std::thread::spawn(move || {
                    let mut state = State {
                        moods: Moods::builtin(),
                        ..State::default()
                    };
                    let value = format!("Fixture Artist {index}");
                    ready.wait();
                    let saved =
                        change(&mut state, Some(&moods_file), &set("favor.artists", &value))
                            .unwrap();
                    assert!(saved.changed);
                    assert!(saved.preferences.favor.artists.contains(&value));
                    assert_eq!(saved.preferences.ban.artists, ["Fixture Blocked"]);
                    assert_eq!(state.pool.preferences().ban.artists, ["Fixture Blocked"]);
                })
            })
            .collect();
        for worker in workers {
            worker.join().unwrap();
        }
        let accepted = show(Some(&moods_file)).unwrap();
        assert_eq!(accepted.energy, Some(25));
        assert_eq!(accepted.ban.artists, ["Fixture Blocked"]);
        assert_eq!(
            accepted.favor.artists.into_iter().collect::<BTreeSet<_>>(),
            (0..4)
                .map(|index| format!("Fixture Artist {index}"))
                .collect::<BTreeSet<_>>()
        );
    }

    #[test]
    fn preference_edits_and_noops_cannot_reuse_an_older_configuration_fingerprint() {
        use super::super::SpotifyDj;
        use fsonos_api::dj::DjEngine;

        for no_op in [false, true] {
            let (_, moods_file) = fresh(if no_op {
                "fingerprint-no-op"
            } else {
                "fingerprint-edit"
            });
            let moods = Some(&moods_file);
            let path = moods_file.with_file_name(PREFERENCES_FILE);
            let original = "energy = 25\n";
            std::fs::write(&path, original).unwrap();
            let store = MemStore::default();
            let dj = SpotifyDj::new(Some(moods_file.clone()));
            dj.state().reload(&store, moods, 100).unwrap();
            let original_inputs = dj.state().plan_inputs;
            assert!(dj.state().plan_is_current(&store, moods).unwrap());

            // In the no-op case another process has already installed B.
            // prefer still reads B and installs it in the in-memory pool.
            if no_op {
                std::fs::write(&path, "energy = 70\n").unwrap();
            }
            let done = dj.prefer(&set("energy", "70")).unwrap();
            assert_eq!(done.changed, !no_op);
            {
                let state = dj.state();
                assert_eq!(state.pool.preferences().energy, Some(70));
                assert!(!state.planned);
                assert!(state.plan_inputs.is_none());
                assert!(state.plan_check_at.is_none());
                assert!(state.plan_retry_at.is_none());
            }

            // Restoring A used to match the retained fingerprint while the
            // pool still carried B. Neither readiness nor start may reuse B.
            std::fs::write(&path, original).unwrap();
            assert_eq!(Some(super::super::plan_inputs(moods)), original_inputs);
            assert!(!dj.state().plan_is_current(&store, moods).unwrap());
            dj.state().refresh(&store, moods, 101).unwrap();
            let state = dj.state();
            assert_eq!(state.pool.preferences().energy, Some(25));
            assert!(state.plan_is_current(&store, moods).unwrap());
        }
    }

    #[test]
    fn refused_preference_edits_keep_a_current_plan_reusable() {
        use super::super::SpotifyDj;
        use fsonos_api::dj::DjEngine;

        let (_, moods_file) = fresh("fingerprint-refused");
        let moods = Some(&moods_file);
        let path = moods_file.with_file_name(PREFERENCES_FILE);
        std::fs::write(&path, "energy = 25\n").unwrap();
        let store = MemStore::default();
        let dj = SpotifyDj::new(Some(moods_file.clone()));
        dj.state().reload(&store, moods, 100).unwrap();
        let original_inputs = dj.state().plan_inputs;

        assert!(dj.prefer(&set("energy", "150")).is_err());
        let state = dj.state();
        assert_eq!(state.plan_inputs, original_inputs);
        assert_eq!(state.pool.preferences().energy, Some(25));
        assert!(state.plan_is_current(&store, moods).unwrap());
    }

    #[test]
    fn a_no_op_through_the_surface_keeps_the_authored_document() {
        let (mut state, moods_file) = fresh("authored-no-op");
        let path = moods_file.with_file_name(PREFERENCES_FILE);
        let text = "# Owner's note\nenergy = 30 # quiet\n[favor]\ngenres = ['jazz']\n";
        std::fs::write(&path, text).unwrap();
        let saved = change(&mut state, Some(&moods_file), &set("favor.genres", "JAZZ")).unwrap();
        assert!(!saved.changed);
        assert_eq!(std::fs::read_to_string(&path).unwrap(), text);
        assert_eq!(state.pool.preferences().energy, Some(30));
        assert_eq!(state.pool.preferences().favor.genres, ["jazz"]);
    }
}
