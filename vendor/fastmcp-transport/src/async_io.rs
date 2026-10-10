//! Async I/O wrappers for stdio integration with asupersync.
//!
//! This module provides async wrappers for stdin/stdout that implement
//! asupersync's `AsyncRead` and `AsyncWrite` traits.
//!
//! # Phase 0 Implementation
//!
//! In Phase 0, these wrappers perform blocking I/O internally but present
//! an async API. This allows the codebase to use async patterns that will
//! benefit from true async I/O when the runtime is upgraded.
//!
//! # Cancellation limitations
//!
//! Capability-accepting convenience methods run `Cx::checkpoint()` before
//! entering synchronous I/O and between bounded line-buffer fills. This
//! observes cancellation masking as well as deadline, poll-quota, and
//! cost-quota exhaustion. The `AsyncRead`/`AsyncWrite` poll methods themselves
//! have no `Cx` and perform blocking standard-library I/O. None of these
//! checkpoints can interrupt an operation once the underlying read, write,
//! flush, or stdout lock blocks. Unix stdio server entrypoints use the separate
//! bounded nonblocking stdout commit method instead of these legacy writes.

use asupersync::Cx;
use asupersync::io::{AsyncRead, AsyncWrite, ReadBuf};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::pin::Pin;
use std::sync::Mutex;
use std::task::{Context, Poll};
#[cfg(unix)]
use std::time::{Duration, Instant};

fn io_checkpoint(cx: &Cx) -> io::Result<()> {
    cx.checkpoint().map_err(|error| {
        // This layer exposes std::io traits, which have no cancellation or
        // budget error type. Preserve the full reason on `Cx` for transport
        // callers to classify, and use Interrupted for every cooperative stop.
        io::Error::new(io::ErrorKind::Interrupted, error.to_string())
    })
}

/// Async wrapper for stdin.
///
/// Provides an `AsyncRead` implementation over stdin. In Phase 0, this
/// performs blocking reads internally but presents an async API.
///
/// # Example
///
/// ```ignore
/// use fastmcp_transport::async_io::AsyncStdin;
/// use asupersync::io::AsyncReadExt;
///
/// let mut stdin = AsyncStdin::new();
/// let mut buf = String::new();
/// stdin.read_to_string(&mut buf).await?;
/// ```
#[derive(Debug)]
pub struct AsyncStdin {
    inner: BufReader<std::io::Stdin>,
}

impl AsyncStdin {
    /// Creates a new `AsyncStdin` wrapping the standard input.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: BufReader::new(std::io::stdin()),
        }
    }
}

impl Default for AsyncStdin {
    fn default() -> Self {
        Self::new()
    }
}

impl AsyncRead for AsyncStdin {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // Phase 0: Blocking read, immediate Poll::Ready
        let n = self.inner.read(buf.unfilled())?;
        buf.advance(n);
        Poll::Ready(Ok(()))
    }
}

/// Async wrapper for stdout.
///
/// Provides an `AsyncWrite` implementation over stdout. In Phase 0, this
/// performs blocking writes internally but presents an async API.
///
/// # Example
///
/// ```ignore
/// use fastmcp_transport::async_io::AsyncStdout;
/// use asupersync::io::AsyncWriteExt;
///
/// let mut stdout = AsyncStdout::new();
/// stdout.write_all(b"hello\n").await?;
/// stdout.flush().await?;
/// ```
#[derive(Debug)]
pub struct AsyncStdout {
    inner: std::io::Stdout,
}

static STDOUT_LOCK: Mutex<()> = Mutex::new(());

#[cfg(unix)]
fn lock_stdout_until(deadline: Instant) -> io::Result<std::sync::MutexGuard<'static, ()>> {
    loop {
        match STDOUT_LOCK.try_lock() {
            Ok(guard) => return Ok(guard),
            Err(std::sync::TryLockError::Poisoned(_)) => {
                return Err(io::Error::other("stdout lock poisoned"));
            }
            Err(std::sync::TryLockError::WouldBlock) => {
                let now = Instant::now();
                if now >= deadline {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "stdout lock acquisition exceeded the commit deadline",
                    ));
                }
                std::thread::park_timeout(
                    deadline
                        .saturating_duration_since(now)
                        .min(Duration::from_millis(1)),
                );
            }
        }
    }
}

