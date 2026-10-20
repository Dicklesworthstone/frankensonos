//! Capture selection and timestamp-preserving pairing, without audio-server writes.

use anyhow::{bail, ensure};

use super::StreamArgs;

// Pulse supplies latency-adjusted wall-clock timestamps. Keep their relative
// origin with -isync; independently resetting each input to zero invents lag.
// Resample on that common timeline before assigning reference=left, mic=right.
const PULSE_PAIR_FILTER: &str = include_str!("pulse_pair.filter");

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Input {
    Paired {
        format: String,
        source: String,
    },
    PulsePair {
        reference: String,
        microphone: String,
    },
}

impl Input {
    /// Validation precedes mpv connection, speaker changes and microphone I/O.
    pub(super) fn from_args(args: &StreamArgs) -> anyhow::Result<Option<Self>> {
        let paired = args.sync.sync_input.as_deref();
        let microphone = args.sync.sync_microphone.as_deref();
        if paired.is_none() && microphone.is_none() {
            ensure!(
                args.sync.sync_mpv.is_none(),
                "--sync-mpv requires --sync-input or --sync-microphone"
            );
            return Ok(None);
        }
        ensure!(args.sync.sync_mpv.is_some(), "capture requires --sync-mpv");
        ensure!(
            args.latency_wav.is_none(),
            "live capture conflicts with --latency-wav"
        );
        match (paired, microphone) {
            (Some(source), None) => {
                ensure!(
                    !source.trim().is_empty() && !source.contains('\0'),
                    "--sync-input must name a paired local capture source"
                );
                ensure!(
                    matches!(args.format.as_str(), "pulse" | "avfoundation" | "alsa"),
                    "live synchronization supports local pulse, avfoundation or alsa capture"
                );
                Ok(Some(Self::Paired {
                    format: args.format.clone(),
                    source: source.to_owned(),
                }))
            }
            (None, Some(microphone)) => {
                ensure!(
                    cfg!(target_os = "linux") && args.format == "pulse",
                    "--sync-microphone requires Linux --format pulse; use --sync-input for a pre-paired source elsewhere"
                );
                Self::pulse_pair(&args.input, microphone).map(Some)
            }
            _ => bail!("choose only one of --sync-input and --sync-microphone"),
        }
    }

    fn pulse_pair(reference: &str, microphone: &str) -> anyhow::Result<Self> {
        // Defaults can move independently between the encoder and capture.
        // Numeric indices can be recycled after a source is unplugged.
        ensure!(
            named_source(reference)
                && reference
                    .strip_suffix(".monitor")
                    .is_some_and(|sink| !sink.is_empty()),
            "pairing requires --input to name the exact .monitor source sent to Sonos (not @DEFAULT_MONITOR@)"
        );
        ensure!(
            named_source(microphone)
                && !microphone.ends_with(".monitor")
                && microphone != reference,
            "--sync-microphone must name a distinct microphone source, not a default, index or monitor"
        );
        Ok(Self::PulsePair {
            reference: reference.to_owned(),
            microphone: microphone.to_owned(),
        })
    }

    pub(super) fn arguments(&self) -> Vec<String> {
        match self {
            Self::Paired { format, source } => {
                vec!["-f".into(), format.clone(), "-i".into(), source.clone()]
            }
            Self::PulsePair {
                reference,
                microphone,
            } => {
                let mut args = vec!["-copyts".into(), "-start_at_zero".into()];
                for (index, source) in [reference, microphone].into_iter().enumerate() {
                    if index == 1 {
                        args.extend(["-isync".into(), "0".into()]);
                    }
                    args.extend([
                        "-thread_queue_size".into(),
                        "64".into(),
                        "-f".into(),
                        "pulse".into(),
                        "-wallclock".into(),
                        "1".into(),
                        "-sample_rate".into(),
                        "48000".into(),
                        "-channels".into(),
                        "2".into(),
                        "-i".into(),
                        source.clone(),
                    ]);
                }
                args.extend([
                    "-filter_complex".into(),
                    PULSE_PAIR_FILTER.trim().into(),
                    "-map".into(),
                    "[paired]".into(),
                ]);
                args
            }
        }
    }
}

