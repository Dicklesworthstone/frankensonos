//! Generated PCM goes through the production decoder, estimator and collector.

use super::*;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

fn stop() -> Stop {
    Stop {
        session: Arc::new(AtomicBool::new(false)),
        stream: Arc::new(AtomicBool::new(false)),
        deadline: None,
    }
}

fn pcm(lag: usize, mut seed: u32) -> Vec<u8> {
    let reference: Vec<i16> = (0..4 * SAMPLE_RATE)
        .map(|_| {
            seed = seed.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            i16::try_from(seed % 20_001).unwrap() - 10_000
        })
        .collect();
    let mut bytes = Vec::with_capacity(reference.len() * 4);
    for (index, value) in reference.iter().enumerate() {
        let delayed = index.checked_sub(lag).map_or(0, |n| reference[n]);
        bytes.extend(value.to_le_bytes());
        bytes.extend(delayed.to_le_bytes());
    }
    bytes
}

#[test]
fn three_independent_audio_windows_finish_with_the_median_lag() {
    let stop = stop();
    let mut windows = [pcm(3_200, 1), pcm(3_210, 2), pcm(3_190, 3)].into_iter();
    let measured = collect(1_000, &stop, || Ok(windows.next())).unwrap();
    assert_eq!(measured.windows_read, 3);
    assert_eq!(measured.estimate.lag_samples, 3_200);
    assert_eq!(measured.spread_samples, 20);
    assert!(measured.estimate.correlation > 0.99);
    assert!(windows.next().is_none());
}

#[test]
fn silence_requires_three_new_confident_audio_windows() {
    let stop = stop();
    let mut windows = [
        pcm(3_200, 10),
        pcm(3_210, 11),
        vec![0; 4 * SAMPLE_RATE * 4],
        pcm(3_190, 12),
        pcm(3_200, 13),
        pcm(3_210, 14),
    ]
    .into_iter();
    let measured = collect(1_000, &stop, || Ok(windows.next())).unwrap();
    assert_eq!(measured.windows_read, 6);
    assert_eq!(measured.estimate.lag_samples, 3_200);
    assert!(windows.next().is_none());
}

#[test]
fn a_confident_but_different_lag_is_not_premature_agreement() {
    let stop = stop();
    let mut windows = [
        pcm(3_200, 21),
        pcm(6_000, 22),
        pcm(3_190, 23),
        pcm(3_200, 24),
        pcm(3_210, 25),
    ]
    .into_iter();
    let measured = collect(1_000, &stop, || Ok(windows.next())).unwrap();
    assert_eq!(measured.windows_read, 5);
    assert_eq!(measured.estimate.lag_samples, 3_200);
}

#[test]
fn a_failed_capture_after_two_good_windows_never_reports_the_partial_result() {
    let stop = stop();
    let mut calls = 0;
    let error = collect(1_000, &stop, || {
        calls += 1;
        if calls == 3 {
            bail!("microphone disconnected");
        }
        Ok(Some(pcm(3_200, calls)))
    })
    .unwrap_err();
    assert_eq!(calls, 3);
    assert!(error.to_string().contains("microphone disconnected"));
}

#[test]
fn persistent_silence_exhausts_the_window_budget_without_an_estimate() {
    let stop = stop();
    let mut calls = 0;
    let error = collect(1_000, &stop, || {
        calls += 1;
        Ok(Some(vec![0; 4 * SAMPLE_RATE * 4]))
    })
    .unwrap_err();
    assert_eq!(calls, MAX_WINDOWS);
    assert!(error.to_string().contains("window budget"));
}

#[test]
fn a_late_valid_capture_cannot_outlive_the_measurement_deadline() {
    let bytes = pcm(3_200, 31);
    let mut stop = stop();
    let deadline = Instant::now() + Duration::from_millis(30);
    stop.deadline = Some(deadline);
    let error = collect(1_000, &stop, || {
        std::thread::sleep(
            deadline.saturating_duration_since(Instant::now()) + Duration::from_millis(1),
        );
        Ok(Some(bytes.clone()))
    })
    .unwrap_err();
    assert!(error.to_string().contains("deadline exceeded"));
}

#[test]
#[ignore = "requires locally installed FFmpeg; uses generated audio, no microphone"]
fn real_ffmpeg_capture_feeds_the_production_measurement_collector() {
    use super::super::{Options, capture, input::Input};
    let options = Options {
        ffmpeg: std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".into()),
        input: Input::Paired {
            format: "lavfi".into(),
            source: concat!(
                "anoisesrc=r=8000:a=0.2:s=12345,asplit[dry][wet];",
                "[wet]adelay=400[late];",
                "[dry][late]join=inputs=2:channel_layout=stereo:map=0.0-FL|1.0-FR"
            )
            .into(),
        },
        max_lag_ms: 1_000,
    };
    let mut stop = stop();
    stop.deadline = Some(Instant::now() + Duration::from_secs(60));
    let measured = collect(1_000, &stop, || capture::record(&options, &stop)).unwrap();
    assert_eq!(measured.windows_read, 3);
    assert!(measured.estimate.lag_samples.abs_diff(3_200) <= 2);
}

#[test]
#[ignore = "requires locally installed FFmpeg; uses generated silence, no microphone"]
fn deadline_interrupts_and_reaps_real_time_ffmpeg_capture() {
    use super::super::{Options, capture, input::Input};
    let options = Options {
        ffmpeg: std::env::var("FFMPEG").unwrap_or_else(|_| "ffmpeg".into()),
        input: Input::Paired {
            format: "lavfi".into(),
            source: "anullsrc=r=8000:cl=stereo,arealtime".into(),
        },
        max_lag_ms: 1_000,
    };
    let mut stop = stop();
    stop.deadline = Some(Instant::now() + Duration::from_millis(100));
    let started = Instant::now();
    assert!(capture::record(&options, &stop).unwrap().is_none());
    assert!(started.elapsed() < Duration::from_secs(5));
}
