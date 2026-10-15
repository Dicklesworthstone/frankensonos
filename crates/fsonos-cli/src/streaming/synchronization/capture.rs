//! Finite, memory-bounded stereo capture from one owner-configured local source.

use std::io::Read as _;
use std::process::{Child, Command, Stdio};
use std::sync::mpsc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context as _, bail, ensure};

use super::super::latency::SAMPLE_RATE;
use super::{Options, Stop};

fn arguments(options: &Options, seconds: u32) -> Vec<String> {
    vec![
        "-hide_banner".into(),
        "-loglevel".into(),
        "error".into(),
        "-nostdin".into(),
        "-f".into(),
        options.format.clone(),
        "-i".into(),
        options.input.clone(),
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
    ]
}

struct Capture {
    child: Child,
    reader: Option<JoinHandle<()>>,
}

impl Drop for Capture {
    fn drop(&mut self) {
        // FFmpeg is launched directly, never through a shell or a pipeline.
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

pub(super) fn record(options: &Options, stop: &Stop) -> anyhow::Result<Option<Vec<u8>>> {
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
    };
    let stdout = capture
        .child
        .stdout
        .take()
        .context("capture has no PCM pipe")?;
    let (send, receive) = mpsc::sync_channel(1);
    capture.reader = Some(
        thread::Builder::new()
            .name("fsonos-sync-pcm".into())
            .spawn(move || {
                let mut bytes = Vec::with_capacity(expected);
                let result = stdout
                    .take((expected + 1) as u64)
                    .read_to_end(&mut bytes)
                    .map(|_| bytes);
                let _ = send.send(result);
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
                Ok(result) => {
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
    use super::*;

    #[test]
    fn capture_is_one_stereo_device_with_a_finite_raw_pcm_output() {
        let options = Options {
            ffmpeg: "ffmpeg".into(),
            format: "pulse".into(),
            input: "paired source; not a shell".into(),
            max_lag_ms: 8_000,
        };
        let args = arguments(&options, 20);
        assert_eq!(args.iter().filter(|arg| *arg == "-i").count(), 1);
        assert_eq!(args.last().unwrap(), "pipe:1");
        for (option, value) in [
            ("-i", options.input.as_str()),
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
}
