//! An explicit foreground application, launched only after the stream is ready.
//! PULSE_SINK is a child-local preference, not a default-output or app-move command.

use std::ffi::OsString;
use std::process::{Child, Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context as _, ensure};

use super::StreamArgs;

pub(super) struct Launch {
    program: OsString,
    arguments: Vec<OsString>,
    sink: String,
}

impl Launch {
    /// Validation only. In particular, do not execute the application to probe it.
    pub(super) fn prepare(args: &StreamArgs) -> anyhow::Result<Option<Self>> {
        let Some((program, arguments)) = args.application.split_first() else {
            return Ok(None);
        };
        ensure!(
            cfg!(target_os = "linux") && args.format == "pulse",
            "application launch requires Linux --format pulse"
        );
        let sink = args
            .virtual_output
            .as_ref()
            .context("application launch requires --virtual-output")?;
        ensure!(
            args.input == format!("{sink}.monitor") && args.latency_wav.is_none(),
            "application launch requires the prepared temporary output monitor"
        );
        #[cfg(unix)]
        ensure!(
            !args.sync.measure_latency && args.sync.sync_mpv.is_none(),
            "application launch is separate from measurement and existing-player mpv correction"
        );
        ensure!(
            !program.is_empty(),
            "application executable must not be empty"
        );
        ensure!(
            args.application
                .iter()
                .all(|arg| !arg.as_encoded_bytes().contains(&0)),
            "application executable and arguments must not contain NUL"
        );
        Ok(Some(Self {
            program: program.clone(),
            arguments: arguments.to_vec(),
            sink: sink.clone(),
        }))
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.arguments).env("PULSE_SINK", &self.sink);
        // The controller owns the terminal and stdout (including --json).
        // GUI controls still work; the child cannot consume controller input.
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit());
        command
    }
}

struct Process {
    child: Child,
    reaped: Arc<AtomicBool>,
}

impl Process {
    fn run(&mut self, stop: &AtomicBool, stream_stop: &AtomicBool) -> anyhow::Result<()> {
        loop {
            if stop.load(Ordering::Acquire) || stream_stop.load(Ordering::Acquire) {
                return self.stop();
            }
            if let Some(status) = self
                .child
                .try_wait()
                .context("observe foreground application")?
            {
                self.reaped.store(true, Ordering::Release);
                // Ctrl-C can reach the child and the controller together.
                if stop.load(Ordering::Acquire) || stream_stop.load(Ordering::Acquire) {
                    return Ok(());
                }
                ensure!(
                    status.success(),
                    "foreground application exited with {status}"
                );
                // Sonos may still have buffered audio. Do not truncate it by
                // guessing a drain delay or replacing the source on child exit.
                eprintln!(
                    "fsonos stream: application finished; the audio stream remains available. \
                     Press Ctrl-C after buffered audio finishes."
                );
                return Ok(());
            }
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn stop(&mut self) -> anyhow::Result<()> {
        if self.reaped.load(Ordering::Acquire) {
            return Ok(());
        }
        if self.child.try_wait()?.is_none() {
            if let Err(error) = self.child.kill() {
                // It may have exited between the observation and kill. Never
                // signal a numeric PID after reaping; retain the Child handle.
                if self.child.try_wait()?.is_none() {
                    return Err(error).context("stop foreground application");
                }
            }
            self.child.wait().context("reap foreground application")?;
        }
        self.reaped.store(true, Ordering::Release);
        Ok(())
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("fsonos stream: application cleanup was not confirmed: {error:#}");
        }
    }
}

pub(super) struct Application {
    stop: Arc<AtomicBool>,
    reaped: Arc<AtomicBool>,
    worker: Option<JoinHandle<anyhow::Result<()>>>,
}

impl Application {
    /// Called only after output read-back, encoder readiness and accepted Play.
    /// No application is relaunched on failure, child exit, or stream recovery.
    pub(super) fn start(launch: &Launch, stream_stop: Arc<AtomicBool>) -> anyhow::Result<Self> {
        Self::from_command(launch.command(), stream_stop)
    }

    fn from_command(mut command: Command, stream_stop: Arc<AtomicBool>) -> anyhow::Result<Self> {
        ensure!(
            !stream_stop.load(Ordering::Acquire),
            "stream cancelled before application launch"
        );
        let reaped = Arc::new(AtomicBool::new(false));
        let mut process = Process {
            child: command
                .spawn()
                .context("launch requested foreground application")?,
            reaped: Arc::clone(&reaped),
        };
        ensure!(
            !stream_stop.load(Ordering::Acquire),
            "stream cancelled during application launch"
        );
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        // Process owns cleanup even if thread creation fails or the worker panics.
        let worker = thread::Builder::new()
            .name("fsonos-stream-app".into())
            .spawn(move || {
                let result = process.run(&worker_stop, &stream_stop);
                if let Err(error) = &result {
                    eprintln!(
                        "fsonos stream: {error:#}; no application restart was attempted. \
                         Press Ctrl-C to finish streaming."
                    );
                }
                result
            })?;
        Ok(Self {
            stop,
            reaped,
            worker: Some(worker),
        })
    }

    pub(super) fn shutdown(&mut self) -> anyhow::Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("foreground application worker panicked"))??;
        }
        Ok(())
    }

    /// An error exit is still reaped. Only uncertain process cleanup requires
    /// retaining the temporary sink instead of risking audio-server fallback.
    pub(super) fn reaped(&self) -> bool {
        self.reaped.load(Ordering::Acquire)
    }
}

impl Drop for Application {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("fsonos stream: application cleanup was not confirmed: {error:#}");
        }
    }
}

#[cfg(test)]
mod tests;
