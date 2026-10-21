//! Explicit, finite test noise for measurement on a named Pulse sink.
//! The reference remains a real monitor capture, never a synthetic timeline.

use std::process::{Child, Command, Stdio};

use anyhow::{Context as _, ensure};

use super::StreamArgs;

const SIGNAL: &str = include_str!("probe.lavfi");
const MAX_WINDOWS: u8 = 3;

pub(super) struct Config {
    sink: String,
    remaining: u8,
}

impl Config {
    /// Validate before any microphone, mpv connection or speaker mutation.
    pub(super) fn from_args(args: &StreamArgs) -> anyhow::Result<Option<Self>> {
        let Some(sink) = args.sync.measure_probe.as_deref() else {
            return Ok(None);
        };
        ensure!(
            cfg!(target_os = "linux") && args.format == "pulse",
            "--measure-probe requires Linux --format pulse"
        );
        ensure!(
            args.sync.measure_latency
                && args.sync.sync_microphone.is_some()
                && args.sync.sync_input.is_none()
                && args.sync.sync_mpv.is_none()
                && args.latency_wav.is_none(),
            "--measure-probe requires measurement-only with --sync-microphone"
        );
        Self::named(sink, &args.input).map(Some)
    }

    fn named(sink: &str, reference: &str) -> anyhow::Result<Self> {
        ensure!(
            !sink.is_empty()
                && sink.len() <= 480
                && sink != "default"
                && !sink.ends_with(".monitor")
                && sink.bytes().any(|byte| byte.is_ascii_alphabetic())
                && sink
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
                && reference == format!("{sink}.monitor"),
            "--measure-probe must name the sink whose exact <sink>.monitor is --input; defaults, indices and other sinks are refused"
        );
        Ok(Self {
            sink: sink.to_owned(),
            remaining: MAX_WINDOWS,
        })
    }

    /// An inconclusive probe cannot become an endless audible retry loop.
    pub(super) fn reserve_window(&mut self) -> anyhow::Result<()> {
        ensure!(
            self.remaining > 0,
            "calibration probe exhausted three windows without agreement; check routing and microphone placement"
        );
        self.remaining -= 1;
        Ok(())
    }

    fn arguments(&self, seconds: u32) -> anyhow::Result<Vec<String>> {
        ensure!((12..=30).contains(&seconds), "invalid probe duration");
        let signal = format!("{},afade=t=out:st={}:d=1", SIGNAL.trim(), seconds - 1);
        Ok(vec![
            "-hide_banner".into(),
            "-loglevel".into(),
            "error".into(),
            "-nostdin".into(),
            "-re".into(),
            "-f".into(),
            "lavfi".into(),
            "-i".into(),
            signal,
            "-t".into(),
            seconds.to_string(),
            "-vn".into(),
            "-ac".into(),
            "2".into(),
            "-ar".into(),
            "48000".into(),
            "-c:a".into(),
            "pcm_s16le".into(),
            "-f".into(),
            "pulse".into(),
            "-device".into(),
            self.sink.clone(),
            "-buffer_duration".into(),
            "50".into(),
            "FrankenSonos calibration probe".into(),
        ])
    }
}

/// Kept inside the capture guard: stop audible output before stopping capture.
/// No reconnect, default sink fallback, volume change or persistent module.
pub(super) struct Emission {
    child: Child,
}

impl Emission {
    pub(super) fn start(ffmpeg: &str, config: &Config, seconds: u32) -> anyhow::Result<Self> {
        let arguments = config.arguments(seconds)?;
        eprintln!(
            "fsonos measure: emitting bounded test noise into {}; local outputs may also be audible",
            config.sink
        );
        let child = Command::new(ffmpeg)
            .args(arguments)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::inherit())
            .spawn()
            .context("start the explicitly requested Pulse calibration probe")?;
        Ok(Self { child })
    }

    pub(super) fn check(&mut self) -> anyhow::Result<()> {
        if let Some(status) = self.child.try_wait()? {
            ensure!(status.success(), "calibration probe process failed");
        }
        Ok(())
    }
}

