//! Explicit local acoustic feedback for mpv; ordinary streaming never records.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context as _, ensure};
use clap::Args;

use super::{StreamArgs, calibration, latency};

mod capture;
mod mpv;

// The field names are the flags (--sync-mpv, --sync-input, ...).
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Default, Args)]
pub struct SyncArgs {
    /// Opt in to local microphone feedback and correction through this mpv Unix socket.
    #[arg(long, requires = "sync_input", conflicts_with = "latency_wav")]
    pub sync_mpv: Option<PathBuf>,
    /// One time-aligned stereo capture: left=dry reference, right=room microphone.
    #[arg(long, requires = "sync_mpv", conflicts_with = "latency_wav")]
    pub sync_input: Option<String>,
    /// Uncorrected media-specific mpv delay in seconds, not the last correction.
    #[arg(
        long,
        default_value_t = 0.0,
        requires = "sync_mpv",
        allow_negative_numbers = true
    )]
    pub sync_base_delay: f64,
}

struct Options {
    ffmpeg: String,
    format: String,
    input: String,
    max_lag_ms: u32,
}

struct Stop {
    session: Arc<AtomicBool>,
    stream: Arc<AtomicBool>,
}

impl Stop {
    fn requested(&self) -> bool {
        self.session.load(Ordering::Acquire) || self.stream.load(Ordering::Acquire)
    }
}

pub struct Prepared {
    controller: mpv::Controller,
    options: Options,
}

impl Prepared {
    /// Read-only preflight before any speaker mutation. Never starts mpv itself.
    pub fn from_args(args: &StreamArgs) -> anyhow::Result<Option<Self>> {
        let Some(path) = &args.sync.sync_mpv else {
            return Ok(None);
        };
        let input = args
            .sync
            .sync_input
            .as_deref()
            .context("--sync-mpv requires --sync-input")?;
        ensure!(
            !input.trim().is_empty(),
            "--sync-input must name a paired local capture source"
        );
        ensure!(
            matches!(args.format.as_str(), "pulse" | "avfoundation" | "alsa"),
            "live synchronization supports local pulse, avfoundation or alsa capture"
        );
        let controller =
            mpv::Controller::new(mpv::Client::connect(path)?, args.sync.sync_base_delay)?;
        Ok(Some(Self {
            controller,
            options: Options {
                ffmpeg: args.ffmpeg.clone(),
                format: args.format.clone(),
                input: input.to_owned(),
                max_lag_ms: args.max_lag_ms,
            },
        }))
    }

    /// Start only after Sonos accepted playback; a failed Play never records.
    pub fn start(self, stream: Arc<AtomicBool>) -> anyhow::Result<Session> {
        let session = Arc::new(AtomicBool::new(false));
        let stop = Stop {
            session: Arc::clone(&session),
            stream,
        };
        let worker = thread::Builder::new()
            .name("fsonos-audio-sync".into())
            .spawn(move || {
                let Self {
                    mut controller,
                    options,
                } = self;
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    run_loop(&mut controller, &options, &stop)
                }))
                .unwrap_or_else(|_| Err(anyhow::anyhow!("audio sync worker panicked")));
                let restored = controller.restore();
                if let Err(error) = &result {
                    eprintln!("fsonos sync: stopped correcting: {error:#}");
                }
                if let Err(error) = &restored {
                    eprintln!("fsonos sync: {error:#}");
                }
                result.and(restored)
            })?;
        Ok(Session {
            stop: session,
            worker: Some(worker),
        })
    }
}

pub struct Session {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<anyhow::Result<()>>>,
}

impl Session {
    pub fn shutdown(&mut self) -> anyhow::Result<()> {
        self.stop.store(true, Ordering::Release);
        if let Some(worker) = self.worker.take() {
            worker
                .join()
                .map_err(|_| anyhow::anyhow!("audio sync worker panicked"))??;
        }
        Ok(())
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown() {
            eprintln!("fsonos sync: {error:#}");
        }
    }
}

/// Own the worker until streaming stops, including encoder failure and Ctrl-C.
pub fn wait(
    stream: &mut super::broadcast::LiveStream,
    prepared: Option<Prepared>,
    stop: Arc<AtomicBool>,
) -> anyhow::Result<()> {
    let mut session = prepared.map(|prepared| prepared.start(stop)).transpose()?;
    let result = stream.wait_until_stopped().map_err(anyhow::Error::from);
    let stopped = session.as_mut().map(Session::shutdown).transpose();
    result?;
    stopped?;
    Ok(())
}

#[derive(Default)]
struct StableLag {
    samples: Vec<f64>,
}

impl StableLag {
    fn clear(&mut self) {
        self.samples.clear();
    }

    fn observe(&mut self, seconds: f64) -> Option<f64> {
        if !seconds.is_finite() || !(0.02..14.0).contains(&seconds) {
            self.clear();
            return None;
        }
        self.samples.push(seconds);
        if self.samples.len() > 3 {
            self.samples.remove(0);
        }
        if self.samples.len() < 3 {
            return None;
        }
        let mut sorted = [self.samples[0], self.samples[1], self.samples[2]];
        sorted.sort_by(f64::total_cmp);
        (sorted[2] - sorted[0] <= 0.04).then_some(sorted[1])
    }
}

fn run_loop(
    controller: &mut mpv::Controller,
    options: &Options,
    stop: &Stop,
) -> anyhow::Result<()> {
    let mut stable = StableLag::default();
    let mut last_rejection = None;
    while !stop.requested() {
        if !controller.ready()? {
            stable.clear();
            thread::sleep(Duration::from_millis(500));
            continue;
        }
        let Some(bytes) = capture::record(options, stop)? else {
            break;
        };
        let (reference, microphone) =
            calibration::decode_pcm(&bytes).map_err(anyhow::Error::msg)?;
        let max_lag = usize::try_from(options.max_lag_ms)? * (latency::SAMPLE_RATE / 1_000);
        let measured = match latency::estimate(&reference, &microphone, max_lag) {
            Ok(measured) => {
                last_rejection = None;
                measured
            }
            Err(reason) => {
                stable.clear();
                if last_rejection != Some(reason) {
                    eprintln!("fsonos sync: holding the last delay: {reason}");
                    last_rejection = Some(reason);
                }
                continue;
            }
        };
        if stop.requested() {
            break;
        }
        if let Some(lag) = stable.observe(measured.seconds())
            && controller.apply(lag)?
        {
            eprintln!("fsonos sync: acoustic lag {lag:.3} s; mpv correction confirmed");
            // Fresh agreement after a correction; do not reuse pre-change windows.
            stable.clear();
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requires_three_consistent_samples_and_uses_the_median() {
        let mut stable = StableLag::default();
        assert_eq!(stable.observe(3.0), None);
        assert_eq!(stable.observe(3.02), None);
        assert!((stable.observe(3.01).unwrap() - 3.01).abs() < 1e-9);
    }

    #[test]
    fn outliers_silence_and_invalid_samples_do_not_reuse_stale_confidence() {
        let mut stable = StableLag::default();
        for sample in [3.0, 8.0, 3.01, 3.0] {
            assert!(stable.observe(sample).is_none());
        }
        assert!(stable.observe(3.02).is_some());
        stable.clear();
        assert!(stable.observe(3.0).is_none());
        assert!(stable.observe(f64::NAN).is_none());
        assert!(stable.observe(3.0).is_none());
        assert!(stable.observe(3.01).is_none());
        assert!(stable.observe(3.02).is_some());
    }
}