fn named_source(source: &str) -> bool {
    !source.is_empty()
        && source.len() <= 512
        && source != "default"
        && source.bytes().any(|byte| byte.is_ascii_alphabetic())
        && source
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
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
    fn ordinary_streaming_has_no_measurement_input() {
        let cli = Cli::try_parse_from(["stream", "Room"]).unwrap();
        assert!(Input::from_args(&cli.args).unwrap().is_none());
    }

    #[test]
    fn microphone_and_paired_capture_are_exclusive_opt_ins() {
        for flags in [
            vec!["--sync-mpv", "/tmp/test-mpv"],
            vec!["--sync-microphone", "mic"],
            vec!["--sync-input", "paired"],
            vec![
                "--sync-mpv",
                "/tmp/test-mpv",
                "--sync-input",
                "paired",
                "--sync-microphone",
                "mic",
            ],
            vec![
                "--sync-mpv",
                "/tmp/test-mpv",
                "--sync-microphone",
                "mic",
                "--latency-wav",
                "aligned.wav",
            ],
        ] {
            let mut argv = vec!["stream", "Room"];
            argv.extend(flags);
            assert!(Cli::try_parse_from(argv).is_err());
        }
    }

    #[test]
    fn existing_paired_sources_keep_their_single_input_contract() {
        for format in ["pulse", "alsa", "avfoundation"] {
            let cli = Cli::try_parse_from([
                "stream",
                "Room",
                "--sync-mpv",
                "/tmp/test-mpv",
                "--sync-input",
                "paired source; not a shell",
                "--format",
                format,
            ])
            .unwrap();
            let input = Input::from_args(&cli.args).unwrap().unwrap();
            assert_eq!(
                input.arguments(),
                ["-f", format, "-i", "paired source; not a shell"]
            );
        }
    }

    #[test]
    fn automatic_pairing_is_explicit_and_linux_pulse_only() {
        for format in ["pulse", "alsa", "avfoundation"] {
            let cli = Cli::try_parse_from([
                "stream",
                "Room",
                "--sync-mpv",
                "/tmp/test-mpv",
                "--input",
                "movie.monitor",
                "--sync-microphone",
                "room_mic",
                "--format",
                format,
            ])
            .unwrap();
            assert_eq!(
                Input::from_args(&cli.args).is_ok(),
                cfg!(target_os = "linux") && format == "pulse"
            );
        }
    }

    #[test]
    fn mutable_defaults_indices_and_self_reference_are_refused() {
        for reference in ["", "default", "@DEFAULT_MONITOR@", "12", ".monitor", "microphone"] {
            assert!(Input::pulse_pair(reference, "room_mic").is_err());
        }
        for microphone in [
            "",
            "default",
            "@DEFAULT_SOURCE@",
            "12",
            "movie.monitor",
            "other.monitor",
            "mic\0",
            "mic; command",
        ] {
            assert!(Input::pulse_pair("movie.monitor", microphone).is_err());
        }
        assert!(Input::pulse_pair("movie.monitor", "alsa_input.usb-example.analog-stereo").is_ok());
    }

    #[test]
    fn pairing_uses_native_timestamps_and_explicit_channel_mapping() {
        let args = Input::pulse_pair("movie.monitor", "room_mic")
            .unwrap()
            .arguments();
        let inputs: Vec<_> = args
            .windows(2)
            .filter(|pair| pair[0] == "-i")
            .map(|pair| pair[1].as_str())
            .collect();
        assert_eq!(inputs, ["movie.monitor", "room_mic"]);
        assert_eq!(&args[..2], ["-copyts", "-start_at_zero"]);
        let sync = args.iter().position(|arg| arg == "-isync").unwrap();
        assert_eq!(&args[sync..sync + 2], ["-isync", "0"]);
        assert_eq!(args.iter().filter(|arg| *arg == "-wallclock").count(), 2);
        assert_eq!(args.iter().filter(|arg| *arg == "-thread_queue_size").count(), 2);
        assert!(args.iter().all(|arg| arg != "-use_wallclock_as_timestamps"));
        assert!(PULSE_PAIR_FILTER.contains("map=0.0-FL|1.0-FR"));
        assert!(!PULSE_PAIR_FILTER.contains("PTS-STARTPTS"));
        assert_eq!(&args[args.len() - 2..], ["-map", "[paired]"]);
    }
}