/// Commits bytes to an exclusively owned, unbuffered Unix descriptor.
/// Callers serialize access and treat every error as connection-fatal.
#[cfg(unix)]
pub(crate) fn write_all_fd_until(
    writer: &impl std::os::fd::AsFd,
    mut bytes: &[u8],
    deadline: Instant,
) -> io::Result<()> {
    let flags = rustix::fs::fcntl_getfl(writer).map_err(io::Error::from)?;
    let restore_flags = !flags.contains(rustix::fs::OFlags::NONBLOCK);
    if restore_flags {
        rustix::fs::fcntl_setfl(writer, flags | rustix::fs::OFlags::NONBLOCK)
            .map_err(io::Error::from)?;
    }

    let write_result = (|| {
        while !bytes.is_empty() {
            let now = Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "stdio write exceeded the commit deadline",
                ));
            }
            let poll_timeout =
                rustix::event::Timespec::try_from(deadline.saturating_duration_since(now))
                    .map_err(|_| {
                        io::Error::new(io::ErrorKind::InvalidInput, "timeout out of range")
                    })?;
            let mut poll_fds = [rustix::event::PollFd::new(
                writer,
                rustix::event::PollFlags::OUT,
            )];
            match rustix::event::poll(&mut poll_fds, Some(&poll_timeout)) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        "stdio writability exceeded the commit deadline",
                    ));
                }
                Ok(_) => {}
                Err(rustix::io::Errno::INTR) => continue,
                Err(error) => return Err(io::Error::from(error)),
            }

            match rustix::io::write(writer, bytes) {
                Ok(0) => return Err(io::Error::from(io::ErrorKind::WriteZero)),
                Ok(written) => bytes = &bytes[written..],
                Err(rustix::io::Errno::INTR | rustix::io::Errno::AGAIN) => continue,
                Err(error) => return Err(io::Error::from(error)),
            }
        }
        Ok(())
    })();
    let restore_result = if restore_flags {
        rustix::fs::fcntl_setfl(writer, flags).map_err(io::Error::from)
    } else {
        Ok(())
    };
    match (write_result, restore_result) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), _) | (Ok(()), Err(error)) => Err(error),
    }
}

