//! Finite, memory-bounded capture of a paired source or timestamp-aligned Pulse inputs.

use std::io::{self, Read as _};
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail, ensure};

use super::super::latency::SAMPLE_RATE;
use super::{Options, Stop, probe};

fn arguments(options: &Options, seconds: u32) -> Vec<String> {
    let mut args = vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
    ];
    args.extend(options.input.arguments());
    args.extend([
        "-t".into(),
        seconds.to_string(),
        "-vn".into(),
        "-ac".into(),
        "2".into(),
        "-ar".into(),
        "8000".into(),
        "-c:a".into(),
        "pcm_s16le".into(),
        "-f".into(),
        "s16le".into(),
        "pipe:1".into(),
    ]);
    args
}

struct Capture {
    child: Child,
    reader: Option<JoinHandle<()>>,
    probe: Option<probe::Emission>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        // Stop audible output first, including on capture failure or unwind.
        drop(self.probe.take());
        // FFmpeg is launched directly, never through a shell or a pipeline.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

enum Window {
    Ready,
    Finished(io::Result<Vec<u8>>),
}

/// Read one complete frame before authorizing an optional audible probe.
/// Retain the existing one-byte overflow sentinel, even on fragmented reads.
fn read_window(
    reader: impl io::Read,
    expected: usize,
    send: &mpsc::SyncSender<Window>,
) -> io::Result<Vec<u8>> {
    let mut reader = reader.take((expected + 1) as u64);
    let mut first = [0; 4];
    reader.read_exact(&mut first)?;
    if send.send(Window::Ready).is_err() {
        return Err(io::ErrorKind::Interrupted.into());
    }
    let mut bytes = Vec::with_capacity(expected + 1);
    bytes.extend(first);
    reader.read_to_end(&mut bytes)?;
    Ok(bytes)
}

pub(super) fn record(options: &Options, stop: &Stop) -> anyhow::Result<Option<Vec<u8>>> {
    record_with_probe(options, stop, None)
}

pub(super) fn record_with_probe(
    options: &Options,
    stop: &Stop,
    probe: Option<&probe::Config>,
) -> anyhow::Result<Option<Vec<u8>>> {
    if stop.requested() {
        return Ok(None);
    }
    let seconds = (options.max_lag_ms.saturating_mul(2).div_ceil(1_000) + 4).clamp(12, 30);
    let expected = usize::try_from(seconds)? * SAMPLE_RATE * 4;
    let mut capture = Capture {
        child: Command::new(&options.ffmpeg)
            .args(arguments(options, seconds))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .context("start local synchronized microphone/reference capture")?,
        reader: None,
        probe: None,
    };
    let stdout = capture
        .child
        .stdout
        .take()
        .context("capture has no PCM pipe")?;
    // Both messages fit even when cancellation drops the receiving loop.
    let (send, receive) = mpsc::sync_channel(2);
    capture.reader = Some(
        thread::Builder::new()
            .name("fsonos-sync-pcm".into())
            .spawn(move || {
                let result = read_window(stdout, expected, &send);
                let _ = send.send(Window::Finished(result));
            })?,
    );
    let started = Instant::now();
    let deadline = Duration::from_secs(u64::from(seconds) + 10);
    let mut samples = None;
    loop {
        if stop.requested() {
            return Ok(None);
        }
        ensure!(
            started.elapsed() < deadline,
            "synchronized capture deadline exceeded"
        );
        if samples.is_none() {
            match receive.recv_timeout(Duration::from_millis(100)) {
                Ok(Window::Ready) => {
                    if let Some(config) = probe {
                        if stop.requested() {
                            return Ok(None);
                        }
                        ensure!(
                            started.elapsed() < deadline && capture.child.try_wait()?.is_none(),
                            "capture ended before the calibration probe could start"
                        );
                        ensure!(capture.probe.is_none(), "duplicate capture readiness");
                        capture.probe = Some(probe::Emission::start(
                            &options.ffmpeg,
                            config,
                            seconds,
                        )?);
                    }
                }
                Ok(Window::Finished(result)) => {
                    let bytes = result.context("read synchronized PCM")?;
                    ensure!(
                        bytes.len() == expected,
                        "capture did not produce one complete bounded window"
                    );
                    samples = Some(bytes);
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    bail!("capture reader ended unexpectedly");
                }
            }
        } else {
            thread::sleep(Duration::from_millis(20));
        }
        if let Some(probe) = &mut capture.probe {
            probe.check()?;
        }
        if let Some(status) = capture.child.try_wait()? {
            ensure!(status.success(), "synchronized capture process failed");
            if samples.is_some() {
                return Ok(samples);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::input::Input;
    use super::*;

    #[test]
    fn capture_is_one_stereo_device_with_a_finite_raw_pcm_output() {
        let source = "paired source; not a shell";
        let options = Options {
            ffmpeg: "ffmpeg".into(),
            input: Input::Paired {
                format: "pulse".into(),
                source: source.into(),
            },
            max_lag_ms: 8_000,
        };
        let args = arguments(&options, 20);
        assert_eq!(args.iter().filter(|arg| *arg == "-i").count(), 1);
        assert_eq!(args.last().unwrap(), "pipe:1");
        for (option, value) in [
            ("-i", source),
            ("-t", "20"),
            ("-ac", "2"),
            ("-ar", "8000"),
            ("-f", "s16le"),
            ("-c:a", "pcm_s16le"),
        ] {
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == option && pair[1] == value)
            );
        }
    }

    #[test]
    fn no_probe_readiness_is_published_without_a_complete_pcm_frame() {
        for length in 0..4 {
            let (send, receive) = mpsc::sync_channel(2);
            assert!(read_window(io::Cursor::new(vec![0; length]), 16, &send).is_err());
            assert!(receive.try_recv().is_err());
        }
    }

    #[test]
    fn fragmented_pcm_readiness_preserves_bytes_and_the_overflow_bound() {
        struct Fragmented(io::Cursor<Vec<u8>>);
        impl io::Read for Fragmented {
            fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
                let take = bytes.len().min(1);
                io::Read::read(&mut self.0, &mut bytes[..take])
            }
        }
        for length in [4, 16, 17, 1_000] {
            let (send, receive) = mpsc::sync_channel(2);
            let source = Fragmented(io::Cursor::new(vec![42; length]));
            let bytes = read_window(source, 16, &send).unwrap();
            assert!(matches!(receive.try_recv().unwrap(), Window::Ready));
            assert!(receive.try_recv().is_err());
            assert_eq!(bytes, vec![42; length.min(17)]);
        }
    }

    #[test]
    fn abandoned_capture_does_not_continue_after_readiness() {
        let (send, receive) = mpsc::sync_channel(2);
        drop(receive);
        let error = read_window(io::Cursor::new(vec![0; 16]), 16, &send).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::Interrupted);
    }
}
