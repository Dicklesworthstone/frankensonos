//! Private IPC and explicit routing for the owned mpv launch mode.

use std::ffi::OsString;
use std::os::unix::fs::{DirBuilderExt as _, FileTypeExt as _};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, ensure};

use super::Launch;

/// The opt-in owns configuration, IPC and audio routing. Display and media
/// options remain literal arguments; no shell or user configuration is loaded.
pub(super) fn validate(arguments: &[OsString]) -> anyhow::Result<()> {
    let mut files = false;
    let mut source = false;
    for argument in arguments {
        if files {
            source |= !argument.is_empty();
            continue;
        }
        if argument == "--" {
            files = true;
            continue;
        }
        let raw = argument.as_encoded_bytes();
        if !raw.starts_with(b"-") {
            source |= !raw.is_empty();
            continue;
        }
        let text = argument
            .to_str()
            .context("mpv option names must be valid UTF-8; put native filenames after --")?;
        let option = text.trim_start_matches('-').split('=').next().unwrap_or("");
        let option = option.strip_prefix("no-").unwrap_or(option);
        ensure!(
            !matches!(
                option,
                "input-ipc-server"
                    | "input-ipc-client"
                    | "input-commands"
                    | "input-conf"
                    | "input-file"
                    | "audio-device"
                    | "ao"
                    | "config"
                    | "config-dir"
                    | "include"
                    | "use-filedir-conf"
                    | "profile"
                    | "script"
                    | "scripts"
                    | "load-scripts"
                    | "idle"
                    | "terminal"
                    | "input-terminal"
                    | "fork"
                    | "daemon"
                    | "{"
                    | "}"
            ) && !option.starts_with("scripts-")
                && !option.starts_with("profile-")
                && !option.starts_with("input-commands-"),
            "--sync-launched-mpv manages configuration, IPC and audio routing; remove mpv option {text} or use --sync-mpv with your own player"
        );
        if option == "playlist" {
            source = true;
        }
    }
    ensure!(
        source,
        "the trailing mpv command must include media to play"
    );
    Ok(())
}

pub(super) struct Ipc {
    directory: PathBuf,
    path: PathBuf,
    output: String,
    retained: bool,
}

impl Ipc {
    pub(super) fn new(sink: &str) -> anyhow::Result<Self> {
        let directory = std::env::temp_dir().join(format!(
            "fsonos-mpv-{}",
            fsonos_core::announce::clip::clip_id()?
        ));
        let path = directory.join("ipc");
        // Check Unix path length before creating anything, never silently use
        // a public, predictable fallback socket when TMPDIR is too long.
        socket2::SockAddr::unix(&path).context("temporary mpv IPC path is too long")?;
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .context("create the private mpv IPC directory")?;
        Ok(Self {
            directory,
            path,
            output: format!("pulse/{sink}"),
            retained: false,
        })
    }

    pub(super) fn command(&self, launch: &Launch) -> Command {
        let mut command = Command::new(&launch.program);
        let mut socket = OsString::from("--input-ipc-server=");
        socket.push(&self.path);
        command.args([
            OsString::from("--no-config"),
            OsString::from("--load-scripts=no"),
            OsString::from("--input-terminal=no"),
            OsString::from("--terminal=no"),
            OsString::from("--idle=no"),
            socket,
            OsString::from(format!("--audio-device={}", self.output)),
        ]);
        command
            .args(&launch.arguments)
            .env("PULSE_SINK", &launch.sink);
        super::configure_stdio(&mut command);
        command
    }

    pub(super) fn path(&self) -> &Path {
        &self.path
    }

    pub(super) fn output(&self) -> &str {
        &self.output
    }

    pub(super) fn retain(&mut self) {
        self.retained = true;
        eprintln!(
            "fsonos stream: mpv cleanup was not confirmed; retaining private IPC directory {}",
            self.directory.display()
        );
    }
}

impl Drop for Ipc {
    fn drop(&mut self) {
        if self.retained {
            return;
        }
        // Only the known socket and then an empty directory: never recursively
        // remove unexpected files a child may have created.
        if std::fs::symlink_metadata(&self.path)
            .is_ok_and(|metadata| metadata.file_type().is_socket())
            && let Err(error) = std::fs::remove_file(&self.path)
        {
            eprintln!("fsonos stream: private mpv socket cleanup failed: {error}");
        }
        if let Err(error) = std::fs::remove_dir(&self.directory) {
            eprintln!("fsonos stream: private mpv IPC directory cleanup failed: {error}");
        }
    }
}

#[cfg(test)]
mod tests;
