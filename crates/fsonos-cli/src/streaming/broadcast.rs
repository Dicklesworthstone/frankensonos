//! Bounded live MP3 fan-out. Capture never waits for a network client.
//!
//! New readers start at the newest complete frame, not the encoder pipe's
//! backlog. Readers which fall outside the retained window disconnect rather
//! than silently accumulating latency. The encoder disables its bit reservoir
//! so those frame-boundary joins do not depend on an earlier client's audio.

use std::collections::VecDeque;
use std::io::{self, Read as _, Write as _};
use std::net::{TcpListener, TcpStream};
use std::process::Child;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

// 128 MPEG-1 frames at 44.1 kHz are about 3.3 seconds, at most 134 KiB.
const MAX_FRAMES: usize = 128;
const MAX_CLIENTS: usize = 8;
const HEADER_LIMIT: usize = 8192;
const HEADER_TIMEOUT: Duration = Duration::from_secs(3);
const IO_POLL: Duration = Duration::from_millis(100);
// Silence still produces encoded frames. Ten seconds without a complete frame
// means capture has stalled, even when FFmpeg and its pipe reader remain alive.
const AUDIO_TIMEOUT: Duration = Duration::from_secs(10);
const AUDIO_HEADERS: &[u8] =
    b"HTTP/1.1 200 OK\r\nContent-Type: audio/mpeg\r\nCache-Control: no-store\r\nConnection: close\r\n\r\n";
const NOT_FOUND: &[u8] =
    b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
const UNAVAILABLE: &[u8] =
    b"HTTP/1.1 503 Service Unavailable\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";

#[derive(Default)]
struct State {
    frames: VecDeque<Arc<[u8]>>,
    next: u64,
    ended: bool,
    last_frame: Option<Instant>,
}

impl State {
    fn check_fresh(&self, now: Instant) -> io::Result<()> {
        if self
            .last_frame
            .is_some_and(|last| now.saturating_duration_since(last) >= AUDIO_TIMEOUT)
        {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "FFmpeg produced no complete MP3 frame for ten seconds; live capture stalled",
            ));
        }
        Ok(())
    }
}

#[derive(Default)]
pub(super) struct Audio {
    state: Mutex<State>,
    changed: Condvar,
    readers: AtomicUsize,
}

// Count only live GET handlers which successfully wrote a complete frame.
// A HEAD/probe, pending header or old disconnected reader is not readiness.
struct Reader<'a> {
    count: &'a AtomicUsize,
    delivering: bool,
}

impl<'a> Reader<'a> {
    fn new(count: &'a AtomicUsize) -> Self {
        Self {
            count,
            delivering: false,
        }
    }

    fn delivered(&mut self) {
        if !self.delivering {
            // At most MAX_CLIENTS reader guards can be active.
            self.count.fetch_add(1, Ordering::AcqRel);
            self.delivering = true;
        }
    }
}

impl Drop for Reader<'_> {
    fn drop(&mut self) {
        if self.delivering {
            self.count.fetch_sub(1, Ordering::AcqRel);
        }
    }
}

fn await_reader(
    count: &AtomicUsize,
    timeout: Duration,
    mut check: impl FnMut() -> io::Result<()>,
) -> io::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        // Cancellation or a dead encoder wins over stale delivery evidence.
        check()?;
        if count.load(Ordering::Acquire) > 0 {
            return Ok(());
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "no live HTTP audio reader before the application startup deadline",
            ));
        }
        thread::sleep(remaining.min(IO_POLL));
    }
}

fn poisoned<T>(_: std::sync::PoisonError<T>) -> io::Error {
    io::Error::other("live audio buffer poisoned")
}

impl Audio {
    fn publish(&self, frame: Vec<u8>) -> io::Result<()> {
        let mut state = self.state.lock().map_err(poisoned)?;
        let next = state
            .next
            .checked_add(1)
            .ok_or_else(|| io::Error::other("live audio sequence exhausted"))?;
        state.frames.push_back(frame.into());
        state.next = next;
        state.last_frame = Some(Instant::now());
        if state.frames.len() > MAX_FRAMES {
            let _ = state.frames.pop_front();
        }
        self.changed.notify_all();
        Ok(())
    }

    fn check_fresh(&self) -> io::Result<()> {
        self.state
            .lock()
            .map_err(poisoned)?
            .check_fresh(Instant::now())
    }

