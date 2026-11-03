//! Explicit local acoustic measurement and mpv feedback; ordinary streaming never records.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::Context as _;
use clap::Args;

use super::{StreamArgs, calibration, latency};

mod capture;
mod input;
mod measurement;
mod mpv;
mod ownership;
mod probe;
mod startup;

pub use measurement::Measurement;

// Most field names are flags (--sync-mpv, --sync-input, ...).
#[allow(clippy::struct_field_names)]
#[derive(Debug, Clone, Default, Args)]
pub struct SyncArgs {
    /// Opt in to local microphone feedback and correction through this mpv Unix socket.
    #[arg(
        long,
        groups = ["capture_action", "mpv_correction"],
        requires = "sync_capture",
        conflicts_with_all = ["measure_latency", "sync_launched_mpv", "latency_wav"]
    )]
    pub sync_mpv: Option<PathBuf>,
    /// Linux Pulse: synchronize the trailing mpv command using private IPC and --virtual-output.
    #[arg(
        long,
        groups = ["capture_action", "mpv_correction"],
        requires_all = ["sync_capture", "virtual_output", "application"],
        conflicts_with_all = ["sync_mpv", "measure_latency", "latency_wav"]
    )]
    pub sync_launched_mpv: bool,
    /// Measure live acoustic lag, then stop streaming and attempt playback restoration; no mpv writes.
    #[arg(
        long,
        group = "capture_action",
        requires = "sync_capture",
        conflicts_with_all = ["sync_mpv", "sync_launched_mpv", "latency_wav"]
    )]
    pub measure_latency: bool,
    /// Measurement phase deadline in seconds (default 180), excluding playback setup and restoration.
    #[arg(long, requires = "measure_latency", value_parser = clap::value_parser!(u64).range(10..=600))]
    pub measure_timeout: Option<u64>,
    /// Emit bounded low-level test noise into this Pulse sink during measurement (audible).
    #[arg(
        long,
        requires_all = ["measure_latency", "sync_microphone"],
        conflicts_with_all = ["sync_mpv", "sync_input", "latency_wav"]
    )]
    pub measure_probe: Option<String>,
    /// Calibrate mpv once with bounded audible test noise into this Pulse sink, then track passively.
    #[arg(
        long,
        requires_all = ["mpv_correction", "sync_microphone"],
        conflicts_with_all = ["measure_latency", "measure_probe", "sync_input", "latency_wav"]
    )]
    pub sync_probe: Option<String>,
    /// One time-aligned stereo capture: left=dry reference, right=room microphone.
    #[arg(
        long,
        group = "sync_capture",
        requires = "capture_action",
        conflicts_with_all = ["sync_microphone", "latency_wav"]
    )]
    pub sync_input: Option<String>,
    /// Linux pulse: pair this named microphone with the exact .monitor named by --input.
    #[arg(
        long,
        group = "sync_capture",
        requires = "capture_action",
        conflicts_with_all = ["sync_input", "latency_wav"]
    )]
    pub sync_microphone: Option<String>,
    /// Uncorrected media-specific mpv delay in seconds, not the last correction.
    #[arg(
        long,
        default_value_t = 0.0,
        requires = "mpv_correction",
        conflicts_with = "measure_latency",
        allow_negative_numbers = true
    )]
    pub sync_base_delay: f64,
}

struct Options {
    ffmpeg: String,
    input: input::Input,
    max_lag_ms: u32,
}

struct Stop {
    session: Arc<AtomicBool>,
    stream: Arc<AtomicBool>,
    deadline: Option<Instant>,
}

impl Stop {
    fn requested(&self) -> bool {
        self.session.load(Ordering::Acquire)
            || self.stream.load(Ordering::Acquire)
            || self.deadline.is_some_and(|at| Instant::now() >= at)
    }
}

pub struct Prepared {
    controller: Option<mpv::Controller>,
    launched: bool,
    base_delay: f64,
    options: Options,
    measurement_timeout: Duration,
    probe: Option<probe::Config>,
}

