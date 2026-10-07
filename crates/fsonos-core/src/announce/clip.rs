//! Announcement clips: speech, chimes, and WAV files, kept in the media
//! directory for the players to fetch.
//!
//! Every clip is a 16-bit PCM WAV. Speech comes from macOS `say`, which
//! renders 44.1 kHz WAV; chimes are synthesized here, so no recorded audio is
//! shipped; a WAV file can be imported from the CLI. Clips live in
//! `<data-dir>/media` under unguessable 128-bit ids for [`DEFAULT_TTL`]. The
//! daemon serves them read-only at `/media/<id>.wav` from its GENA listener,
//! which the players can already reach; [`MediaStore::path`] is the only
//! lookup that serving needs.
//!
//! [`ClipSource`] lists only what a remote client may ask for. Importing a
//! file ([`MediaStore::import_wav`]) is CLI-only: over HTTP or MCP it would
//! let a remote caller make the daemon read any local file and serve it on
//! the LAN.

use std::fmt;
use std::fs;
use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::str::FromStr;
use std::time::{Duration, SystemTime};

/// Sample rate of synthesized chimes, and the rate `say` is asked for.
pub const SAMPLE_RATE: u32 = 44_100;

/// How long a stored clip is kept.
pub const DEFAULT_TTL: Duration = Duration::from_hours(1);

/// The longest text spoken in one announcement.
pub const MAX_SPEECH_CHARS: usize = 1_000;

/// Why a clip could not be made or stored.
#[derive(Debug, thiserror::Error)]
pub enum ClipError {
    #[error("{0}")]
    Unsupported(&'static str),
    #[error("unknown chime {name:?}; choose one of: {}", Chime::NAMES.join(", "))]
    UnknownChime { name: String },
    #[error("nothing to say")]
    EmptySpeech,
    #[error("that is {0} characters; announcements speak at most {MAX_SPEECH_CHARS}")]
    SpeechTooLong(usize),
    #[error("voice {0:?} is not a voice name")]
    BadVoice(String),
    #[error("`say` failed: {0}")]
    Speech(String),
    #[error("not a playable WAV file: {0}")]
    NotWav(&'static str),
    #[error(transparent)]
    Io(#[from] io::Error),
}

/// A clip a remote client may ask for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClipSource {
    /// Speak `text` with macOS `say`, in `voice` or the system voice.
    Tts { text: String, voice: Option<String> },
    /// A synthesized chime, by [`Chime`] name.
    Chime { name: String },
}

/// A clip in the media directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredClip {
    /// 32 lowercase hex digits; the file is `<id>.wav`.
    pub id: String,
    pub duration: Duration,
}

impl StoredClip {
    /// Where the players fetch it, given the media listener's base URL
    /// (`http://<address>:<port>`).
    #[must_use]
    pub fn url(&self, base: &str) -> String {
        format!("{}/media/{}.wav", base.trim_end_matches('/'), self.id)
    }
}

/// The media directory: clips by id, removed after a TTL.
#[derive(Debug, Clone)]
pub struct MediaStore {
    dir: PathBuf,
    ttl: Duration,
}

impl MediaStore {
    /// The store in `<data_dir>/media` (created on first use).
    #[must_use]
    pub fn new(data_dir: &Path) -> Self {
        Self {
            dir: data_dir.join("media"),
            ttl: DEFAULT_TTL,
        }
    }