    fn finish(&self) -> io::Result<()> {
        self.state.lock().map_err(poisoned)?.ended = true;
        self.changed.notify_all();
        Ok(())
    }

    /// Run on the capture thread even while no player is connected. Killing
    /// and reaping the encoder unblocks a read during shutdown.
    pub(super) fn ingest(&self, mut input: impl io::Read, stop: &AtomicBool) -> io::Result<()> {
        let result = (|| {
            let mut pending = Vec::new();
            let mut chunk = [0; 8192];
            while !stop.load(Ordering::Acquire) {
                let count = match input.read(&mut chunk) {
                    Ok(0) => {
                        if !stop.load(Ordering::Acquire) && !pending.is_empty() {
                            return Err(io::Error::new(
                                io::ErrorKind::UnexpectedEof,
                                "encoder ended inside an MP3 frame",
                            ));
                        }
                        return Ok(());
                    }
                    Ok(count) => count,
                    Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                    Err(error) => return Err(error),
                };
                frames(&mut pending, &chunk[..count], |frame| self.publish(frame))?;
            }
            Ok(())
        })();
        self.finish()?;
        result
    }

    /// A bad device/encoder must fail before the previous music is replaced.
    pub(super) fn wait_ready(&self, stop: &AtomicBool, timeout: Duration) -> io::Result<()> {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock().map_err(poisoned)?;
        loop {
            if stop.load(Ordering::Acquire) {
                return Err(io::Error::new(
                    io::ErrorKind::Interrupted,
                    "stream cancelled",
                ));
            }
            if state.ended {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "encoder stopped before live audio was ready",
                ));
            }
            state.check_fresh(Instant::now())?;
            if !state.frames.is_empty() {
                return Ok(());
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "encoder produced no complete MP3 frame before the startup deadline",
                ));
            }
            (state, _) = self
                .changed
                .wait_timeout(state, remaining.min(IO_POLL))
                .map_err(poisoned)?;
        }
    }

    fn subscribe(&self) -> io::Result<u64> {
        let state = self.state.lock().map_err(poisoned)?;
        state.check_fresh(Instant::now())?;
        Ok(state.next.saturating_sub(1))
    }

    fn next(&self, cursor: &mut u64, stop: &AtomicBool) -> io::Result<Option<Arc<[u8]>>> {
        let mut state = self.state.lock().map_err(poisoned)?;
        while !stop.load(Ordering::Acquire) {
            // Do not replay an old tail or hold an HTTP stream open forever
            // when capture is alive but no longer producing encoded audio.
            state.check_fresh(Instant::now())?;
            let retained = u64::try_from(state.frames.len()).map_err(io::Error::other)?;
            let first = state.next - retained;
            if *cursor < first {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "reader fell behind the live audio window; reconnect at the live edge",
                ));
            }
            if *cursor < state.next {
                let index = usize::try_from(*cursor - first).map_err(io::Error::other)?;
                let frame = Arc::clone(&state.frames[index]);
                *cursor += 1;
                return Ok(Some(frame));
            }
            if state.ended {
                return Ok(None);
            }
            (state, _) = self
                .changed
                .wait_timeout(state, IO_POLL)
                .map_err(poisoned)?;
        }
        Ok(None)
    }
}

/// The encoder contract is MPEG-1 Layer III at 44.1 kHz, with no ID3/Xing
/// preamble or bit reservoir. Reject reserved/free bitrates and other formats.
fn frame_len(header: &[u8]) -> Option<usize> {
    if header.len() < 4 || header[0] != 0xff || header[1] & 0xfe != 0xfa {
        return None;
    }
    if header[2] & 0x0c != 0 {
        return None;
    }
    let rate = match header[2] >> 4 {
        1 => 32,
        2 => 40,
        3 => 48,
        4 => 56,
        5 => 64,
        6 => 80,
        7 => 96,
        8 => 112,
        9 => 128,
        10 => 160,
        11 => 192,
        12 => 224,
        13 => 256,
        14 => 320,
        _ => return None,
    };
    Some(144_000 * rate / 44_100 + usize::from((header[2] >> 1) & 1))
}