impl AsyncStdout {
    /// Creates a new `AsyncStdout` wrapping the standard output.
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: std::io::stdout(),
        }
    }

    /// Writes data to stdout after a context checkpoint.
    ///
    /// This is a preflight check only; it cannot interrupt the write or stdout
    /// lock acquisition after either operation begins.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::Interrupted`] if the context checkpoint observes
    /// cancellation or budget exhaustion, or an I/O error from stdout.
    pub fn write_all_sync(&mut self, cx: &Cx, buf: &[u8]) -> io::Result<()> {
        io_checkpoint(cx)?;

        let _guard = STDOUT_LOCK
            .lock()
            .map_err(|_| io::Error::other("stdout lock poisoned"))?;
        self.inner.write_all(buf)
    }

    /// Flushes stdout after a context checkpoint.
    ///
    /// This is a preflight check only; it cannot interrupt the flush or stdout
    /// lock acquisition after either operation begins.
    ///
    /// # Errors
    ///
    /// Returns [`io::ErrorKind::Interrupted`] if the context checkpoint observes
    /// cancellation or budget exhaustion, or an I/O error from stdout.
    pub fn flush_sync(&mut self, cx: &Cx) -> io::Result<()> {
        io_checkpoint(cx)?;

        let _guard = STDOUT_LOCK
            .lock()
            .map_err(|_| io::Error::other("stdout lock poisoned"))?;
        self.inner.flush()
    }

    // --- Unchecked methods for two-phase commit ---

    /// Writes data to stdout without checking cancellation.
    ///
    /// This is used in the commit phase of two-phase sends, where
    /// cancellation has already been checked at reserve time.
    ///
    /// # Errors
    ///
    /// Returns an error only on I/O failure.
    pub fn write_all_unchecked(&mut self, buf: &[u8]) -> io::Result<()> {
        let _guard = STDOUT_LOCK
            .lock()
            .map_err(|_| io::Error::other("stdout lock poisoned"))?;
        self.inner.write_all(buf)
    }

    /// Flushes stdout without checking cancellation.
    ///
    /// This is used in the commit phase of two-phase sends, where
    /// cancellation has already been checked at reserve time.
    ///
    /// # Errors
    ///
    /// Returns an error only on I/O failure.
    pub fn flush_unchecked(&mut self) -> io::Result<()> {
        let _guard = STDOUT_LOCK
            .lock()
            .map_err(|_| io::Error::other("stdout lock poisoned"))?;
        self.inner.flush()
    }

    /// Commits one complete byte sequence to Unix stdout with a wall-clock
    /// bound and nonblocking descriptor writes.
    ///
    /// The method owns the process-wide stdout serialization lock for the
    /// entire sequence, enables `O_NONBLOCK` on fd 1, polls for writability,
    /// and retries partial writes and `EINTR`/`EAGAIN` until completion. Once a
    /// prefix has been written, any later error is connection-fatal because a
    /// subsequent frame could not safely repair the byte stream. If this call
    /// enables `O_NONBLOCK`, it attempts to restore the original descriptor
    /// flags before releasing the process-local stdout lock. Restoration
    /// failure is returned as a connection-fatal I/O error; in that case the
    /// descriptor may remain nonblocking. A separately inherited/duped
    /// descriptor can also observe the temporary flag change because Unix
    /// stores status flags on the shared open-file description.
    ///
    /// Regular files and some device/filesystem implementations may ignore
    /// `O_NONBLOCK`; the bound is guaranteed for ordinary Unix pipes and
    /// sockets, which are the supported MCP stdio host boundary.
    ///
    /// # Errors
    ///
    /// Returns an invalid-input error for a zero timeout, a timeout error when
    /// lock acquisition or writability exceeds the bound, or the underlying
    /// descriptor/poll/write error.
    #[cfg(unix)]
    pub fn write_all_bounded(&mut self, bytes: &[u8], timeout: Duration) -> io::Result<()> {
        if timeout.is_zero() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "stdout commit timeout must be nonzero",
            ));
        }
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "timeout overflow"))?;
        let _guard = lock_stdout_until(deadline)?;
        write_all_fd_until(&self.inner, bytes, deadline)
    }
}

/// Implement std::io::Write for AsyncStdout to enable two-phase send.
///
/// These methods bypass cancellation checks because they're used in the
/// commit phase of two-phase sends, where cancellation was already checked
/// during reservation.
impl Write for AsyncStdout {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let _guard = STDOUT_LOCK
            .lock()
            .map_err(|_| io::Error::other("stdout lock poisoned"))?;
        self.inner.write(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        let _guard = STDOUT_LOCK
            .lock()
            .map_err(|_| io::Error::other("stdout lock poisoned"))?;
        self.inner.flush()
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        let _guard = STDOUT_LOCK
            .lock()
            .map_err(|_| io::Error::other("stdout lock poisoned"))?;
        self.inner.write_all(buf)
    }
}

impl Default for AsyncStdout {
    fn default() -> Self {
        Self::new()
    }
}

impl AsyncWrite for AsyncStdout {
    fn poll_write(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        // Phase 0: Blocking write, immediate Poll::Ready
        let _guard = STDOUT_LOCK
            .lock()
            .map_err(|_| io::Error::other("stdout lock poisoned"))?;
        let n = self.inner.write(buf)?;
        Poll::Ready(Ok(n))
    }

    fn poll_flush(mut self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let _guard = STDOUT_LOCK
            .lock()
            .map_err(|_| io::Error::other("stdout lock poisoned"))?;
        self.inner.flush()?;
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Stdout doesn't need explicit shutdown
        Poll::Ready(Ok(()))
    }
}

