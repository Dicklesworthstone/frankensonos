//! One-shot acoustic measurement. No player commands or microphone files here.

use anyhow::{Context as _, bail, ensure};
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::{Stop, calibration, latency};
use latency::{Estimate, SAMPLE_RATE};

const AGREEMENT_SAMPLES: usize = SAMPLE_RATE * 40 / 1_000;
const MAX_WINDOWS: u32 = 64;

#[derive(Debug, Clone, Copy)]
pub struct Measurement {
    /// Median lag; confidence fields are the minimum across the three windows.
    pub estimate: Estimate,
    pub windows_read: u32,
    pub spread_samples: usize,
}

impl Measurement {
    /// Emit only after the caller has stopped capture and attempted restoration.
    pub fn emit(self, json: bool, room: &str, playback_restored: bool) -> anyhow::Result<()> {
        crate::emit(
            json,
            &serde_json::json!({
                "room": room,
                "mode": "live",
                "lag_seconds": self.estimate.seconds(),
                "lag_samples": self.estimate.lag_samples,
                "sample_rate": SAMPLE_RATE,
                "correlation_min": self.estimate.correlation,
                "peak_ratio_min": self.estimate.peak_ratio,
                "agreeing_windows": 3,
                "windows_read": self.windows_read,
                "spread_samples": self.spread_samples,
                "mpv_audio_delay_delta": -self.estimate.seconds(),
                "mpv_modified": false,
                "playback_restored": playback_restored,
            }),
            |_| {
                let restored = if playback_restored {
                    "Previous playback restored."
                } else {
                    "Previous playback not fully restored; see the cleanup warnings."
                };
                format!(
                    "Measured acoustic lag: {:.3} s across three agreeing windows.\n\
                     Suggested audio-delay correction: -{:.3} s; no video player changed.\n\
                     {restored}\n",
                    self.estimate.seconds(),
                    self.estimate.seconds(),
                )
            },
        )
    }
}

#[derive(Default)]
struct Agreement {
    samples: Vec<Estimate>,
}

impl Agreement {
    fn clear(&mut self) {
        self.samples.clear();
    }

    fn observe(&mut self, estimate: Estimate, windows_read: u32) -> Option<Measurement> {
        self.samples.push(estimate);
        if self.samples.len() > 3 {
            self.samples.remove(0);
        }
        let [first, second, third] = self.samples.as_slice() else {
            return None;
        };
        let mut sorted = [*first, *second, *third];
        sorted.sort_unstable_by_key(|sample| sample.lag_samples);
        let spread_samples = sorted[2].lag_samples - sorted[0].lag_samples;
        if spread_samples > AGREEMENT_SAMPLES {
            return None;
        }
        let mut estimate = sorted[1];
        estimate.correlation = sorted.iter().map(|s| s.correlation).fold(1.0, f64::min);
        estimate.peak_ratio = sorted.iter().map(|s| s.peak_ratio).fold(f64::MAX, f64::min);
        Some(Measurement {
            estimate,
            windows_read,
            spread_samples,
        })
    }
}

fn check(stop: &Stop) -> anyhow::Result<()> {
    ensure!(
        !stop.session.load(Ordering::Acquire) && !stop.stream.load(Ordering::Acquire),
        "live latency measurement cancelled; no estimate reported"
    );
    ensure!(
        stop.deadline.is_none_or(|at| Instant::now() < at),
        "live latency measurement deadline exceeded before three agreeing windows"
    );
    Ok(())
}