fn frames(
    pending: &mut Vec<u8>,
    chunk: &[u8],
    mut publish: impl FnMut(Vec<u8>) -> io::Result<()>,
) -> io::Result<()> {
    pending.extend_from_slice(chunk);
    let mut offset = 0;
    while pending.len() - offset >= 4 {
        let Some(length) = frame_len(&pending[offset..]) else {
            offset += 1;
            continue;
        };
        if pending.len() - offset < length {
            break;
        }
        publish(pending[offset..offset + length].to_vec())?;
        offset += length;
    }
    drop(pending.drain(..offset));
    Ok(())
}

/// Read the complete request with both a size bound and an absolute deadline.
/// A slow partial request occupies at most one of the bounded client slots.
fn request(peer: &mut TcpStream, stop: &AtomicBool) -> io::Result<Vec<u8>> {
    peer.set_read_timeout(Some(IO_POLL))?;
    let deadline = Instant::now() + HEADER_TIMEOUT;
    let mut bytes = Vec::new();
    let mut chunk = [0; 512];
    loop {
        if stop.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "stream cancelled",
            ));
        }
        if Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "HTTP header deadline",
            ));
        }
        let available = (HEADER_LIMIT - bytes.len()).min(chunk.len());
        if available == 0 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "HTTP header limit",
            ));
        }
        match peer.read(&mut chunk[..available]) {
            Ok(0) => return Err(io::ErrorKind::UnexpectedEof.into()),
            Ok(count) => bytes.extend_from_slice(&chunk[..count]),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) =>
            {
                continue;
            }
            Err(error) => return Err(error),
        }
        if let Some(end) = bytes.windows(4).position(|part| part == b"\r\n\r\n") {
            bytes.truncate(end);
            return Ok(bytes);
        }
    }
}

fn serve_peer(mut peer: TcpStream, path: &str, audio: &Audio, stop: &AtomicBool) -> io::Result<()> {
    peer.set_write_timeout(Some(Duration::from_secs(1)))?;
    let header = request(&mut peer, stop)?;
    let first = header
        .split(|byte| *byte == b'\n')
        .next()
        .unwrap_or_default();
    let first = std::str::from_utf8(first).unwrap_or_default();
    let fields: Vec<_> = first.split_whitespace().collect();
    if fields.len() != 3
        || !matches!(fields[0], "GET" | "HEAD")
        || fields[1] != path
        || !matches!(fields[2], "HTTP/1.1" | "HTTP/1.0")
    {
        return peer.write_all(NOT_FOUND);
    }
    let mut cursor = match audio.subscribe() {
        Ok(cursor) => cursor,
        Err(error) if error.kind() == io::ErrorKind::TimedOut => {
            return peer.write_all(UNAVAILABLE);
        }
        Err(error) => return Err(error),
    };
    peer.write_all(AUDIO_HEADERS)?;
    if fields[0] == "HEAD" {
        return Ok(());
    }
    let mut reader = Reader::new(&audio.readers);
    while let Some(frame) = audio.next(&mut cursor, stop)? {
        peer.write_all(&frame)?;
        reader.delivered();
    }
    Ok(())
}

