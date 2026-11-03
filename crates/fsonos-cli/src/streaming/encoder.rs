//! Capture stays alive until the stream supervisor finishes playback recovery.

use std::io;
use std::process::{Child, Command, Stdio};

/// Spawn only the requested encoder. The supervisor owns its Child handle and
/// kills/reaps it after restoration; terminal cancellation must not race that
/// ordering by killing FFmpeg directly.
pub(super) fn spawn(command: &mut Command) -> io::Result<Child> {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt as _;
        command.process_group(0);
    }
    command.spawn()
}

#[cfg(all(test, unix))]
mod tests;
