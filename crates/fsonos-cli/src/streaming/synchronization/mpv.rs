//! Local mpv JSON IPC. One connection, bounded replies, no mutation retries.

use std::io::{Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::Path;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail, ensure};
use serde_json::{Value, json};

mod events;

use events::PlaybackEvents;

const LIMIT: usize = 65_536;
const EPSILON: f64 = 0.001;

pub(super) struct Client {
    socket: UnixStream,
    pending: Vec<u8>,
    request_id: i64,
    timeout: Duration,
    events: PlaybackEvents,
}

impl Client {
    pub(super) fn connect(path: &Path) -> anyhow::Result<Self> {
        // Connect happens during preflight, before any Sonos or capture writes.
        Self::from_stream(UnixStream::connect(path).context("connect to the local mpv socket")?)
    }

    fn from_stream(socket: UnixStream) -> anyhow::Result<Self> {
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            pending: Vec::new(),
            request_id: 0,
            timeout: Duration::from_secs(2),
            events: PlaybackEvents::default(),
        })
    }

    fn request(&mut self, command: &Value) -> anyhow::Result<Value> {
        self.request_id = self
            .request_id
            .checked_add(1)
            .context("mpv request counter overflow")?;
        let id = self.request_id;
        let mut request = serde_json::to_vec(&json!({"command": command, "request_id": id}))?;
        request.push(b'\n');
        let started = Instant::now();
        let mut sent = 0;
        while sent < request.len() {
            ensure!(
                started.elapsed() < self.timeout,
                "mpv write deadline exceeded; not replayed"
            );
            match self.socket.write(&request[sent..]) {
                Ok(0) => bail!("mpv closed during command write; not replayed"),
                Ok(count) => sent += count,
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
        let mut received = self.pending.len();
        loop {
            ensure!(
                started.elapsed() < self.timeout,
                "mpv reply deadline exceeded; not replayed"
            );
            if let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<_> = self.pending.drain(..=end).collect();
                let response: Value =
                    serde_json::from_slice(&line).context("invalid mpv JSON reply")?;
                self.events.observe(&response)?;
                if response.get("event").is_some() {
                    continue;
                }
                if response.get("request_id").and_then(Value::as_i64) == Some(id) {
                    ensure!(
                        response.get("error").and_then(Value::as_str) == Some("success"),
                        "mpv rejected the command; not replayed"
                    );
                    return Ok(response.get("data").cloned().unwrap_or(Value::Null));
                }
                continue;
            }
            let mut buffer = [0_u8; 4096];
            match self.socket.read(&mut buffer) {
                Ok(0) => bail!("mpv closed before acknowledging the command; not replayed"),
                Ok(count) => {
                    received += count;
                    ensure!(received <= LIMIT, "mpv reply/event byte limit exceeded");
                    self.pending.extend_from_slice(&buffer[..count]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn watch(&mut self) -> anyhow::Result<()> {
        for (index, property) in events::PROPERTIES.iter().enumerate() {
            let id = u64::try_from(index)? + 1;
            self.request(&json!(["observe_property", id, property]))?;
        }
        Ok(())
    }

    /// Process trailing events in the same packet as a reply and events already
    /// queued in the socket. No wait for new events, and no unbounded drain.
    fn drain_events(&mut self) -> anyhow::Result<()> {
        let started = Instant::now();
        let mut received = self.pending.len();
        loop {
            ensure!(
                started.elapsed() < self.timeout,
                "mpv event drain deadline exceeded"
            );
            if let Some(end) = self.pending.iter().position(|byte| *byte == b'\n') {
                let line: Vec<_> = self.pending.drain(..=end).collect();
                let message: Value = serde_json::from_slice(&line).context("invalid mpv event")?;
                self.events.observe(&message)?;
                continue;
            }
            let mut bytes = [0_u8; 4096];
            match self.socket.read(&mut bytes) {
                Ok(0) => bail!("mpv closed while observing playback"),
                Ok(count) => {
                    received += count;
                    ensure!(received <= LIMIT, "mpv reply/event byte limit exceeded");
                    self.pending.extend_from_slice(&bytes[..count]);
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    if self.pending.is_empty() {
                        return self.events.check();
                    }
                    // A fragmented event already started. Complete it within the
                    // same deadline before authorizing a new measurement or write.
                    thread::sleep(Duration::from_millis(5));
                }
                Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error.into()),
            }
        }
    }

    fn get(&mut self, name: &str) -> anyhow::Result<Value> {
        self.request(&json!(["get_property", name]))
    }

    fn text(&mut self, name: &str) -> anyhow::Result<String> {
        self.get(name)?
            .as_str()
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .context("mpv has no active media/output identity")
    }

    fn snapshot(&mut self) -> anyhow::Result<Snapshot> {
        let media = self.text("path")?;
        let output = self.text("audio-device")?;
        let delay = self
            .get("audio-delay")?
            .as_f64()
            .context("mpv has no numeric audio-delay")?;
        ensure!(
            delay.is_finite() && delay.abs() <= 30.0,
            "mpv audio-delay is outside -30..30 seconds"
        );
        let paused = self
            .get("pause")?
            .as_bool()
            .context("mpv has no pause state")?;
        let speed = self
            .get("speed")?
            .as_f64()
            .context("mpv has no playback speed")?;
        ensure!(
            (speed - 1.0).abs() < 1e-6,
            "audio sync requires normal mpv playback speed"
        );
        ensure!(
            self.text("path")? == media && self.text("audio-device")? == output,
            "mpv media/output changed during observation"
        );
        self.drain_events()?;
        Ok(Snapshot {
            media,
            output,
            delay,
            paused,
        })
    }

    fn set_delay(&mut self, delay: f64) -> anyhow::Result<()> {
        self.request(&json!(["set_property", "audio-delay", delay]))?;
        Ok(())
    }
}

struct Snapshot {
    media: String,
    output: String,
    delay: f64,
    paused: bool,
}

/// Evidence for one continuous playback generation; not a transaction token.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) struct Window {
    revision: u64,
}

pub(super) struct Controller {
    client: Client,
    original: Snapshot,
    original_identity: u64,
    expected: f64,
    attempted: Option<f64>,
    base_delay: f64,
    relinquished: bool,
}

impl Controller {
    pub(super) fn new(mut client: Client, base_delay: f64) -> anyhow::Result<Self> {
        ensure!(
            base_delay.is_finite() && base_delay.abs() <= 10.0,
            "--sync-base-delay must be finite and within -10..10 seconds"
        );
        client.watch()?;
        let original = client.snapshot()?;
        client.events.seed(&original);
        let original_identity = client.events.identity();
        Ok(Self {
            expected: original.delay,
            client,
            original,
            original_identity,
            attempted: None,
            base_delay,
            relinquished: false,
        })
    }

    fn observed(&mut self) -> anyhow::Result<Snapshot> {
        self.client.events.check()?;
        let observed = self.client.snapshot();
        if self.client.events.identity() != self.original_identity {
            self.relinquished = true;
            bail!("mpv media/output lifecycle changed; synchronization relinquished");
        }
        let current = observed?;
        if current.media != self.original.media || current.output != self.original.output {
            self.relinquished = true;
            bail!("mpv media/output changed; synchronization relinquished");
        }
        Ok(current)
    }

    pub(super) fn ready(&mut self) -> anyhow::Result<bool> {
        ensure!(
            !self.relinquished,
            "synchronization ownership was relinquished"
        );
        let revision = self.client.events.revision();
        let current = self.observed()?;
        if (current.delay - self.expected).abs() > EPSILON {
            self.relinquished = true;
            bail!("mpv audio-delay changed outside this session; synchronization relinquished");
        }
        Ok(!current.paused && self.client.events.revision() == revision)
    }

    pub(super) fn begin_window(&mut self) -> anyhow::Result<Option<Window>> {
        Ok(self.ready()?.then(|| Window {
            revision: self.client.events.revision(),
        }))
    }

    pub(super) fn finish_window(&mut self, window: Window) -> anyhow::Result<bool> {
        Ok(self.ready()? && self.client.events.revision() == window.revision)
    }

    /// Apply an absolute target based on the explicit baseline, never cumulative.
    pub(super) fn apply(&mut self, lag: f64, window: Window) -> anyhow::Result<bool> {
        ensure!(
            lag.is_finite() && (0.02..14.0).contains(&lag),
            "invalid measured lag"
        );
        if !self.finish_window(window)? {
            return Ok(false);
        }
        let target = self.base_delay - lag;
        if (target - self.expected).abs() <= 0.04 {
            return Ok(false);
        }
        // Record before sending. A failed reply does not prove the write failed.
        self.attempted = Some(target);
        self.client.set_delay(target)?;
        let current = self.observed()?;
        ensure!(
            (current.delay - target).abs() <= EPSILON,
            "mpv did not retain the requested delay"
        );
        self.expected = target;
        Ok(true)
    }

    pub(super) fn restore(&mut self) -> anyhow::Result<()> {
        let Some(attempted) = self.attempted else {
            return Ok(());
        };
        ensure!(
            !self.relinquished,
            "prior mpv delay not restored: ownership was relinquished"
        );
        let current = self.observed()?;
        ensure!(
            (current.delay - attempted).abs() <= EPSILON
                || (current.delay - self.expected).abs() <= EPSILON,
            "prior mpv delay not restored: a different delay is now set"
        );
        // Consume ownership before dispatch. Neither failure nor Drop replays it.
        self.attempted = None;
        self.client.set_delay(self.original.delay)?;
        let current = self.observed()?;
        ensure!(
            (current.delay - self.original.delay).abs() <= EPSILON,
            "mpv restoration was not confirmed"
        );
        Ok(())
    }
}

#[cfg(test)]
mod tests;
