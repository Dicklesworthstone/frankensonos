//! Revision-checked owner-wide moods/programs, shared by every surface.

use std::collections::BTreeMap;
use std::io::Read as _;
use std::path::{Path, PathBuf};

use clap::Subcommand;
use fsonos_api::surface::dj_config::{
    DjConfigDocument, DjConfigDto, DjConfigureRequest, DjConfiguredDto, MAX_CONFIG_BYTES,
    MoodDefinitionDto, ProgramDefinitionDto,
};
use fsonos_api::{ErrorCode, Failure};
use fsonos_core::clock::Clock;
use fsonos_core::store::Store;
use fsonos_spotify::prefs::Preferences;
use fsonos_spotify::steer::Moods;
use fsonos_spotify::steer::config::{
    ConfigWriteError, MoodsConfig, MoodsSnapshot, ProgramConfig, check_references,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use super::{State, prefs};
use crate::config::GlobalArgs;
use crate::remote::Daemon;

/// Read a revision, then apply the edited `config` document at that revision.
#[derive(Debug, Clone, Subcommand)]
pub enum ConfigurationAction {
    /// Show the revision, editable document, and effective moods/programs.
    Show,
    /// Replace owner-wide configuration from a JSON document. Obtain the
    /// revision with `dj configuration show`; stale edits are refused.
    Apply {
        file: PathBuf,
        #[arg(long, value_name = "REVISION")]
        expected_revision: String,
    },
}

fn path(moods_file: Option<&PathBuf>) -> Result<&Path, Failure> {
    moods_file.map(PathBuf::as_path).ok_or_else(|| {
        Failure::new(
            ErrorCode::NotImplemented,
            "the DJ has no data directory to keep moods and programs in",
        )
        .with_hint("Set FSONOS_DATA_DIR (or HOME).")
    })
}

fn convert<T: Serialize, U: DeserializeOwned>(value: &T) -> Result<U, Failure> {
    serde_json::to_value(value)
        .and_then(serde_json::from_value)
        .map_err(|error| {
            Failure::new(
                ErrorCode::Internal,
                format!("DJ configuration representation: {error}"),
            )
        })
}

fn catalogue(moods: &Moods) -> Result<BTreeMap<String, MoodDefinitionDto>, Failure> {
    moods
        .names()
        .map(|name| {
            let mood = moods.get(name).ok_or_else(|| {
                Failure::new(ErrorCode::Internal, "a listed DJ mood has no definition")
            })?;
            Ok((name.to_owned(), convert(mood)?))
        })
        .collect()
}

fn programs(moods: &Moods) -> Result<Vec<ProgramDefinitionDto>, Failure> {
    moods
        .programs()
        .iter()
        .map(|program| convert(&ProgramConfig::from(program)))
        .collect()
}

fn configuration(snapshot: &MoodsSnapshot, moods: &Moods) -> Result<DjConfigDto, Failure> {
    let builtins = Moods::builtin();
    Ok(DjConfigDto {
        revision: snapshot.revision.clone(),
        config: convert(&snapshot.config)?,
        builtin_moods: catalogue(&builtins)?,
        builtin_programs: programs(&builtins)?,
        moods: catalogue(moods)?,
        programs: programs(moods)?,
    })
}

fn invalid(error: impl std::fmt::Display) -> Failure {
    Failure::invalid(error.to_string())
        .with_hint("Read the current DJ configuration and correct the proposed moods or programs.")
}

fn conflict(revision: &str) -> Failure {
    Failure::new(
        ErrorCode::ConfigChanged,
        "moods.toml changed since this configuration was read",
    )
    .with_hint(format!(
        "Read /dj/configuration and review your edit against revision {revision}; do not replay the old write."
    ))
}

pub(super) fn show(moods_file: Option<&PathBuf>) -> Result<DjConfigDto, Failure> {
    let snapshot = MoodsConfig::load(path(moods_file)?).map_err(invalid)?;
    let moods = snapshot.config.resolve().map_err(invalid)?;
    configuration(&snapshot, &moods)
}

/// Leave feeds, queue positions, temporary steering and feedback untouched.
/// The next pick rebuilds the library plan against the changed definitions.
fn invalidate_plan(state: &mut State) {
    state.planned = false;
    state.plan_inputs = None;
    state.plan_check_at = None;
    state.plan_retry_at = None;
    state.refill_retries.clear();
}

/// Called with the engine's State held, including the dependent-preferences
/// check and the file's independent compare-and-replace operation.
pub(super) fn change(
    state: &mut State,
    moods_file: Option<&PathBuf>,
    request: &DjConfigureRequest,
    store: &dyn Store,
    clock: &dyn Clock,
) -> Result<DjConfiguredDto, Failure> {
    let request = request.checked()?;
    let file = path(moods_file)?;
    let current = MoodsConfig::load(file).map_err(invalid)?;
    if current.revision != request.expected_revision {
        return Err(conflict(&current.revision));
    }
    let proposed: MoodsConfig = serde_json::to_value(&request.config)
        .and_then(serde_json::from_value)
        .map_err(invalid)?;
    let proposed =
        MoodsConfig::parse(&proposed.to_toml().map_err(invalid)?, "moods.toml").map_err(invalid)?;
    let moods = proposed.resolve().map_err(invalid)?;
    let preferences_file = prefs::file(moods_file).ok_or_else(|| {
        Failure::new(ErrorCode::Internal, "DJ preferences have no data directory")
    })?;
    let preferences = Preferences::load(&preferences_file, &moods).map_err(|error| {
        Failure::invalid(format!(
            "the proposed configuration invalidates preferences: {error}"
        ))
        .with_hint(
            "Update preferences that refer to a removed mood before removing its definition.",
        )
    })?;
    let previous = current.config.resolve().map_err(invalid)?;
    check_references(&previous, &moods, store, clock.now().timestamp()).map_err(invalid)?;
    let cached_plan_matches =
        state.planned && state.plan_inputs == Some(super::plan_inputs(moods_file));
    // Complete all response conversions before publishing the file. An
    // internal representation error must not hide a successful write.
    let mut shown = configuration(
        &MoodsSnapshot {
            config: proposed.clone(),
            revision: current.revision,
        },
        &moods,
    )?;
    let saved = proposed.save(file, &request.expected_revision).map_err(|error| {
        match error {
            ConfigWriteError::Conflict { current_revision } => conflict(&current_revision),
            ConfigWriteError::Spotify(error) => invalid(error),
            error @ ConfigWriteError::Publish { published: true, .. } => {
                invalidate_plan(state);
                Failure::new(ErrorCode::OutcomeUncertain, error.to_string())
                    .with_hint("The configuration was published but its durability could not be confirmed. Read /dj/configuration before deciding what to do; do not replay this write.")
            }
            error => Failure::new(ErrorCode::Internal, error.to_string())
                .with_hint("Check access to the data directory, then read /dj/configuration before attempting another edit."),
        }
    })?;
    shown.revision = saved.snapshot.revision;
    if saved.changed || !cached_plan_matches {
        state.pool = std::mem::take(&mut state.pool).with_preferences(preferences, &moods);
        state.moods = moods;
        invalidate_plan(state);
    }
    Ok(DjConfiguredDto {
        done: if saved.changed {
            "DJ moods and programs saved; future picks follow the new configuration".into()
        } else {
            "DJ configuration is unchanged".into()
        },
        changed: saved.changed,
        configuration: shown,
    })
}

fn read_request(file: &Path, expected_revision: &str) -> Result<DjConfigureRequest, Failure> {
    let mut bytes = Vec::new();
    std::fs::File::open(file)
        .and_then(|file| {
            file.take((MAX_CONFIG_BYTES + 1) as u64)
                .read_to_end(&mut bytes)
        })
        .map_err(|error| Failure::invalid(format!("{}: {error}", file.display())))?;
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(Failure::invalid(format!(
            "DJ configuration JSON is limited to {MAX_CONFIG_BYTES} bytes"
        )));
    }
    let config: DjConfigDocument = serde_json::from_slice(&bytes).map_err(|error| {
        Failure::invalid(format!("{}: {error}", file.display())).with_hint(
            "The file must contain the editable config object from dj configuration show, not the whole response.",
        )
    })?;
    DjConfigureRequest {
        expected_revision: expected_revision.to_owned(),
        config,
    }
    .checked()
}

/// Use the running daemon, which owns the live plan and dependent stores.
/// A write is dispatched once, using the revision explicitly supplied.
pub fn run(global: &GlobalArgs, action: Option<&ConfigurationAction>) -> anyhow::Result<()> {
    let request = match action {
        Some(ConfigurationAction::Apply {
            file,
            expected_revision,
        }) => Some(read_request(file, expected_revision)?),
        None | Some(ConfigurationAction::Show) => None,
    };
    let daemon = Daemon::find(global, true)?.ok_or_else(|| {
        Failure::new(
            ErrorCode::NotReady,
            "DJ configuration requires the running daemon",
        )
        .with_hint("Start fsonos serve, then run this command without --direct.")
    })?;
    if let Some(request) = request {
        let saved: DjConfiguredDto = daemon.post("/dj/configuration", &request)?;
        crate::emit(global.json, &saved, |saved| {
            format!(
                "{}\nRevision: {}\n",
                saved.done, saved.configuration.revision
            )
        })
    } else {
        let shown: DjConfigDto = daemon.get("/dj/configuration")?;
        let text = serde_json::to_string_pretty(&shown)?;
        crate::emit(global.json, &shown, |_| format!("{text}\n"))
    }
}

#[cfg(test)]
mod tests;