/// Async line reader with explicit context checkpoints.
///
/// This struct checkpoints the capability context before blocking reads and
/// between bounded buffer fills. Checkpoints respect masking and observe
/// cancellation plus deadline and quota exhaustion. They cannot interrupt an
/// underlying stdin read that is already blocked.
///
/// # Example
///
/// ```ignore
/// use fastmcp_transport::async_io::AsyncLineReader;
/// use asupersync::Cx;
///
/// let cx = Cx::for_testing();
/// let mut reader = AsyncLineReader::new();
///
/// loop {
///     match reader.read_line(&cx) {
///         Ok(Some(line)) => process_line(&line),
///         Ok(None) => break, // EOF
///         Err(e) if e.kind() == io::ErrorKind::Interrupted => break, // Cancelled
///         Err(e) => return Err(e),
///     }
/// }
/// ```
#[derive(Debug)]
pub struct AsyncLineReader {
    stdin: AsyncStdin,
    buffer: Vec<u8>,
    terminal: bool,
}

/// Default bound used by the public line-reading convenience methods.
///
/// Transport implementations with their own configured codec limit use the
/// crate-private bounded byte API instead.
const DEFAULT_MAX_LINE_SIZE: usize = 10 * 1024 * 1024;

#[derive(Debug)]
pub(crate) enum BoundedLineReadError {
    Io(io::Error),
    TooLarge(usize),
}

impl From<io::Error> for BoundedLineReadError {
    fn from(error: io::Error) -> Self {
        Self::Io(error)
    }
}

fn bounded_frame_len(buffer: &[u8]) -> usize {
    let without_lf = buffer.strip_suffix(b"\n").unwrap_or(buffer);
    without_lf.strip_suffix(b"\r").unwrap_or(without_lf).len()
}

fn bounded_line_failure_is_terminal(error: &BoundedLineReadError, buffer: &[u8]) -> bool {
    matches!(error, BoundedLineReadError::TooLarge(_))
        || matches!(error, BoundedLineReadError::Io(_)) && !buffer.is_empty()
}

fn read_line_bytes_bounded_from<R: BufRead>(
    reader: &mut R,
    buffer: &mut Vec<u8>,
    cx: &Cx,
    max_frame_size: usize,
) -> Result<Option<usize>, BoundedLineReadError> {
    buffer.clear();
    let wire_limit = max_frame_size.saturating_add(2);

    loop {
        io_checkpoint(cx)?;

        let available = match reader.fill_buf() {
            Ok(available) => available,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {
                // A signal interruption is transient. Re-enter the full
                // context checkpoint before retrying so cancellation,
                // deadlines, quotas, and masking retain their authoritative
                // semantics without discarding an already-read frame prefix.
                io_checkpoint(cx)?;
                continue;
            }
            Err(error) => return Err(error.into()),
        };
        if available.is_empty() {
            if buffer.is_empty() {
                return Ok(None);
            }
            let frame_len = bounded_frame_len(buffer);
            if frame_len > max_frame_size {
                buffer.clear();
                return Err(BoundedLineReadError::TooLarge(frame_len));
            }
            return Ok(Some(frame_len));
        }

        let newline = available.iter().position(|byte| *byte == b'\n');
        let bytes_to_consume = newline.map_or(available.len(), |position| position + 1);
        let projected = buffer.len().saturating_add(bytes_to_consume);
        if projected > wire_limit {
            buffer.clear();
            return Err(BoundedLineReadError::TooLarge(projected));
        }

        buffer.extend_from_slice(&available[..bytes_to_consume]);
        reader.consume(bytes_to_consume);

        if newline.is_some() {
            let frame_len = bounded_frame_len(buffer);
            if frame_len > max_frame_size {
                buffer.clear();
                return Err(BoundedLineReadError::TooLarge(frame_len));
            }
            return Ok(Some(frame_len));
        }
    }
}

impl AsyncLineReader {
    /// Creates a new `AsyncLineReader`.
    #[must_use]
    pub fn new() -> Self {
        Self {
            stdin: AsyncStdin::new(),
            buffer: Vec::with_capacity(4096),
            terminal: false,
        }
    }

    /// Reads at most one line without allowing `BufRead::read_line` to grow a
    /// string past the caller's frame limit.
    ///
    /// The two extra wire bytes permit an exact-limit JSON frame followed by
    /// CRLF. They are removed before the returned frame length is checked.
    fn read_line_bytes_bounded(
        &mut self,
        cx: &Cx,
        max_frame_size: usize,
    ) -> Result<Option<usize>, BoundedLineReadError> {
        if self.terminal {
            return Err(BoundedLineReadError::Io(io::Error::new(
                io::ErrorKind::InvalidData,
                "line reader is terminal after a prior partial-frame failure",
            )));
        }
        let result = read_line_bytes_bounded_from(
            &mut self.stdin.inner,
            &mut self.buffer,
            cx,
            max_frame_size,
        );
        if let Err(error) = &result
            && bounded_line_failure_is_terminal(error, &self.buffer)
        {
            self.terminal = true;
        }
        result
    }