impl Prepared {
    /// Read-only preflight before any speaker mutation. Measurement never connects to mpv.
    pub fn from_args(args: &StreamArgs) -> anyhow::Result<Option<Self>> {
        let probe = probe::Config::from_args(args)?;
        let Some(input) = input::Input::from_args(args)? else {
            return Ok(None);
        };
        mpv::validate_base_delay(args.sync.sync_base_delay)?;
        let mut controller = args
            .sync
            .sync_mpv
            .as_deref()
            .map(|path| {
                mpv::Controller::new(mpv::Client::connect(path)?, args.sync.sync_base_delay)
            })
            .transpose()?;
        if probe.is_some()
            && let Some(controller) = &mut controller
        {
            anyhow::ensure!(
                controller.ready()?,
                "start mpv playback before requesting audible startup calibration"
            );
        }
        Ok(Some(Self {
            controller,
            launched: args.sync.sync_launched_mpv,
            base_delay: args.sync.sync_base_delay,
            options: Options {
                ffmpeg: args.ffmpeg.clone(),
                input,
                max_lag_ms: args.max_lag_ms,
            },
            measurement_timeout: Duration::from_secs(args.sync.measure_timeout.unwrap_or(180)),
            probe,
        }))
    }

    /// The managed child starts only after the stream has a live reader. Its
    /// private IPC and actual Pulse output must be ready before capture starts.
    pub(super) fn attach_launched(
        mut self,
        application: Option<&super::application::Application>,
        stop: &Arc<AtomicBool>,
        mut check_stream: impl FnMut() -> anyhow::Result<()>,
    ) -> anyhow::Result<Self> {
        if !self.launched {
            return Ok(self);
        }
        let application = application.context("managed mpv application was not launched")?;
        let (path, output) = application
            .mpv_endpoint()
            .context("managed mpv has no private IPC endpoint")?;
        let client = mpv::Client::connect_launched(
            path,
            application.id(),
            output,
            Arc::clone(stop),
            || {
                check_stream()?;
                application.ensure_running()
            },
        )?;
        let mut controller = mpv::Controller::new(client, self.base_delay)?;
        anyhow::ensure!(controller.ready()?, "launched mpv is not playing");
        check_stream()?;
        application.ensure_running()?;
        controller.finish_launch(output)?;
        self.controller = Some(controller);
        self.launched = false;
        Ok(self)
    }

    /// Start only after Sonos accepted playback; a failed Play never records.
    pub fn start(self, stream: Arc<AtomicBool>) -> anyhow::Result<Session> {
        let mut controller = self
            .controller
            .context("measurement has no mpv controller")?;
        let options = self.options;
        let probe = self.probe;
        let session = Arc::new(AtomicBool::new(false));
        let stop = Stop {
            session: Arc::clone(&session),
            stream,
            deadline: None,
        };
        let (checks, requests) = ownership::channel();
        let worker = thread::Builder::new()
            .name("fsonos-audio-sync".into())
            .spawn(move || {
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    if let Some(probe) = probe {
                        let calibration_stop = Stop {
                            session: Arc::clone(&stop.session),
                            stream: Arc::clone(&stop.stream),
                            deadline: Some(Instant::now() + Duration::from_secs(180)),
                        };
                        startup::run(
                            &mut controller,
                            &options,
                            &calibration_stop,
                            probe,
                            |settled| checks.verify(settled, &calibration_stop),
                        )?;
                    }
                    // The probe is consumed once. Passive drift tracking starts
                    // with fresh agreement and can never emit another probe.
                    run_loop(&mut controller, &options, &stop, |settled| {
                        checks.verify(settled, &stop)
                    })
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
            requests,
        })
    }
}

