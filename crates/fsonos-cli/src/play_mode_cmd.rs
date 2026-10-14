//! Read or set complete queue ordering through the shared API. Merely reading
//! a mode never starts playback, selects a source, or creates an undo record.

use clap::Args;
use clap::builder::PossibleValuesParser;
use fsonos_api::surface::queue::{PlayModeDto, PlayModeRequest, PlayModeSetDto};
use fsonos_core::policy::Client;
use fsonos_types::PlayMode;

use crate::config::GlobalArgs;
use crate::remote::{Daemon, pct};
use crate::{daemon, emit};

#[derive(Debug, Args)]
pub struct PlayModeArgs {
    /// A room, Room@S1 / Room@S2, here, or a single-room alias.
    #[arg(value_parser = nonblank)]
    zone: String,
    /// Complete wire mode. SHUFFLE also repeats the queue; SHUFFLE_NOREPEAT
    /// does not. Omit to read the setting, queue selection and DJ ownership.
    #[arg(value_parser = mode_values())]
    mode: Option<String>,
    /// Require this previously observed mode before attempting the change.
    #[arg(long, requires = "mode", value_parser = mode_values())]
    expected_mode: Option<String>,
}

fn mode_values() -> PossibleValuesParser {
    PossibleValuesParser::new(PlayMode::ALL.map(PlayMode::as_str))
}

fn nonblank(value: &str) -> Result<String, String> {
    if value.trim().is_empty() {
        Err("name a room whose group's mode should be read or changed".into())
    } else {
        Ok(value.to_owned())
    }
}

impl PlayModeArgs {
    fn request(&self) -> Option<PlayModeRequest> {
        self.mode.as_ref().map(|mode| PlayModeRequest {
            zone: self.zone.clone(),
            mode: mode.clone(),
            expected_mode: self.expected_mode.clone(),
        })
    }

    fn read_path(&self) -> String {
        format!("/zones/{}/play-mode", pct(&self.zone))
    }
}

pub fn run(global: &GlobalArgs, args: &PlayModeArgs) -> anyhow::Result<()> {
    if let Some(remote) = Daemon::find(global, global.daemon)? {
        return through(&remote, global.json, args);
    }
    let dir = daemon::data_dir(global)?;
    let surface = daemon::surface(global, daemon::policy(&dir)?)?;
    match args.request() {
        None => emit(
            global.json,
            &surface.get_play_mode(&Client::Cli, &args.zone)?,
            PlayModeDto::text,
        ),
        Some(request) => {
            let surface = daemon::with_action_log(surface, &dir, "cli");
            let result = surface.set_play_mode(&Client::Cli, &request)?;
            emit(global.json, &result, PlayModeSetDto::text)
        }
    }
}

fn through(remote: &Daemon, json: bool, args: &PlayModeArgs) -> anyhow::Result<()> {
    match args.request() {
        None => {
            let mode: PlayModeDto = remote.get(&args.read_path())?;
            emit(json, &mode, PlayModeDto::text)
        }
        Some(request) => {
            // Propagate uncertain outcomes; never fall back to direct playback.
            let result: PlayModeSetDto = remote.post("/play-mode", &request)?;
            emit(json, &result, PlayModeSetDto::text)
        }
    }
}

#[cfg(test)]
mod tests;