    pub(crate) fn read_non_empty_line_bounded(
        &mut self,
        cx: &Cx,
        max_frame_size: usize,
    ) -> Result<Option<&[u8]>, BoundedLineReadError> {
        loop {
            let Some(frame_len) = self.read_line_bytes_bounded(cx, max_frame_size)? else {
                return Ok(None);
            };
            if frame_len != 0 {
                return Ok(Some(&self.buffer[..frame_len]));
            }
        }
    }

    /// Reads a line from stdin with context checkpoints.
    ///
    /// Returns `Ok(Some(line))` when a line is read, `Ok(None)` on EOF,
    /// or an error on cancellation/I/O failure. Lines are bounded to 10 MiB
    /// before allocation into the reusable buffer.
    ///
    /// # Errors
    ///
    /// - Returns `io::ErrorKind::Interrupted` if cancellation or a context
    ///   budget is observed at a checkpoint.
    /// - Returns other I/O errors as-is.
    ///
    /// A size-limit error is terminal for this reader because the unread
    /// remainder of the oversized line is deliberately not drained. An I/O,
    /// cancellation, or budget error after a frame prefix was consumed is also
    /// terminal: retrying could reinterpret the suffix as a new frame. A
    /// checkpoint failure before any bytes are consumed remains retryable.
    pub fn read_line(&mut self, cx: &Cx) -> io::Result<Option<String>> {
        let Some(frame_len) = self
            .read_line_bytes_bounded(cx, DEFAULT_MAX_LINE_SIZE)
            .map_err(|error| match error {
                BoundedLineReadError::Io(error) => error,
                BoundedLineReadError::TooLarge(size) => io::Error::new(
                    io::ErrorKind::InvalidData,
                    format!("line exceeds maximum size: {size} bytes"),
                ),
            })?
        else {
            return Ok(None);
        };

        let line = std::str::from_utf8(&self.buffer[..frame_len])
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
            .to_owned();
        Ok(Some(line))
    }

    /// Reads a non-empty line, skipping empty lines.
    ///
    /// Returns `Ok(Some(line))` when a non-empty line is read, `Ok(None)` on EOF,
    /// or an error on cancellation/I/O failure.
    ///
    /// This method runs a context checkpoint between each line read.
    ///
    /// # Errors
    ///
    /// - Returns `io::ErrorKind::Interrupted` if cancellation or a context
    ///   budget is observed at a checkpoint.
    /// - Returns other I/O errors as-is.
    pub fn read_non_empty_line(&mut self, cx: &Cx) -> io::Result<Option<String>> {
        loop {
            io_checkpoint(cx)?;

            match self.read_line(cx)? {
                None => return Ok(None),            // EOF
                Some(line) if line.is_empty() => {} // Skip empty lines, continue looping
                Some(line) => return Ok(Some(line)),
            }
        }
    }
}

impl Default for AsyncLineReader {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    enum DeterministicBufReadStep {
        Bytes(Vec<u8>),
        Interrupted,
        Eof,
    }

    struct DeterministicInterruptedBufRead {
        steps: VecDeque<DeterministicBufReadStep>,
        current: Vec<u8>,
        offset: usize,
        cancel_on_interrupt: Option<Cx>,
    }

    impl Read for DeterministicInterruptedBufRead {
        fn read(&mut self, destination: &mut [u8]) -> io::Result<usize> {
            let available = self.fill_buf()?;
            let read = available.len().min(destination.len());
            destination[..read].copy_from_slice(&available[..read]);
            self.consume(read);
            Ok(read)
        }
    }

    impl BufRead for DeterministicInterruptedBufRead {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            if self.offset < self.current.len() {
                return Ok(&self.current[self.offset..]);
            }