/// `record` also checks the live encoder and the intended Sonos source before
/// and after each capture. Its error is terminal, not another noisy window.
/// The shared deadline cancels an in-flight PCM capture through `Stop`.
pub(super) fn collect(
    max_lag_ms: u32,
    stop: &Stop,
    mut record: impl FnMut() -> anyhow::Result<Option<Vec<u8>>>,
) -> anyhow::Result<Measurement> {
    ensure!(
        (21..=14_000).contains(&max_lag_ms),
        "invalid maximum acoustic lag"
    );
    let max_lag = usize::try_from(max_lag_ms)? * (SAMPLE_RATE / 1_000);
    let mut agreement = Agreement::default();
    let mut last_rejection = None;
    for windows_read in 1..=MAX_WINDOWS {
        check(stop)?;
        let bytes = record()?;
        check(stop)?;
        let bytes = bytes.context("capture ended without a complete measurement window")?;
        let (reference, microphone) =
            calibration::decode_pcm(&bytes).map_err(anyhow::Error::msg)?;
        let estimate = latency::estimate(&reference, &microphone, max_lag);
        // A capture or computation finishing after cancellation is not a result.
        check(stop)?;
        match estimate {
            Ok(estimate) => {
                last_rejection = None;
                if let Some(measured) = agreement.observe(estimate, windows_read) {
                    return Ok(measured);
                }
            }
            Err(reason) => {
                agreement.clear();
                if last_rejection != Some(reason) {
                    eprintln!("fsonos measure: waiting for matching audio: {reason}");
                    last_rejection = Some(reason);
                }
            }
        }
    }
    bail!("live latency measurement exhausted its window budget without agreement")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    fn stop() -> Stop {
        Stop {
            session: Arc::new(AtomicBool::new(false)),
            stream: Arc::new(AtomicBool::new(false)),
            deadline: Some(Instant::now() + Duration::from_secs(60)),
        }
    }

    fn sample(lag_samples: usize, correlation: f64, peak_ratio: f64) -> Estimate {
        Estimate {
            lag_samples,
            correlation,
            peak_ratio,
        }
    }

    #[test]
    fn three_windows_report_median_and_conservative_quality() {
        let mut agreement = Agreement::default();
        assert!(agreement.observe(sample(24_000, 0.9, 3.0), 1).is_none());
        assert!(agreement.observe(sample(24_320, 0.8, 5.0), 2).is_none());
        let measured = agreement.observe(sample(24_080, 0.7, 4.0), 3).unwrap();
        assert_eq!(measured.estimate.lag_samples, 24_080);
        assert_eq!(measured.spread_samples, 320);
        assert!((measured.estimate.correlation - 0.7).abs() < 1e-9);
        assert!((measured.estimate.peak_ratio - 3.0).abs() < 1e-9);
    }

    #[test]
    fn outliers_and_rejections_require_new_agreement() {
        let mut agreement = Agreement::default();
        for (index, lag) in [24_000, 40_000, 24_100, 24_200].into_iter().enumerate() {
            assert!(
                agreement
                    .observe(sample(lag, 0.9, 3.0), u32::try_from(index).unwrap())
                    .is_none()
            );
        }
        assert!(agreement.observe(sample(24_000, 0.9, 3.0), 5).is_some());
        agreement.clear();
        for index in 6..8 {
            assert!(agreement.observe(sample(24_000, 0.9, 3.0), index).is_none());
        }
        assert!(agreement.observe(sample(24_000, 0.9, 3.0), 8).is_some());
    }

    #[test]
    fn cancellation_and_deadline_prevent_capture() {
        let mut stop = stop();
        stop.stream.store(true, Ordering::Release);
        assert!(collect(1_000, &stop, || panic!("must not capture")).is_err());
        stop.stream.store(false, Ordering::Release);
        stop.deadline = Some(Instant::now());
        assert!(collect(1_000, &stop, || panic!("must not capture")).is_err());
    }

    #[test]
    fn late_cancellation_and_source_errors_do_not_become_estimates() {
        let stop = stop();
        let error = collect(1_000, &stop, || {
            stop.stream.store(true, Ordering::Release);
            Ok(Some(vec![0; 4 * SAMPLE_RATE * 4]))
        })
        .unwrap_err();
        assert!(error.to_string().contains("cancelled"));
        stop.stream.store(false, Ordering::Release);
        let mut calls = 0;
        let error = collect(1_000, &stop, || {
            calls += 1;
            bail!("owner selected another source")
        })
        .unwrap_err();
        assert_eq!(calls, 1);
        assert!(error.to_string().contains("owner selected"));
    }

    #[test]
    fn truncated_pcm_is_not_retried_as_silence() {
        let stop = stop();
        let mut calls = 0;
        assert!(
            collect(1_000, &stop, || {
                calls += 1;
                Ok(Some(vec![0; 7]))
            })
            .is_err()
        );
        assert_eq!(calls, 1);
    }
}

#[cfg(test)]
#[path = "measurement/signal_tests.rs"]
mod signal_tests;
