//! Bounded, offline acoustic delay estimation. Both channels must share a clock.
//!
//! GCC-PHAT locates a broadband delay; an independent normalized correlation and
//! competing-peak check reject silence, unrelated audio and periodic ambiguity.
//! No microphone, speaker or player I/O lives here.

// Sample counts are bounded below 2^19, exactly representable as f64.
#![allow(clippy::cast_precision_loss)]

use std::f64::consts::TAU;

pub const SAMPLE_RATE: usize = 8_000;
pub const MAX_SAMPLES: usize = 30 * SAMPLE_RATE;

#[derive(Debug, Clone, Copy)]
pub struct Estimate {
    pub lag_samples: usize,
    pub correlation: f64,
    pub peak_ratio: f64,
}

impl Estimate {
    pub fn seconds(self) -> f64 {
        self.lag_samples as f64 / SAMPLE_RATE as f64
    }
}

#[derive(Clone, Copy, Default)]
struct Complex {
    re: f64,
    im: f64,
}

impl Complex {
    fn multiply(self, other: Self) -> Self {
        Self {
            re: self.re * other.re - self.im * other.im,
            im: self.re * other.im + self.im * other.re,
        }
    }
}

fn fft(values: &mut [Complex], inverse: bool) {
    let len = values.len();
    let mut reversed = 0;
    for index in 1..len {
        let mut bit = len >> 1;
        while reversed & bit != 0 {
            reversed ^= bit;
            bit >>= 1;
        }
        reversed ^= bit;
        if index < reversed {
            values.swap(index, reversed);
        }
    }
    let mut width = 2;
    while width <= len {
        let angle = if inverse { TAU } else { -TAU } / width as f64;
        let step = Complex {
            re: angle.cos(),
            im: angle.sin(),
        };
        for block in values.chunks_exact_mut(width) {
            let (left, right) = block.split_at_mut(width / 2);
            let mut phase = Complex { re: 1.0, im: 0.0 };
            for (even, odd) in left.iter_mut().zip(right) {
                let rotated = odd.multiply(phase);
                *odd = Complex {
                    re: even.re - rotated.re,
                    im: even.im - rotated.im,
                };
                even.re += rotated.re;
                even.im += rotated.im;
                phase = phase.multiply(step);
            }
        }
        width *= 2;
    }
    if inverse {
        for value in values {
            value.re /= len as f64;
            value.im /= len as f64;
        }
    }
}

fn centered(signal: &[f64]) -> Result<Vec<f64>, &'static str> {
    if signal
        .iter()
        .any(|value| !value.is_finite() || value.abs() > 1.0)
    {
        return Err("samples must be finite normalized PCM in -1..1");
    }
    let mean = signal.iter().sum::<f64>() / signal.len() as f64;
    let result: Vec<_> = signal.iter().map(|value| value - mean).collect();
    let power = result.iter().map(|value| value * value).sum::<f64>() / result.len() as f64;
    if power < 1e-6 {
        return Err("not enough audible signal to measure latency");
    }
    Ok(result)
}