            match self.steps.pop_front() {
                Some(DeterministicBufReadStep::Bytes(bytes)) => {
                    assert!(!bytes.is_empty(), "byte steps must make progress");
                    self.current = bytes;
                    self.offset = 0;
                    Ok(&self.current)
                }
                Some(DeterministicBufReadStep::Interrupted) => {
                    if let Some(cx) = &self.cancel_on_interrupt {
                        cx.set_cancel_requested(true);
                    }
                    Err(io::Error::new(
                        io::ErrorKind::Interrupted,
                        "deterministic signal interruption",
                    ))
                }
                Some(DeterministicBufReadStep::Eof) | None => Ok(&[]),
            }
        }

        fn consume(&mut self, amount: usize) {
            self.offset = self.offset.saturating_add(amount).min(self.current.len());
        }
    }

    // =========================================================================
    // AsyncStdin tests
    // =========================================================================

    #[test]
    fn async_stdin_new_creates_instance() {
        let stdin = AsyncStdin::new();
        // Verify the struct is created successfully
        assert!(format!("{stdin:?}").contains("AsyncStdin"));
    }

    #[test]
    fn async_stdin_default_creates_instance() {
        let stdin = AsyncStdin::default();
        assert!(format!("{stdin:?}").contains("AsyncStdin"));
    }

    // =========================================================================
    // AsyncStdout tests
    // =========================================================================

    #[test]
    fn async_stdout_new_creates_instance() {
        let stdout = AsyncStdout::new();
        assert!(format!("{stdout:?}").contains("AsyncStdout"));
    }

    #[test]
    fn async_stdout_default_creates_instance() {
        let stdout = AsyncStdout::default();
        assert!(format!("{stdout:?}").contains("AsyncStdout"));
    }

    #[test]
    fn async_stdout_write_all_sync_respects_cancellation() {
        let mut stdout = AsyncStdout::new();
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        let result = stdout.write_all_sync(&cx, b"test data");
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(err.to_string().to_ascii_lowercase().contains("cancel"));
    }

    #[test]
    fn async_stdout_flush_sync_respects_cancellation() {
        let mut stdout = AsyncStdout::new();
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        let result = stdout.flush_sync(&cx);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
        assert!(err.to_string().to_ascii_lowercase().contains("cancel"));
    }

    #[test]
    fn io_checkpoint_observes_budgets_cancellation_and_masking() {
        let deadline_cx = Cx::for_testing_with_budget(
            asupersync::Budget::new().with_deadline(asupersync::Time::ZERO),
        );
        assert_eq!(
            io_checkpoint(&deadline_cx).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert!(matches!(
            deadline_cx.cancel_reason().map(|reason| reason.kind),
            Some(asupersync::CancelKind::Deadline)
        ));

        let quota_cx = Cx::for_testing_with_budget(asupersync::Budget::new().with_poll_quota(0));
        assert_eq!(
            io_checkpoint(&quota_cx).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
        assert!(matches!(
            quota_cx.cancel_reason().map(|reason| reason.kind),
            Some(asupersync::CancelKind::PollQuota)
        ));

        let masked_deadline_cx = Cx::for_testing_with_budget(
            asupersync::Budget::new().with_deadline(asupersync::Time::ZERO),
        );
        masked_deadline_cx.masked(|| {
            io_checkpoint(&masked_deadline_cx)
                .expect("masking defers deadline enforcement at the checkpoint");
        });
        assert_eq!(
            io_checkpoint(&masked_deadline_cx).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );

        let masked_cancel_cx = Cx::for_testing();
        masked_cancel_cx.set_cancel_requested(true);
        masked_cancel_cx.masked(|| {
            io_checkpoint(&masked_cancel_cx)
                .expect("masking defers explicit cancellation at the checkpoint");
        });
        assert_eq!(
            io_checkpoint(&masked_cancel_cx).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn async_stdout_checkpoint_methods_preserve_masking_and_reject_budgets() {
        let mut stdout = AsyncStdout::new();
        let deadline_cx = Cx::for_testing_with_budget(
            asupersync::Budget::new().with_deadline(asupersync::Time::ZERO),
        );
        assert_eq!(
            stdout.write_all_sync(&deadline_cx, b"").unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );

        let quota_cx = Cx::for_testing_with_budget(asupersync::Budget::new().with_cost_quota(0));
        assert_eq!(
            stdout.flush_sync(&quota_cx).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );

        let masked_cx = Cx::for_testing();
        masked_cx.set_cancel_requested(true);
        masked_cx.masked(|| {
            stdout
                .write_all_sync(&masked_cx, b"")
                .expect("masked empty write is admitted");
            stdout
                .flush_sync(&masked_cx)
                .expect("masked flush is admitted");
        });
    }

    #[test]
    fn async_stdout_write_all_unchecked_succeeds() {
        let mut stdout = AsyncStdout::new();
        // This writes to actual stdout but should succeed
        let result = stdout.write_all_unchecked(b"");
        assert!(result.is_ok());
    }

    #[test]
    fn async_stdout_flush_unchecked_succeeds() {
        let mut stdout = AsyncStdout::new();
        let result = stdout.flush_unchecked();
        assert!(result.is_ok());
    }

    #[test]
    fn async_stdout_write_trait_write_succeeds() {
        let mut stdout = AsyncStdout::new();
        // Test Write trait implementation
        let result = Write::write(&mut stdout, b"");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), 0);
    }

    #[test]
    fn async_stdout_write_trait_flush_succeeds() {
        let mut stdout = AsyncStdout::new();
        let result = Write::flush(&mut stdout);
        assert!(result.is_ok());
    }

    #[test]
    fn async_stdout_write_trait_write_all_succeeds() {
        let mut stdout = AsyncStdout::new();
        let result = Write::write_all(&mut stdout, b"");
        assert!(result.is_ok());
    }

    #[test]
    fn async_stdout_poll_write_returns_ready() {
        use std::task::Waker;

        let mut stdout = AsyncStdout::new();
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        let result = Pin::new(&mut stdout).poll_write(&mut cx, b"");
        assert!(matches!(result, Poll::Ready(Ok(0))));
    }

    #[test]
    fn async_stdout_poll_flush_returns_ready() {
        use std::task::Waker;

        let mut stdout = AsyncStdout::new();
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        let result = Pin::new(&mut stdout).poll_flush(&mut cx);
        assert!(matches!(result, Poll::Ready(Ok(()))));
    }

    #[test]
    fn async_stdout_poll_shutdown_returns_ready() {
        use std::task::Waker;

        let mut stdout = AsyncStdout::new();
        let waker = Waker::noop();
        let mut cx = Context::from_waker(waker);

        let result = Pin::new(&mut stdout).poll_shutdown(&mut cx);
        assert!(matches!(result, Poll::Ready(Ok(()))));
    }

    // =========================================================================
    // AsyncLineReader tests
    // =========================================================================

    #[test]
    fn async_line_reader_new_creates_instance() {
        let reader = AsyncLineReader::new();
        assert!(format!("{reader:?}").contains("AsyncLineReader"));
    }

    #[test]
    fn async_line_reader_default_creates_instance() {
        let reader = AsyncLineReader::default();
        assert!(format!("{reader:?}").contains("AsyncLineReader"));
    }

    #[test]
    fn async_line_reader_has_preallocated_buffer() {
        let reader = AsyncLineReader::new();
        // The buffer is initialized with capacity 4096
        // We can verify through debug output that it exists
        let debug = format!("{reader:?}");
        assert!(debug.contains("buffer"));
    }

    #[test]
    fn async_line_reader_read_line_respects_cancellation() {
        let mut reader = AsyncLineReader::new();
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        // The bounded reader checks cancellation before touching stdin.
        let result = reader.read_line(&cx);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn async_line_reader_read_non_empty_line_respects_cancellation() {
        let mut reader = AsyncLineReader::new();
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        let result = reader.read_non_empty_line(&cx);
        assert!(result.is_err());
        assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn async_line_reader_read_non_empty_line_checks_cancellation_early() {
        // Test that cancellation is checked at the start of read_non_empty_line,
        // not just within read_line
        let mut reader = AsyncLineReader::new();
        let cx = Cx::for_testing();
        cx.set_cancel_requested(true);

        // Should return immediately without trying to read
        let result = reader.read_non_empty_line(&cx);
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }

    #[test]
    fn async_line_reader_rejects_deadline_and_quota_before_touching_stdin() {
        let mut reader = AsyncLineReader::new();
        let deadline_cx = Cx::for_testing_with_budget(
            asupersync::Budget::new().with_deadline(asupersync::Time::ZERO),
        );
        assert_eq!(
            reader.read_line(&deadline_cx).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );

        let quota_cx = Cx::for_testing_with_budget(asupersync::Budget::new().with_poll_quota(0));
        assert_eq!(
            reader.read_non_empty_line(&quota_cx).unwrap_err().kind(),
            io::ErrorKind::Interrupted
        );
    }

    #[test]
    fn reality_check_regression_async_line_reader_retries_interrupted_fill_with_prefix() {
        let mut reader = DeterministicInterruptedBufRead {
            steps: VecDeque::from([
                DeterministicBufReadStep::Bytes(b"frame-pre".to_vec()),
                DeterministicBufReadStep::Interrupted,
                DeterministicBufReadStep::Bytes(b"fix\nnext-frame\n".to_vec()),
                DeterministicBufReadStep::Eof,
            ]),
            current: Vec::new(),
            offset: 0,
            cancel_on_interrupt: None,
        };
        let mut buffer = Vec::new();

        let frame_len = read_line_bytes_bounded_from(
            &mut reader,
            &mut buffer,
            &Cx::for_testing(),
            DEFAULT_MAX_LINE_SIZE,
        )
        .expect("a transient interruption must be retried")
        .expect("the complete frame must be returned");

        assert_eq!(&buffer[..frame_len], b"frame-prefix");
        assert_eq!(
            reader.fill_buf().expect("the suffix remains buffered"),
            b"next-frame\n"
        );
    }

    #[test]
    fn reality_check_regression_async_line_reader_checks_context_after_interruption() {
        let cx = Cx::for_testing();
        let mut reader = DeterministicInterruptedBufRead {
            steps: VecDeque::from([
                DeterministicBufReadStep::Bytes(b"partial".to_vec()),
                DeterministicBufReadStep::Interrupted,
                DeterministicBufReadStep::Bytes(b"must-not-be-read\n".to_vec()),
            ]),
            current: Vec::new(),
            offset: 0,
            cancel_on_interrupt: Some(cx.clone()),
        };
        let mut buffer = Vec::new();

        let error =
            read_line_bytes_bounded_from(&mut reader, &mut buffer, &cx, DEFAULT_MAX_LINE_SIZE)
                .expect_err("the context cancellation must stop the retry");

        assert!(matches!(
            error,
            BoundedLineReadError::Io(ref error)
                if error.kind() == io::ErrorKind::Interrupted
        ));
        assert!(bounded_line_failure_is_terminal(&error, &buffer));
        assert_eq!(buffer, b"partial");
        assert!(matches!(
            reader.steps.front(),
            Some(DeterministicBufReadStep::Bytes(bytes))
                if bytes.as_slice() == b"must-not-be-read\n"
        ));
    }

    #[test]
    fn reality_check_regression_async_line_reader_rejects_retry_after_partial_failure() {
        let mut reader = AsyncLineReader::new();
        reader.terminal = true;

        let error = reader
            .read_line(&Cx::for_testing())
            .expect_err("a terminal reader must not reinterpret a frame suffix");

        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert!(error.to_string().contains("terminal"));
    }

    // =========================================================================
    // Cancellation error message tests
    // =========================================================================

    #[test]
    fn cancellation_error_has_correct_message() {
        let err = io::Error::new(io::ErrorKind::Interrupted, "cancelled");
        assert_eq!(err.to_string(), "cancelled");
        assert_eq!(err.kind(), io::ErrorKind::Interrupted);
    }

    // =========================================================================
    // STDOUT_LOCK tests
    // =========================================================================

    #[test]
    fn stdout_lock_allows_concurrent_access() {
        // Test that the static STDOUT_LOCK can be acquired multiple times
        // (from same thread, sequentially)
        let mut stdout1 = AsyncStdout::new();
        let mut stdout2 = AsyncStdout::new();

        // Both should be able to flush (lock is acquired and released each time)
        assert!(stdout1.flush_unchecked().is_ok());
        assert!(stdout2.flush_unchecked().is_ok());
    }
}
