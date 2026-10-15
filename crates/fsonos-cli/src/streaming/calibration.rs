//! Read an explicitly supplied, time-aligned reference/microphone recording.
//! Recordings never leave this process and no device is contacted.

use std::fs::File;
use std::io::Read as _;
use std::path::Path;

use super::latency::{self, Estimate, MAX_SAMPLES, SAMPLE_RATE};

const MAX_WAVE_BYTES: usize = MAX_SAMPLES * 4 + 65_536;

fn number(bytes: &[u8]) -> u32 {
    u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
}

pub fn decode_wave(bytes: &[u8]) -> Result<(Vec<f64>, Vec<f64>), &'static str> {
    if bytes.len() < 12
        || bytes.len() > MAX_WAVE_BYTES
        || &bytes[..4] != b"RIFF"
        || &bytes[8..12] != b"WAVE"
    {
        return Err("expected a bounded RIFF/WAVE recording");
    }
    let declared = usize::try_from(number(&bytes[4..8])).map_err(|_| "WAV is too large")?;
    if declared.checked_add(8) != Some(bytes.len()) {
        return Err("WAV length does not match its RIFF header");
    }
    let mut offset = 12_usize;
    let mut format = None;
    let mut samples = None;
    while offset < bytes.len() {
        let header_end = offset.checked_add(8).ok_or("WAV chunk overflow")?;
        let header = bytes
            .get(offset..header_end)
            .ok_or("truncated WAV chunk header")?;
        let size = usize::try_from(number(&header[4..8])).map_err(|_| "WAV chunk is too large")?;
        let end = header_end.checked_add(size).ok_or("WAV chunk overflow")?;
        let chunk = bytes.get(header_end..end).ok_or("truncated WAV chunk")?;
        // Each chunk is taken as it comes; a second one is an error.
        match &header[..4] {
            b"fmt " if format.replace(chunk).is_some() => return Err("duplicate WAV format"),
            b"data" if samples.replace(chunk).is_some() => {
                return Err("duplicate WAV audio data");
            }
            _ => {}
        }
        offset = end.checked_add(size % 2).ok_or("WAV padding overflow")?;
        if offset > bytes.len() {
            return Err("missing WAV chunk padding");
        }
    }
    let format = format.ok_or("missing WAV format")?;
    if format.len() < 16
        || &format[..4] != b"\x01\x00\x02\x00"
        || number(&format[4..8]) != 8_000
        || number(&format[8..12]) != 32_000
        || &format[12..16] != b"\x04\x00\x10\x00"
    {
        return Err("use stereo 16-bit PCM WAV at 8000 Hz: left=reference, right=microphone");
    }
    decode_pcm(samples.ok_or("missing WAV audio data")?)
}

/// A complete interleaved s16le window; left is dry audio, right is microphone.
pub fn decode_pcm(bytes: &[u8]) -> Result<(Vec<f64>, Vec<f64>), &'static str> {
    if !bytes.len().is_multiple_of(4)
        || !(4 * SAMPLE_RATE..=MAX_SAMPLES).contains(&(bytes.len() / 4))
    {
        return Err("provide 4-30 seconds of complete stereo PCM frames");
    }
    let mut reference = Vec::with_capacity(bytes.len() / 4);
    let mut microphone = Vec::with_capacity(bytes.len() / 4);
    for frame in bytes.as_chunks::<4>().0 {
        reference.push(f64::from(i16::from_le_bytes([frame[0], frame[1]])) / 32_768.0);
        microphone.push(f64::from(i16::from_le_bytes([frame[2], frame[3]])) / 32_768.0);
    }
    Ok((reference, microphone))
}

pub fn measure_file(path: &Path, max_lag_ms: u32) -> anyhow::Result<Estimate> {
    let mut bytes = Vec::new();
    File::open(path)?
        .take((MAX_WAVE_BYTES + 1) as u64)
        .read_to_end(&mut bytes)?;
    let (reference, microphone) = decode_wave(&bytes).map_err(anyhow::Error::msg)?;
    let max_lag = usize::try_from(max_lag_ms)? * (SAMPLE_RATE / 1_000);
    latency::estimate(&reference, &microphone, max_lag).map_err(anyhow::Error::msg)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wave() -> Vec<u8> {
        let samples = vec![0_u8; 4 * SAMPLE_RATE * 4];
        let mut bytes = b"RIFF".to_vec();
        bytes.extend(u32::try_from(samples.len() + 36).unwrap().to_le_bytes());
        bytes.extend(b"WAVEfmt \x10\x00\x00\x00\x01\x00\x02\x00");
        bytes.extend(8_000_u32.to_le_bytes());
        bytes.extend(32_000_u32.to_le_bytes());
        bytes.extend(b"\x04\x00\x10\x00data");
        bytes.extend(u32::try_from(samples.len()).unwrap().to_le_bytes());
        bytes.extend(samples);
        bytes
    }

    #[test]
    fn decodes_signed_channels_without_mixing_them() {
        let mut bytes = wave();
        bytes[44..48].copy_from_slice(&[0, 128, 255, 127]);
        let (reference, microphone) = decode_wave(&bytes).unwrap();
        assert!((reference[0] + 1.0).abs() < f64::EPSILON);
        assert!((microphone[0] - 32_767.0 / 32_768.0).abs() < f64::EPSILON);
    }

    #[test]
    fn accepts_metadata_and_rejects_duplicate_audio() {
        let mut bytes = wave();
        bytes.extend(b"JUNK\x01\x00\x00\x00x\x00");
        let size = u32::try_from(bytes.len() - 8).unwrap();
        bytes[4..8].copy_from_slice(&size.to_le_bytes());
        assert!(decode_wave(&bytes).is_ok());
        bytes.extend(b"data\x00\x00\x00\x00");
        let size = u32::try_from(bytes.len() - 8).unwrap();
        bytes[4..8].copy_from_slice(&size.to_le_bytes());
        assert!(decode_wave(&bytes).is_err());
    }

    #[test]
    fn rejects_truncation_invalid_formats_and_oversized_chunks() {
        let bytes = wave();
        for length in [0, 1, 8, 11, 12, 20, 36, 43, bytes.len() - 1] {
            assert!(decode_wave(&bytes[..length]).is_err());
        }
        for offset in [20, 22, 24, 28, 32, 34, 40] {
            let mut invalid = bytes.clone();
            invalid[offset] ^= 0xff;
            assert!(decode_wave(&invalid).is_err());
        }
    }
}