impl Drop for Emission {
    fn drop(&mut self) {
        // Direct FFmpeg child, not a shell. Every return and unwind reaps it.
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::Parser;

    #[derive(Parser)]
    struct Cli {
        #[command(flatten)]
        args: StreamArgs,
    }

    #[test]
    fn ordinary_streaming_and_passive_measurement_have_no_probe() {
        for flags in [
            vec!["stream", "Room"],
            vec!["stream", "Room", "--measure-latency", "--sync-input", "paired"],
        ] {
            let cli = Cli::try_parse_from(flags).unwrap();
            assert!(Config::from_args(&cli.args).unwrap().is_none());
        }
    }

    #[test]
    fn probe_requires_measurement_and_an_explicit_microphone() {
        for flags in [
            vec!["--measure-probe", "movie"],
            vec![
                "--measure-latency",
                "--sync-input",
                "paired",
                "--measure-probe",
                "movie",
            ],
            vec![
                "--sync-mpv",
                "/tmp/test-mpv",
                "--sync-microphone",
                "mic",
                "--measure-probe",
                "movie",
            ],
            vec!["--latency-wav", "aligned.wav", "--measure-probe", "movie"],
        ] {
            let mut argv = vec!["stream", "Room"];
            argv.extend(flags);
            assert!(Cli::try_parse_from(argv).is_err());
        }
    }

    #[test]
    fn probe_preflight_checks_platform_and_input_without_launching() {
        for format in ["pulse", "alsa", "avfoundation"] {
            let cli = Cli::try_parse_from([
                "stream",
                "Room",
                "--format",
                format,
                "--input",
                "movie.monitor",
                "--measure-latency",
                "--sync-microphone",
                "room_mic",
                "--measure-probe",
                "movie",
                "--ffmpeg",
                "/not-launched-in-preflight",
            ])
            .unwrap();
            let prepared = super::super::Prepared::from_args(&cli.args);
            assert_eq!(
                prepared.is_ok(),
                cfg!(target_os = "linux") && format == "pulse"
            );
        }
    }

    #[test]
    fn probe_never_falls_back_to_defaults_or_another_sink() {
        for sink in [
            "",
            "default",
            "@DEFAULT_SINK@",
            "12",
            "movie.monitor",
            "movie; command",
            "movie\0",
        ] {
            assert!(Config::named(sink, &format!("{sink}.monitor")).is_err());
        }
        assert!(Config::named("movie", "other.monitor").is_err());
        assert!(Config::named("movie", "@DEFAULT_MONITOR@").is_err());
        assert!(Config::named("movie", "movie.monitor").is_ok());
    }

    #[test]
    fn audible_window_budget_cannot_be_refilled() {
        let mut config = Config::named("movie", "movie.monitor").unwrap();
        for _ in 0..MAX_WINDOWS {
            config.reserve_window().unwrap();
        }
        for _ in 0..2 {
            assert!(config.reserve_window().is_err());
        }
    }

    #[test]
    fn probe_uses_a_bounded_low_level_signal_and_one_fixed_output() {
        let config = Config::named("movie", "movie.monitor").unwrap();
        for seconds in [12, 20, 30] {
            let args = config.arguments(seconds).unwrap();
            assert_eq!(args.iter().filter(|arg| *arg == "-i").count(), 1);
            assert!(args.iter().any(|arg| arg == "-re"));
            for (key, value) in [
                ("-device", "movie"),
                ("-buffer_duration", "50"),
                ("-f", "pulse"),
            ] {
                assert!(
                    args.windows(2)
                        .any(|pair| pair[0] == key && pair[1] == value)
                );
            }
            let duration = seconds.to_string();
            assert!(
                args.windows(2)
                    .any(|pair| pair[0] == "-t" && pair[1] == duration)
            );
            let graph = &args[args.iter().position(|arg| arg == "-i").unwrap() + 1];
            assert!(graph.contains("clip(val(0),-0.025,0.025)"));
            assert!(graph.contains(&format!("afade=t=out:st={}:d=1", seconds - 1)));
            assert!(graph.contains(SIGNAL.trim()));
        }
        for seconds in [0, 11, 31, u32::MAX] {
            assert!(config.arguments(seconds).is_err());
        }
    }

    #[test]
    #[ignore = "requires FFmpeg; generated probe and delayed copy only, no audio devices"]
    fn generated_probe_passes_the_production_capture_and_measurement_path() {
        use super::super::{Options, Stop, capture, input::Input, measurement};
        use std::sync::Arc;
        use std::sync::atomic::AtomicBool;
        use std::time::{Duration, Instant};

        let options = Options {
            ffmpeg: std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".into()),
            input: Input::Paired {
                format: "lavfi".into(),
                source: format!(
                    "{},asplit[dry][wet];[wet]adelay=400[late];\
                     [dry][late]join=inputs=2:channel_layout=stereo:map=0.0-FL|1.0-FR",
                    SIGNAL.trim()
                ),
            },
            max_lag_ms: 1_000,
        };
        let stop = Stop {
            session: Arc::new(AtomicBool::new(false)),
            stream: Arc::new(AtomicBool::new(false)),
            deadline: Some(Instant::now() + Duration::from_secs(60)),
        };
        let measured = measurement::collect(1_000, &stop, || capture::record(&options, &stop))
            .unwrap();
        assert_eq!(measured.windows_read, 3);
        assert!(measured.estimate.lag_samples.abs_diff(3_200) <= 2);
    }
}
