//! Bounded admission of one newly launched player, before acoustic capture.

use std::io;
use std::os::fd::OwnedFd;
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, ensure};
use serde_json::json;

use super::Client;

const READY_TIMEOUT: Duration = Duration::from_secs(15);
const POLL: Duration = Duration::from_millis(50);

pub(super) struct Budget {
    deadline: Instant,
    stop: Arc<AtomicBool>,
}

impl Budget {
    fn check(&self) -> anyhow::Result<()> {
        ensure!(
            !self.stop.load(Ordering::Acquire),
            "managed mpv startup cancelled"
        );
        ensure!(
            Instant::now() < self.deadline,
            "managed mpv did not become ready before its startup deadline"
        );
        Ok(())
    }
}

impl Client {
    pub(in crate::streaming::synchronization) fn connect_launched(
        path: &Path,
        pid: u32,
        output: &str,
        stop: Arc<AtomicBool>,
        check: impl FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        Self::connect_with_budget(path, pid, output, stop, READY_TIMEOUT, check)
    }

    fn connect_with_budget(
        path: &Path,
        pid: u32,
        output: &str,
        stop: Arc<AtomicBool>,
        timeout: Duration,
        mut check: impl FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        let budget = Budget {
            deadline: Instant::now() + timeout,
            stop,
        };
        let address = socket2::SockAddr::unix(path)?;
        let socket: UnixStream = loop {
            budget.check()?;
            check()?;
            let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
            let remaining = budget.deadline.saturating_duration_since(Instant::now());
            match socket.connect_timeout(&address, remaining.min(POLL)) {
                Ok(()) => break UnixStream::from(OwnedFd::from(socket)),
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::NotFound
                            | io::ErrorKind::ConnectionRefused
                            | io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => return Err(error).context("connect to the launched mpv socket"),
            }
            budget.check()?;
            thread::sleep(POLL.min(budget.deadline.saturating_duration_since(Instant::now())));
        };
        let mut client = Self::from_stream(socket)?;
        client.startup = Some(budget);
        ensure!(
            client.get("pid")?.as_u64() == Some(u64::from(pid)),
            "private mpv IPC does not belong to the launched process"
        );
        // There is exactly one connection after admission. A malformed reply,
        // lost socket, wrong output or child exit never launches/reconnects.
        loop {
            client.check_startup()?;
            check()?;
            ensure!(
                client.get("audio-device")?.as_str() == Some(output),
                "launched mpv is not routed to its temporary output"
            );
            let idle = client
                .get("core-idle")?
                .as_bool()
                .context("mpv has no playback readiness state")?;
            let audio =
                client.request_with_unavailable(&json!(["get_property", "current-ao"]), true)?;
            ensure!(
                audio.is_null() || audio.as_str() == Some("pulse"),
                "launched mpv did not open the Pulse output"
            );
            if !idle && audio.as_str() == Some("pulse") {
                check()?;
                client.check_startup()?;
                return Ok(client);
            }
            thread::sleep(POLL);
        }
    }

    pub(super) fn check_startup(&self) -> anyhow::Result<()> {
        self.startup.as_ref().map_or(Ok(()), Budget::check)
    }
}

#[cfg(test)]
mod tests;
