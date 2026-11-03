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

#[cfg(unix)]
mod mpv;

pub(super) struct Launch {
    program: OsString,
    arguments: Vec<OsString>,
    sink: String,
    #[cfg(unix)]
    managed_mpv: bool,
}

impl Launch {
    /// Validation only. In particular, do not execute the application to probe it.
    pub(super) fn prepare(args: &StreamArgs) -> anyhow::Result<Option<Self>> {
        let Some((program, arguments)) = args.application.split_first() else {
            #[cfg(unix)]
            ensure!(
                !args.sync.sync_launched_mpv,
                "--sync-launched-mpv requires a trailing mpv command after --"
            );
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
        #[cfg(unix)]
        if args.sync.sync_launched_mpv {
            mpv::validate(arguments)?;
        }
        Ok(Some(Self {
            program: program.clone(),
            arguments: arguments.to_vec(),
            sink: sink.clone(),
            #[cfg(unix)]
            managed_mpv: args.sync.sync_launched_mpv,
        }))
    }

    fn command(&self) -> Command {
        let mut command = Command::new(&self.program);
        command.args(&self.arguments).env("PULSE_SINK", &self.sink);
        configure_stdio(&mut command);
        command
    }
}

fn configure_stdio(command: &mut Command) {
    // The controller owns the terminal and stdout (including --json).
    // GUI controls still work; the child cannot consume controller input.
    command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::inherit());
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
    #[cfg(unix)]
    pid: u32,
    stop: Arc<AtomicBool>,
    reaped: Arc<AtomicBool>,
    worker: Option<JoinHandle<anyhow::Result<()>>>,
    #[cfg(unix)]
    ipc: Option<mpv::Ipc>,
}

impl Application {
    /// Called only after output read-back, encoder readiness and accepted Play.
    /// No application is relaunched on failure, child exit, or stream recovery.
    pub(super) fn start(launch: &Launch, stream_stop: Arc<AtomicBool>) -> anyhow::Result<Self> {
        #[cfg(unix)]
        if launch.managed_mpv {
            let ipc = mpv::Ipc::new(&launch.sink)?;
            return Self::from_managed_command(ipc.command(launch), ipc, &stream_stop);
        }
        Self::from_command(launch.command(), stream_stop)
    }

    #[cfg(unix)]
    fn from_managed_command(
        mut command: Command,
        ipc: mpv::Ipc,
        stream_stop: &AtomicBool,
    ) -> anyhow::Result<Self> {
        use std::os::unix::process::CommandExt as _;

        ensure!(
            !stream_stop.load(Ordering::Acquire),
            "stream cancelled before mpv launch"
        );
        // Terminal Ctrl-C must reach the supervisor, not close mpv's IPC
        // before synchronization restores its owned delay.
        command.process_group(0);
        // The supervisor explicitly calls shutdown after synchronization,
        // on both normal completion and every error.
        let mut application = Self::from_command(command, Arc::new(AtomicBool::new(false)))?;
        application.ipc = Some(ipc);
        ensure!(
            !stream_stop.load(Ordering::Acquire),
            "stream cancelled during mpv launch"
        );
        Ok(application)
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
        #[cfg(unix)]
        let pid = process.child.id();
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
            #[cfg(unix)]
            pid,
            stop,
            reaped,
            worker: Some(worker),
            #[cfg(unix)]
            ipc: None,
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

    #[cfg(unix)]
    pub(super) fn mpv_endpoint(&self) -> Option<(&std::path::Path, &str)> {
        self.ipc.as_ref().map(|ipc| (ipc.path(), ipc.output()))
    }

    #[cfg(unix)]
    pub(super) fn id(&self) -> u32 {
        self.pid
    }

    #[cfg(unix)]
    pub(super) fn ensure_running(&self) -> anyhow::Result<()> {
        ensure!(
            !self.reaped()
                && self
                    .worker
                    .as_ref()
                    .is_some_and(|worker| !worker.is_finished()),
            "launched mpv exited before synchronization was ready"
        );
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
        #[cfg(unix)]
        if !self.reaped()
            && let Some(ipc) = &mut self.ipc
        {
            ipc.retain();
        }
    }
}

#[cfg(test)]
mod tests;