/// Measure microphone delay relative to the reference, never a negative delay.
/// The maximum must leave at least half the recording available for comparison.
pub fn estimate(
    reference: &[f64],
    microphone: &[f64],
    max_lag_samples: usize,
) -> Result<Estimate, &'static str> {
    let len = reference.len();
    if len != microphone.len() || !(4 * SAMPLE_RATE..=MAX_SAMPLES).contains(&len) {
        return Err("provide 4-30 seconds of synchronized stereo PCM at 8000 Hz");
    }
    let guard = SAMPLE_RATE / 100;
    if max_lag_samples <= 2 * guard || max_lag_samples > len / 2 {
        return Err("maximum lag must be over 20 ms and at most half the recording");
    }
    let reference = centered(reference)?;
    let microphone = centered(microphone)?;
    let fft_len = (2 * len).next_power_of_two();
    let mut dry = vec![Complex::default(); fft_len];
    let mut wet = vec![Complex::default(); fft_len];
    for (value, sample) in dry.iter_mut().zip(&reference) {
        value.re = *sample;
    }
    for (value, sample) in wet.iter_mut().zip(&microphone) {
        value.re = *sample;
    }
    fft(&mut dry, false);
    fft(&mut wet, false);
    let mut largest_power: f64 = 0.0;
    for (value, original) in wet.iter_mut().zip(&dry) {
        *value = value.multiply(Complex {
            re: original.re,
            im: -original.im,
        });
        largest_power = largest_power.max(value.re.hypot(value.im));
    }
    for value in &mut wet {
        // Do not amplify empty/near-empty spectral bins into a false peak.
        let magnitude = value.re.hypot(value.im).max(largest_power * 1e-6);
        value.re /= magnitude;
        value.im /= magnitude;
    }
    fft(&mut wet, true);
    // Search the entire correlation, including negative and out-of-range lags.
    // Otherwise a strong inadmissible delay could leave a plausible sidelobe.
    let peak = wet
        .iter()
        .enumerate()
        .max_by(|(_, left), (_, right)| left.re.abs().total_cmp(&right.re.abs()))
        .map_or(0, |(index, _)| index);
    if peak <= guard || peak >= max_lag_samples - guard {
        return Err("delay is outside the measurement range or too close to its boundary");
    }
    let best = wet[peak].re.abs();
    let runner_up = wet
        .iter()
        .enumerate()
        .filter(|(index, _)| index.abs_diff(peak) > guard)
        .map(|(_, value)| value.re.abs())
        .fold(0.0, f64::max);
    let peak_ratio = best / runner_up.max(1e-12);
    if peak_ratio < 1.5 {
        return Err("ambiguous correlation peaks; use varied audio rather than a tone");
    }
    let original = &reference[..len - peak];
    let delayed = &microphone[peak..];
    let dry_mean = original.iter().sum::<f64>() / original.len() as f64;
    let wet_mean = delayed.iter().sum::<f64>() / delayed.len() as f64;
    let mut covariance = 0.0;
    let mut dry_power = 0.0;
    let mut wet_power = 0.0;
    for (&dry_sample, &wet_sample) in original.iter().zip(delayed) {
        let dry_sample = dry_sample - dry_mean;
        let wet_sample = wet_sample - wet_mean;
        covariance += dry_sample * wet_sample;
        dry_power += dry_sample * dry_sample;
        wet_power += wet_sample * wet_sample;
    }
    let correlation = covariance.abs() / (dry_power * wet_power).sqrt().max(1e-12);
    if correlation < 0.35 {
        return Err("reference and microphone do not contain enough matching audio");
    }
    Ok(Estimate {
        lag_samples: peak,
        correlation,
        peak_ratio,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn noise(len: usize, mut state: u32) -> Vec<f64> {
        (0..len)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                (f64::from(state) / f64::from(u32::MAX) - 0.5) * 0.8
            })
            .collect()
    }

    fn delayed(reference: &[f64], lag: usize) -> Vec<f64> {
        let mut result = vec![0.0; reference.len()];
        result[lag..].copy_from_slice(&reference[..reference.len() - lag]);
        result
    }

    #[test]
    fn finds_broadband_delay_without_wrapping() {
        let reference = noise(8 * SAMPLE_RATE, 1);
        for lag in [801, 8_123, 23_999] {
            let found = estimate(&reference, &delayed(&reference, lag), 4 * SAMPLE_RATE).unwrap();
            assert_eq!(found.lag_samples, lag);
            assert!(found.correlation > 0.99);
            assert!((found.seconds() - lag as f64 / SAMPLE_RATE as f64).abs() < 1e-9);
        }
    }

    #[test]
    fn tolerates_gain_polarity_dc_noise_and_a_short_echo() {
        let reference = noise(8 * SAMPLE_RATE, 2);
        let lag = 17_123;
        let mut microphone = delayed(&reference, lag);
        let noise = noise(reference.len(), 3);
        for index in (1..microphone.len()).rev() {
            microphone[index] =
                -0.6 * microphone[index] + 0.12 * microphone[index - 1] + 0.05 * noise[index] + 0.1;
        }
        let found = estimate(&reference, &microphone, 4 * SAMPLE_RATE).unwrap();
        assert_eq!(found.lag_samples, lag);
    }

    #[test]
    fn rejects_silence_dc_nonfinite_and_unrelated_audio() {
        let reference = noise(8 * SAMPLE_RATE, 7);
        for microphone in [
            vec![0.0; reference.len()],
            vec![0.2; reference.len()],
            noise(reference.len(), 8),
        ] {
            assert!(estimate(&reference, &microphone, 4 * SAMPLE_RATE).is_err());
        }
        let mut microphone = reference.clone();
        microphone[3] = f64::NAN;
        assert!(estimate(&reference, &microphone, 4 * SAMPLE_RATE).is_err());
    }

    #[test]
    fn rejects_negative_out_of_range_and_boundary_delays() {
        let reference = noise(8 * SAMPLE_RATE, 5);
        for lag in [0, 10, 4 * SAMPLE_RATE, 5 * SAMPLE_RATE] {
            assert!(estimate(&reference, &delayed(&reference, lag), 4 * SAMPLE_RATE).is_err());
        }
        assert!(estimate(&delayed(&reference, 1_234), &reference, 4 * SAMPLE_RATE).is_err());
    }

    #[test]
    fn rejects_periodic_ambiguity_and_bad_lengths() {
        let reference: Vec<_> = (0..8 * SAMPLE_RATE)
            .map(|index| (TAU * 400.0 * index as f64 / SAMPLE_RATE as f64).sin() * 0.5)
            .collect();
        assert!(estimate(&reference, &delayed(&reference, 8_000), 4 * SAMPLE_RATE).is_err());
        assert!(estimate(&[], &[], 100).is_err());
        assert!(estimate(&reference, &reference[..20], 100).is_err());
        assert!(estimate(&reference, &reference, reference.len()).is_err());
    }
}