pub struct Session {
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<anyhow::Result<()>>>,
    requests: ownership::Requests,
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

/// Measurement finishes automatically; ordinary streaming and mpv correction
/// retain their existing Ctrl-C lifetime. Both capture modes verify the original
/// room/source; ordinary streaming does not add speaker observations.
pub fn wait(
    stream: &mut super::broadcast::LiveStream,
    prepared: Option<Prepared>,
    stop: Arc<AtomicBool>,
    mut verify: impl FnMut(bool) -> anyhow::Result<()>,
) -> anyhow::Result<Option<Measurement>> {
    anyhow::ensure!(
        !prepared.as_ref().is_some_and(|prepared| prepared.launched),
        "managed mpv must connect before acoustic capture"
    );
    let prepared = match prepared {
        Some(prepared) if prepared.controller.is_none() => {
            let stop = Stop {
                session: Arc::new(AtomicBool::new(false)),
                stream: stop,
                deadline: Some(Instant::now() + prepared.measurement_timeout),
            };
            let mut probe = prepared.probe;
            return measurement::collect(prepared.options.max_lag_ms, &stop, || {
                stream.check()?;
                verify(false)?;
                if let Some(probe) = &mut probe {
                    probe.reserve_window()?;
                }
                let bytes = capture::record_with_probe(&prepared.options, &stop, probe.as_ref())?;
                stream.check()?;
                if bytes.is_some() && !stop.requested() {
                    verify(true)?;
                }
                Ok(bytes)
            })
            .map(Some);
        }
        other => other,
    };
    let mut session = prepared
        .map(|prepared| prepared.start(Arc::clone(&stop)))
        .transpose()?;
    let result = match &session {
        Some(session) => (|| -> anyhow::Result<()> {
            while !stop.load(Ordering::Acquire) {
                stream.check()?;
                session.requests.service(&mut verify);
                thread::sleep(Duration::from_millis(100));
            }
            Ok(())
        })(),
        None => stream.wait_until_stopped().map_err(anyhow::Error::from),
    };
    let stopped = session.as_mut().map(Session::shutdown).transpose();
    result?;
    stopped?;
    Ok(None)
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
    verify: impl FnMut(bool) -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    run_with_capture(controller, options.max_lag_ms, stop, verify, || {
        capture::record(options, stop)
    })
}

fn run_with_capture(
    controller: &mut mpv::Controller,
    max_lag_ms: u32,
    stop: &Stop,
    mut verify: impl FnMut(bool) -> anyhow::Result<bool>,
    mut record: impl FnMut() -> anyhow::Result<Option<Vec<u8>>>,
) -> anyhow::Result<()> {
    let mut stable = StableLag::default();
    let mut last_rejection = None;
    let mut last_window = None;
    while !stop.requested() {
        let Some(window) = controller.begin_window()? else {
            stable.clear();
            last_window = None;
            thread::sleep(Duration::from_millis(500));
            continue;
        };
        // A seek or pause between otherwise valid windows also breaks agreement.
        if last_window != Some(window) {
            stable.clear();
        }
        if !verify(false)? || stop.requested() {
            break;
        }
        let Some(bytes) = record()? else {
            break;
        };
        if stop.requested() {
            break;
        }
        if !verify(true)? || stop.requested() {
            break;
        }
        if !controller.finish_window(window)? {
            stable.clear();
            last_window = None;
            continue;
        }
        last_window = Some(window);
        let (reference, microphone) =
            calibration::decode_pcm(&bytes).map_err(anyhow::Error::msg)?;
        let max_lag = usize::try_from(max_lag_ms)? * (latency::SAMPLE_RATE / 1_000);
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
        if let Some(lag) = stable.observe(measured.seconds()) {
            // The room may have changed while the estimator was computing.
            if !verify(true)? || stop.requested() {
                break;
            }
            if controller.apply(lag, window)? {
                eprintln!("fsonos sync: acoustic lag {lag:.3} s; mpv correction confirmed");
            }
            // This also discards agreement if playback changed during estimation
            // or final read-back. No prior windows authorize another correction.
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
