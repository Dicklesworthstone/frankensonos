//! `fsonos policy show|check`: the house policy the CLI runs under, and
//! whether a policy file can be used.
//!
//! `show` prints the running daemon's policy (`GET /policy`) when one
//! answers, so from another machine `--daemon` shows the house's; otherwise
//! it reads policy.toml in the data directory (the built-in defaults when
//! there is none) and prints it as `get_policy` does, with the CLI as the
//! caller. `check` validates policy.toml, or `--file`:
//! it parses and its values are in range. A problem names its line and
//! column and exits 2.

use clap::Subcommand;
use fsonos_api::Failure;
use fsonos_api::surface::house_policy::PolicyDto;
use fsonos_core::clock::{Clock, SystemClock};
use fsonos_core::policy::{Client, FILE_NAME, Policy, PolicyError};
use serde::Serialize;
use std::path::{Path, PathBuf};

use crate::config::GlobalArgs;

#[derive(Subcommand)]
pub enum PolicyAction {
    /// The policy in effect (policy.toml in the data directory, else the
    /// built-in defaults) and the CLI under it.
    Show,
    /// Check that policy.toml (or --file) can be used.
    Check {
        /// The file to check [default: policy.toml in the data directory].
        #[arg(long, value_name = "PATH")]
        file: Option<PathBuf>,
    },
}

/// `fsonos policy check --json`.
#[derive(Debug, Serialize)]
struct CheckDto {
    path: PathBuf,
    /// Whether the file exists; without one the defaults apply.
    exists: bool,
    /// One line for people.
    summary: String,
    /// Denials another tool of the same class gets around
    /// ([`Policy::open_doors`]).
    warnings: Vec<String>,
}

impl CheckDto {
    fn text(&self) -> String {
        std::iter::once(format!("{}\n", self.summary))
            .chain(self.warnings.iter().map(|w| format!("warning: {w}\n")))
            .collect()
    }
}

/// `fsonos policy show|check`.
pub fn run(global: &GlobalArgs, action: &PolicyAction) -> anyhow::Result<()> {
    match action {
        PolicyAction::Show => {
            let daemon = crate::remote::Daemon::find(global, global.daemon)?;
            let shown: PolicyDto = if let Some(daemon) = daemon {
                daemon.get("/policy")?
            } else {
                let policy = crate::daemon::policy(&crate::daemon::data_dir(global)?)?;
                PolicyDto::of(&policy, &Client::Cli, SystemClock.now())
            };
            crate::emit(global.json, &shown, PolicyDto::text)
        }
        PolicyAction::Check { file } => {
            let path = match file {
                Some(path) => path.clone(),
                None => crate::daemon::data_dir(global)?.join(FILE_NAME),
            };
            let checked = check(&path)?;
            crate::emit(global.json, &checked, CheckDto::text)
        }
    }
}

/// Validate the policy file at `path`.
fn check(path: &Path) -> Result<CheckDto, Failure> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return Ok(CheckDto {
                path: path.to_owned(),
                exists: false,
                summary: format!("no {}: the built-in defaults apply", path.display()),
                warnings: Vec::new(),
            });
        }
        Err(e) => {
            return Err(Failure::invalid(format!(
                "cannot read {}: {e}",
                path.display()
            )));
        }
    };
    let policy = Policy::from_toml(&text).map_err(|e| {
        Failure::invalid(
            PolicyError {
                path: Some(path.to_owned()),
                ..e
            }
            .to_string(),
        )
        .with_hint("Fix the line named; until then the daemon and the CLI refuse this file.")
    })?;
    Ok(CheckDto {
        path: path.to_owned(),
        exists: true,
        summary: format!("{} is valid: {}", path.display(), counts(&policy)),
        warnings: policy.open_doors(),
    })
}

/// "2 room(s), 1 client(s) with rules of their own, quiet hours 22:00–07:00".
fn counts(policy: &Policy) -> String {
    let view = policy.view();
    let quiet = view.quiet_hours.map_or_else(
        || "no quiet hours".to_owned(),
        |q| format!("quiet hours {}–{}", q.start, q.end),
    );
    format!(
        "{} room(s) and {} client(s) with rules of their own, {quiet}",
        view.rooms.len(),
        view.clients.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str, text: Option<&str>) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("fsonos-policy-{}-{name}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(FILE_NAME);
        let _ = std::fs::remove_file(&path);
        if let Some(text) = text {
            std::fs::write(&path, text).unwrap();
        }
        path
    }

    #[test]
    fn a_missing_file_means_the_defaults() {
        let checked = check(&scratch("missing", None)).unwrap();
        assert!(!checked.exists);
        assert!(checked.summary.ends_with("the built-in defaults apply"));
    }

    #[test]
    fn a_valid_file_is_counted() {
        let path = scratch(
            "valid",
            Some(
                "[quiet_hours]\nstart = \"22:00\"\nend = \"07:00\"\nmax_volume = 25\n\n\
                 [rooms.\"Den\"]\nmax_volume = 40\n",
            ),
        );
        let checked = check(&path).unwrap();
        assert!(checked.exists);
        assert!(
            checked.summary.ends_with(
                "is valid: 1 room(s) and 0 client(s) with rules of their own, quiet hours \
                 22:00–07:00"
            ),
            "{}",
            checked.summary
        );
    }

    #[test]
    fn a_denial_another_tool_of_its_class_gets_around_is_a_warning() {
        let path = scratch("doors", Some("[clients.agent]\ndeny = [\"set_volume\"]\n"));
        let checked = check(&path).unwrap();
        assert_eq!(checked.warnings.len(), 1, "{:?}", checked.warnings);
        let text = checked.text();
        assert!(
            text.contains(
                "\nwarning: agent: set_volume is denied, but these volume tools are not: mute;"
            ),
            "{text}"
        );
        let covered = scratch("covered", Some("[clients.agent]\ndeny = [\"volume\"]\n"));
        assert!(check(&covered).unwrap().warnings.is_empty());
    }

    #[test]
    fn a_bad_file_names_its_line_and_exits_2() {
        let path = scratch(
            "bad",
            Some("[defaults]\nmax_volume = 70\nmax_step = \"lots\"\n"),
        );
        let err = check(&path).unwrap_err();
        assert_eq!(err.exit_code(), 2);
        assert!(
            err.detail.contains(&path.display().to_string()) && err.detail.contains("line 3"),
            "{}",
            err.detail
        );
    }
}