pub(super) fn serve(
    listener: &TcpListener,
    path: &str,
    audio: &Arc<Audio>,
    stop: &Arc<AtomicBool>,
) -> io::Result<()> {
    listener.set_nonblocking(true)?;
    let mut clients: Vec<JoinHandle<()>> = Vec::new();
    let result = (|| {
        while !stop.load(Ordering::Acquire) {
            let mut index = 0;
            while index < clients.len() {
                if clients[index].is_finished() {
                    let _ = clients.swap_remove(index).join();
                } else {
                    index += 1;
                }
            }
            match listener.accept() {
                Ok((mut peer, _)) => {
                    // On macOS an accepted socket inherits the listener's
                    // non-blocking mode (not on Linux), where its timeouts
                    // do nothing and a full send buffer ends the stream.
                    if peer.set_nonblocking(false).is_err() {
                        continue;
                    }
                    if clients.len() >= MAX_CLIENTS {
                        let _ = peer.set_write_timeout(Some(IO_POLL));
                        let _ = peer.write_all(UNAVAILABLE);
                        continue;
                    }
                    let path = path.to_owned();
                    let audio = Arc::clone(audio);
                    let stop = Arc::clone(stop);
                    clients.push(
                        thread::Builder::new()
                            .name("fsonos-mp3-reader".into())
                            .spawn(move || {
                                // A peer reset, timeout or lag is local to this reader.
                                let _ = serve_peer(peer, &path, &audio, &stop);
                            })?,
                    );
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(10));
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    })();
    stop.store(true, Ordering::Release);
    for client in clients {
        let _ = client.join();
    }
    result
}

/// Own the child and both worker threads from the first fallible setup step.
/// Every error path and unwind kills/reaps capture before joining its pipe reader.
pub(super) struct LiveStream {
    encoder: Child,
    audio: Arc<Audio>,
    stop: Arc<AtomicBool>,
    cancel: Arc<AtomicBool>,
    producer: Option<JoinHandle<io::Result<()>>>,
    server: Option<JoinHandle<io::Result<()>>>,
    closed: bool,
}

impl LiveStream {
    pub(super) fn start(
        encoder: Child,
        listener: TcpListener,
        path: String,
        cancel: Arc<AtomicBool>,
    ) -> io::Result<Self> {
        let mut stream = Self {
            encoder,
            audio: Arc::new(Audio::default()),
            stop: Arc::new(AtomicBool::new(false)),
            cancel,
            producer: None,
            server: None,
            closed: false,
        };
        let stdout = stream
            .encoder
            .stdout
            .take()
            .ok_or_else(|| io::Error::other("FFmpeg did not open stdout"))?;
        let audio = Arc::clone(&stream.audio);
        let stop = Arc::clone(&stream.stop);
        stream.producer = Some(
            thread::Builder::new()
                .name("fsonos-mp3-capture".into())
                .spawn(move || audio.ingest(stdout, &stop))?,
        );
        let audio = Arc::clone(&stream.audio);
        let stop = Arc::clone(&stream.stop);
        stream.server = Some(
            thread::Builder::new()
                .name("fsonos-live-mp3".into())
                .spawn(move || serve(&listener, &path, &audio, &stop))?,
        );
        Ok(stream)
    }

    pub(super) fn wait_ready(&mut self, timeout: Duration) -> io::Result<()> {
        self.audio.wait_ready(&self.cancel, timeout)?;
        self.check()
    }

    /// Application launch waits for actual HTTP delivery, not merely Play's
    /// reply. This proves a live consumer, not acoustic playback or identity.
    pub(super) fn wait_for_reader(&mut self, timeout: Duration) -> io::Result<()> {
        let audio = Arc::clone(&self.audio);
        await_reader(&audio.readers, timeout, || self.check())
    }

    pub(super) fn check(&mut self) -> io::Result<()> {
        if self.cancel.load(Ordering::Acquire) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "stream cancelled",
            ));
        }
        if self.stop.load(Ordering::Acquire) {
            return Err(io::Error::other("the live MP3 workers are stopping"));
        }
        if let Some(status) = self.encoder.try_wait()? {
            return Err(io::Error::other(format!(
                "FFmpeg stopped ({status}); check --format and --input"
            )));
        }
        if self.producer.as_ref().is_some_and(JoinHandle::is_finished) {
            return Err(io::Error::other(
                "the encoder's MP3 output ended unexpectedly",
            ));
        }
        if self.server.as_ref().is_some_and(JoinHandle::is_finished) {
            return Err(io::Error::other(
                "the local MP3 listener stopped unexpectedly",
            ));
        }
        self.audio.check_fresh()
    }

    pub(super) fn wait_until_stopped(&mut self) -> io::Result<()> {
        while !self.cancel.load(Ordering::Acquire) {
            self.check()?;
            thread::sleep(IO_POLL);
        }
        Ok(())
    }

    pub(super) fn shutdown(&mut self) -> io::Result<()> {
        if self.closed {
            return Ok(());
        }
        self.closed = true;
        self.stop.store(true, Ordering::Release);
        let _ = self.encoder.kill();
        let reaped = self.encoder.wait().map(|_| ());
        let producer = join(self.producer.take());
        let server = join(self.server.take());
        reaped.and(producer).and(server)
    }
}

fn join(worker: Option<JoinHandle<io::Result<()>>>) -> io::Result<()> {
    worker.map_or(Ok(()), |worker| {
        worker
            .join()
            .map_err(|_| io::Error::other("MP3 worker panicked"))?
    })
}

impl Drop for LiveStream {
    fn drop(&mut self) {
        let _ = self.shutdown();
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod audience_tests;