    #[must_use]
    pub fn with_ttl(mut self, ttl: Duration) -> Self {
        self.ttl = ttl;
        self
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Make the clip `source` asks for and store it.
    pub fn make(&self, source: &ClipSource) -> Result<StoredClip, ClipError> {
        match source {
            ClipSource::Chime { name } => self.put(&name.parse::<Chime>()?.wav()),
            ClipSource::Tts { text, voice } => self.speak(text, voice.as_deref()),
        }
    }

    /// Store `wav` (checked to be a PCM WAV) under a new id.
    pub fn put(&self, wav: &[u8]) -> Result<StoredClip, ClipError> {
        let duration = wav_info(wav)?.duration;
        let id = self.new_id()?;
        fs::write(self.dir.join(format!("{id}.wav")), wav)?;
        Ok(StoredClip { id, duration })
    }

    /// Copy the WAV file at `path` into the store. CLI only: never reachable
    /// from HTTP or MCP (see the module docs).
    pub fn import_wav(&self, path: &Path) -> Result<StoredClip, ClipError> {
        self.put(&fs::read(path)?)
    }

    /// Speak `text` into a new clip with macOS `say`.
    pub fn speak(&self, text: &str, voice: Option<&str>) -> Result<StoredClip, ClipError> {
        check_speech(text, voice)?;
        let id = self.new_id()?;
        let path = self.dir.join(format!("{id}.wav"));
        say(text, voice, &path)?;
        let duration = wav_info(&fs::read(&path)?)?.duration;
        Ok(StoredClip { id, duration })
    }

    /// The file for a request path's last segment, `<id>.wav`, if it is a
    /// clip that exists. Anything else (another name, a path, a different
    /// extension) is `None`.
    #[must_use]
    pub fn path(&self, name: &str) -> Option<PathBuf> {
        let id = name.strip_suffix(".wav")?;
        if !is_clip_id(id) {
            return None;
        }
        let path = self.dir.join(name);
        path.is_file().then_some(path)
    }

    /// Remove clips older than the TTL at `now`; returns how many went.
    pub fn prune(&self, now: SystemTime) -> io::Result<usize> {
        let entries = match fs::read_dir(&self.dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(e) => return Err(e),
        };
        let mut removed = 0;
        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let is_clip = name
                .to_str()
                .and_then(|n| n.strip_suffix(".wav"))
                .is_some_and(is_clip_id);
            let expired = entry
                .metadata()?
                .modified()
                .ok()
                .and_then(|m| now.duration_since(m).ok())
                .is_some_and(|age| age > self.ttl);
            if is_clip && expired {
                fs::remove_file(entry.path())?;
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// A fresh id, after making sure the directory exists and pruning it.
    fn new_id(&self) -> Result<String, ClipError> {
        fs::create_dir_all(&self.dir)?;
        self.prune(SystemTime::now())?;
        Ok(clip_id()?)
    }
}

/// 32 lowercase hex digits.
fn is_clip_id(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// A new unguessable clip id: 128 bits from the OS.
pub fn clip_id() -> io::Result<String> {
    let mut bytes = [0u8; 16];
    fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    Ok(bytes.iter().fold(String::with_capacity(32), |mut s, b| {
        use fmt::Write;
        let _ = write!(s, "{b:02x}");
        s
    }))
}

fn check_speech(text: &str, voice: Option<&str>) -> Result<(), ClipError> {
    if text.trim().is_empty() {
        return Err(ClipError::EmptySpeech);
    }
    let chars = text.chars().count();
    if chars > MAX_SPEECH_CHARS {
        return Err(ClipError::SpeechTooLong(chars));
    }
    if let Some(v) = voice {
        let ok = !v.is_empty()
            && v.len() <= 64
            && !v.starts_with('-')
            && v.chars().all(|c| {
                c.is_alphanumeric() || matches!(c, ' ' | '(' | ')' | '.' | '_' | '-' | '\'')
            });
        if !ok {
            return Err(ClipError::BadVoice(v.to_string()));
        }
    }
    Ok(())
}

/// Render `text` to a 44.1 kHz 16-bit WAV at `out`. The text goes in on
/// stdin (`-f -`), so text that starts with `-` is never read as an option.
#[cfg(target_os = "macos")]
fn say(text: &str, voice: Option<&str>, out: &Path) -> Result<(), ClipError> {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let mut cmd = Command::new("/usr/bin/say");
    cmd.arg("-o")
        .arg(out)
        .arg("--file-format=WAVE")
        .arg(format!("--data-format=LEI16@{SAMPLE_RATE}"));
    if let Some(v) = voice {
        cmd.arg("-v").arg(v);
    }
    let mut child = cmd
        .args(["-f", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()?;
    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(text.as_bytes())?;
    }
    let output = child.wait_with_output()?;
    if output.status.success() {
        Ok(())
    } else {
        Err(ClipError::Speech(
            String::from_utf8_lossy(&output.stderr).trim().to_string(),
        ))
    }
}

#[cfg(not(target_os = "macos"))]
fn say(_text: &str, _voice: Option<&str>, _out: &Path) -> Result<(), ClipError> {
    Err(ClipError::Unsupported(
        "speech uses macOS `say`, which this system does not have; \
         announce a chime or a WAV file instead",
    ))
}

/// A synthesized chime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Chime {
    /// Two falling notes, a doorbell.
    Bell,
    /// Three short beeps.
    Beep,
    /// Four rising notes.
    Rise,
}

impl Chime {
    pub const ALL: [Self; 3] = [Self::Bell, Self::Beep, Self::Rise];
    pub const NAMES: [&'static str; 3] = ["bell", "beep", "rise"];

    #[must_use]
    pub fn name(self) -> &'static str {
        match self {
            Self::Bell => "bell",
            Self::Beep => "beep",
            Self::Rise => "rise",
        }
    }

    /// The chime as mono 16-bit samples at [`SAMPLE_RATE`], with a short
    /// silence first so a player that starts late loses none of it.
    #[must_use]
    pub fn samples(self) -> Vec<i16> {
        let mut out = Vec::new();
        silence(&mut out, 250);
        match self {
            Self::Bell => {
                note(&mut out, 659.25, 450, 4.0);
                note(&mut out, 523.25, 900, 3.0);
            }
            Self::Beep => {
                for _ in 0..3 {
                    note(&mut out, 880.0, 120, 0.0);
                    silence(&mut out, 80);
                }
            }
            Self::Rise => {
                for f in [523.25, 659.25, 783.99] {
                    note(&mut out, f, 160, 2.0);
                }
                note(&mut out, 1046.5, 600, 3.0);
            }
        }
        silence(&mut out, 150);
        out
    }

    #[must_use]
    pub fn wav(self) -> Vec<u8> {
        encode_wav(&self.samples(), SAMPLE_RATE)
    }
}

impl FromStr for Chime {
    type Err = ClipError;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        let wanted = s.trim().to_ascii_lowercase();
        Self::ALL
            .into_iter()
            .find(|c| c.name() == wanted)
            .ok_or_else(|| ClipError::UnknownChime { name: s.into() })
    }
}

impl fmt::Display for Chime {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

fn sample_count(ms: u32) -> usize {
    (SAMPLE_RATE / 1000 * ms) as usize
}

fn silence(out: &mut Vec<i16>, ms: u32) {
    out.resize(out.len() + sample_count(ms), 0);
}

/// One note at `freq` Hz for `ms`, with a 5 ms attack and release (no
/// clicks), an exponential `decay` per second, and a soft octave overtone.
#[allow(
    clippy::cast_precision_loss,
    clippy::cast_possible_truncation,
    reason = "audio synthesis: sample indices are small, and the result is within ±0.7 full scale"
)]
fn note(out: &mut Vec<i16>, freq: f32, ms: u32, decay: f32) {
    let n = sample_count(ms);
    let rate = SAMPLE_RATE as f32;
    let edge = 0.005 * rate;
    for i in 0..n {
        let t = i as f32 / rate;
        let ramp = (i as f32 / edge).min((n - i) as f32 / edge).min(1.0);
        let envelope = ramp * (-decay * t).exp();
        let phase = std::f32::consts::TAU * freq * t;
        let wave = (phase.sin() + 0.25 * (2.0 * phase).sin()) / 1.25;
        out.push((wave * envelope * 0.7 * f32::from(i16::MAX)) as i16);
    }
}

/// Mono 16-bit PCM WAV.
#[must_use]
pub fn encode_wav(samples: &[i16], rate: u32) -> Vec<u8> {
    let data_len = u32::try_from(samples.len() * 2).unwrap_or(u32::MAX - 36);
    let mut wav = Vec::with_capacity(44 + samples.len() * 2);
    wav.extend_from_slice(b"RIFF");
    wav.extend_from_slice(&(36 + data_len).to_le_bytes());
    wav.extend_from_slice(b"WAVEfmt ");
    wav.extend_from_slice(&16u32.to_le_bytes());
    wav.extend_from_slice(&1u16.to_le_bytes()); // PCM
    wav.extend_from_slice(&1u16.to_le_bytes()); // mono
    wav.extend_from_slice(&rate.to_le_bytes());
    wav.extend_from_slice(&(rate * 2).to_le_bytes()); // bytes per second
    wav.extend_from_slice(&2u16.to_le_bytes()); // bytes per frame
    wav.extend_from_slice(&16u16.to_le_bytes()); // bits per sample
    wav.extend_from_slice(b"data");
    wav.extend_from_slice(&data_len.to_le_bytes());
    for s in samples {
        wav.extend_from_slice(&s.to_le_bytes());
    }
    wav
}

/// What a PCM WAV holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WavInfo {
    pub sample_rate: u32,
    pub channels: u16,
    pub bits: u16,
    pub duration: Duration,
}

/// Read a PCM WAV's `fmt ` and `data` chunks (other chunks, such as the
/// padding `say` writes, are skipped).
pub fn wav_info(wav: &[u8]) -> Result<WavInfo, ClipError> {
    if wav.len() < 12 || &wav[..4] != b"RIFF" || &wav[8..12] != b"WAVE" {
        return Err(ClipError::NotWav("no RIFF/WAVE header"));
    }
    let u16_at = |i: usize| u16::from_le_bytes([wav[i], wav[i + 1]]);
    let u32_at = |i: usize| u32::from_le_bytes([wav[i], wav[i + 1], wav[i + 2], wav[i + 3]]);
    let mut format = None;
    let mut at = 12;
    while at + 8 <= wav.len() {
        let id = &wav[at..at + 4];
        let len = u32_at(at + 4) as usize;
        let body = at + 8;
        match id {
            b"fmt " => {
                if len < 16 || body + 16 > wav.len() {
                    return Err(ClipError::NotWav("short fmt chunk"));
                }
                // PCM, or WAVE_FORMAT_EXTENSIBLE around PCM.
                if !matches!(u16_at(body), 1 | 0xFFFE) {
                    return Err(ClipError::NotWav("not PCM"));
                }
                let byte_rate = u32_at(body + 8);
                if byte_rate == 0 {
                    return Err(ClipError::NotWav("zero byte rate"));
                }
                format = Some((
                    u32_at(body + 4),
                    u16_at(body + 2),
                    u16_at(body + 14),
                    byte_rate,
                ));
            }
            b"data" => {
                let (sample_rate, channels, bits, byte_rate) =
                    format.ok_or(ClipError::NotWav("data before fmt"))?;
                let data = u64::try_from(len.min(wav.len() - body)).unwrap_or(u64::MAX);
                return Ok(WavInfo {
                    sample_rate,
                    channels,
                    bits,
                    duration: Duration::from_millis(data * 1000 / u64::from(byte_rate)),
                });
            }
            _ => {}
        }
        at = body + len + (len & 1);
    }
    Err(ClipError::NotWav("no data chunk"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn store() -> (tempfile::TempDir, MediaStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = MediaStore::new(dir.path());
        (dir, store)
    }

    #[test]
    fn wav_headers_round_trip_and_skip_unknown_chunks() {
        let wav = encode_wav(&vec![0; 44_100], 44_100);
        assert_eq!(wav.len(), 44 + 88_200);
        assert_eq!(&wav[..4], b"RIFF");
        assert_eq!(
            u32::from_le_bytes(wav[4..8].try_into().unwrap()),
            36 + 88_200
        );
        assert_eq!(&wav[36..40], b"data");
        assert_eq!(
            wav_info(&wav).unwrap(),
            WavInfo {
                sample_rate: 44_100,
                channels: 1,
                bits: 16,
                duration: Duration::from_secs(1),
            }
        );

        // A padding chunk between fmt and data, as `say` writes.
        let mut padded = wav[..36].to_vec();
        padded.extend_from_slice(b"FLLR");
        padded.extend_from_slice(&3u32.to_le_bytes());
        padded.extend_from_slice(&[0, 0, 0, 0]); // 3 bytes + 1 pad
        padded.extend_from_slice(&wav[36..36 + 8 + 22_050]);
        assert_eq!(
            wav_info(&padded).unwrap().duration,
            Duration::from_millis(250)
        );

        assert!(matches!(
            wav_info(b"ID3\x03 not a wav"),
            Err(ClipError::NotWav(_))
        ));
        let mut float = wav.clone();
        float[20] = 3; // IEEE float
        assert!(matches!(
            wav_info(&float),
            Err(ClipError::NotWav("not PCM"))
        ));
        assert!(matches!(
            wav_info(&wav[..40]),
            Err(ClipError::NotWav("no data chunk"))
        ));
    }

    #[test]
    fn chimes_are_short_bounded_and_named() {
        for chime in Chime::ALL {
            let samples = chime.samples();
            let n = samples.len();
            assert!(
                (sample_count(500)..sample_count(3000)).contains(&n),
                "{chime}: {n}"
            );
            let peak = samples.iter().map(|s| s.unsigned_abs()).max().unwrap();
            assert!(peak > 10_000 && peak < 30_000, "{chime}: peak {peak}");
            assert_eq!(samples[0], 0, "{chime} starts in silence");
            assert_eq!(chime.name().parse::<Chime>().unwrap(), chime);
            assert_eq!(
                chime.to_string().to_uppercase().parse::<Chime>().unwrap(),
                chime
            );
        }
        let err = "gong".parse::<Chime>().unwrap_err();
        assert_eq!(
            err.to_string(),
            "unknown chime \"gong\"; choose one of: bell, beep, rise"
        );
    }

    #[test]
    fn clip_ids_are_unguessable_hex() {
        let a = clip_id().unwrap();
        let b = clip_id().unwrap();
        assert!(is_clip_id(&a) && is_clip_id(&b));
        assert_ne!(a, b);
        assert!(!is_clip_id("0123456789ABCDEF0123456789abcdef"));
        assert!(!is_clip_id("0123"));
    }

    #[test]
    fn stored_clips_are_found_only_by_exact_id_and_expire() {
        let (_dir, store) = store();
        let clip = store
            .make(&ClipSource::Chime {
                name: "Bell".into(),
            })
            .unwrap();
        assert!(clip.duration > Duration::from_secs(1));
        assert_eq!(
            clip.url("http://192.0.2.10:3400/"),
            format!("http://192.0.2.10:3400/media/{}.wav", clip.id)
        );
        let name = format!("{}.wav", clip.id);
        let path = store.path(&name).unwrap();
        assert_eq!(fs::read(&path).unwrap(), Chime::Bell.wav());
        for bad in [
            "../media.wav".to_string(),
            format!("../media/{name}"),
            clip.id.clone(),
            format!("{}.mp3", clip.id),
            format!("{}.wav", clip.id.to_uppercase()),
            format!("{}.wav", "0".repeat(32)),
        ] {
            assert_eq!(store.path(&bad), None, "{bad}");
        }

        // A file that is not a clip is never pruned.
        fs::write(store.dir().join("notes.txt"), "keep").unwrap();
        assert_eq!(store.prune(SystemTime::now()).unwrap(), 0);
        let later = SystemTime::now() + DEFAULT_TTL + Duration::from_secs(1);
        assert_eq!(store.prune(later).unwrap(), 1);
        assert_eq!(store.path(&name), None);
        assert!(store.dir().join("notes.txt").exists());
    }

    #[test]
    fn imports_take_wav_only() {
        let (dir, store) = store();
        let wav = dir.path().join("in.wav");
        fs::write(&wav, encode_wav(&[0; 4_410], SAMPLE_RATE)).unwrap();
        let clip = store.import_wav(&wav).unwrap();
        assert_eq!(clip.duration, Duration::from_millis(100));
        let mp3 = dir.path().join("in.mp3");
        fs::write(&mp3, b"ID3\x04\0\0\0\0\0\0").unwrap();
        assert!(matches!(store.import_wav(&mp3), Err(ClipError::NotWav(_))));
    }

    #[test]
    fn speech_input_is_checked_before_say_runs() {
        assert!(matches!(
            check_speech("  ", None),
            Err(ClipError::EmptySpeech)
        ));
        let long = "a".repeat(MAX_SPEECH_CHARS + 1);
        assert!(matches!(
            check_speech(&long, None),
            Err(ClipError::SpeechTooLong(_))
        ));
        assert!(check_speech("-v Bad starts with a dash; still fine as text", None).is_ok());
        assert!(check_speech("Dinner", Some("Samantha")).is_ok());
        assert!(check_speech("Dinner", Some("Eddy (English (UK))")).is_ok());
        for voice in ["-o /tmp/x", "", "a;b", "a/b"] {
            assert!(
                matches!(
                    check_speech("Dinner", Some(voice)),
                    Err(ClipError::BadVoice(_))
                ),
                "{voice}"
            );
        }
    }

    #[cfg(not(target_os = "macos"))]
    #[test]
    fn speech_is_unsupported_off_macos() {
        let (_dir, store) = store();
        let err = store
            .make(&ClipSource::Tts {
                text: "Dinner is ready".into(),
                voice: None,
            })
            .unwrap_err();
        assert!(err.to_string().contains("chime"), "{err}");
    }

    /// Runs `say` for real: set `FSONOS_TEST_SAY=1` on a Mac.
    #[cfg(target_os = "macos")]
    #[test]
    fn say_renders_a_wav_clip() {
        if std::env::var_os("FSONOS_TEST_SAY").is_none() {
            eprintln!("skipped: set FSONOS_TEST_SAY=1 to run macOS `say`");
            return;
        }
        let (_dir, store) = store();
        let clip = store
            .make(&ClipSource::Tts {
                text: "-n Dinner is ready".into(),
                voice: None,
            })
            .unwrap();
        assert!(clip.duration > Duration::from_millis(500), "{clip:?}");
        let wav = fs::read(store.path(&format!("{}.wav", clip.id)).unwrap()).unwrap();
        let info = wav_info(&wav).unwrap();
        assert_eq!((info.sample_rate, info.bits), (SAMPLE_RATE, 16), "{info:?}");
    }
}
