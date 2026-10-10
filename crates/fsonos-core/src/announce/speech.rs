//! Offline speech engines. Configuration belongs to the daemon's owner;
//! requests supply only text and a voice, never a program or arguments.
//!
//! speech.toml lives in the data directory. FSONOS_TTS_COMMAND is a JSON
//! argv array, not a shell command. Text always arrives on stdin. Templates
//! substitute {output} and {voice} inside individual arguments without
//! splitting, quoting, or invoking a shell.

use super::clip::{ClipError, MAX_WAV_BYTES, SAMPLE_RATE, check_speech, read_wav};
use serde::Deserialize;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::time::{Duration, Instant};

/// The default deadline includes model loading on the local CPU.
pub const DEFAULT_TIMEOUT_SECS: u64 = 180;
const MAX_STDERR_BYTES: u64 = 64 * 1024;
const POLL: Duration = Duration::from_millis(20);

/// An explicit backend, or automatic discovery.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum BackendKind {
    #[default]
    Auto,
    #[serde(alias = "ftts")]
    Frankentts,
    #[serde(alias = "macos")]
    Say,
    EspeakNg,
    Piper,
    Command,
}

/// Owner-controlled configuration. Omitted fields keep offline defaults.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct SpeechConfig {
    pub backend: BackendKind,
    /// Executable followed by arguments; {output} is required.
    pub command: Vec<String>,
    pub voice: Option<String>,
    /// An already-installed .onnx file, with its .onnx.json beside it.
    pub piper_model: Option<PathBuf>,
    pub timeout_secs: u64,
}

impl Default for SpeechConfig {
    fn default() -> Self {
        Self {
            backend: BackendKind::Auto,
            command: Vec::new(),
            voice: None,
            piper_model: None,
            timeout_secs: DEFAULT_TIMEOUT_SECS,
        }
    }
}

impl SpeechConfig {
    /// Load speech.toml, then the explicit environment overrides. A missing
    /// config uses defaults; a broken config is never silently ignored.
    pub fn load(data_dir: &Path) -> Result<Self, ClipError> {
        Self::load_with_env(data_dir, |key| match std::env::var(key) {
            Ok(value) => Ok(Some(value)),
            Err(std::env::VarError::NotPresent) => Ok(None),
            Err(std::env::VarError::NotUnicode(_)) => {
                Err(ClipError::SpeechConfig(format!("{key} must contain UTF-8")))
            }
        })
    }

    fn load_with_env(
        data_dir: &Path,
        env: impl Fn(&str) -> Result<Option<String>, ClipError>,
    ) -> Result<Self, ClipError> {
        let path = data_dir.join("speech.toml");
        let mut config = match fs::read_to_string(&path) {
            Ok(text) => toml::from_str(&text)
                .map_err(|e| ClipError::SpeechConfig(format!("{}: {e}", path.display())))?,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Self::default(),
            Err(e) => {
                return Err(ClipError::SpeechConfig(format!("{}: {e}", path.display())));
            }
        };
        if let Some(value) = env("FSONOS_TTS_BACKEND")? {
            config.backend = serde_json::from_value(serde_json::Value::String(value))
                .map_err(|e| ClipError::SpeechConfig(format!("FSONOS_TTS_BACKEND: {e}")))?;
        }
        if let Some(value) = env("FSONOS_TTS_COMMAND")? {
            config.command = serde_json::from_str(&value).map_err(|e| {
                ClipError::SpeechConfig(format!(
                    "FSONOS_TTS_COMMAND must be a JSON argv array: {e}"
                ))
            })?;
            // An explicit command override overrides the file's backend too.
            config.backend = BackendKind::Command;
        }
        if let Some(value) = env("FSONOS_TTS_VOICE")? {
            config.voice = Some(value);
        }
        if let Some(value) = env("FSONOS_PIPER_MODEL")? {
            config.piper_model = Some(PathBuf::from(value));
        }
        if let Some(value) = env("FSONOS_TTS_TIMEOUT_SECS")? {
            config.timeout_secs = value
                .parse()
                .map_err(|e| ClipError::SpeechConfig(format!("FSONOS_TTS_TIMEOUT_SECS: {e}")))?;
        }
        config.validate()?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), ClipError> {
        if !(1..=600).contains(&self.timeout_secs) {
            return Err(ClipError::SpeechConfig(
                "timeout_secs must be between 1 and 600".into(),
            ));
        }
        if let Some(voice) = &self.voice {
            check_speech("check", Some(voice))?;
        }
        if !self.command.is_empty() || self.backend == BackendKind::Command {
            let Some(program) = self.command.first().filter(|s| !s.is_empty()) else {
                return Err(ClipError::SpeechConfig(
                    "command needs an executable and arguments containing {output}".into(),
                ));
            };
            if program.contains(['{', '}', '\0']) {
                return Err(ClipError::SpeechConfig(
                    "the command executable cannot contain placeholders or NUL".into(),
                ));
            }
            if self.command.len() > 64
                || self
                    .command
                    .iter()
                    .any(|arg| arg.len() > 16_384 || arg.contains('\0'))
            {
                return Err(ClipError::SpeechConfig(
                    "command is too large or contains NUL".into(),
                ));
            }
            let mut outputs = 0;
            for arg in &self.command[1..] {
                outputs += arg.matches("{output}").count();
                let literal = arg.replace("{output}", "").replace("{voice}", "");
                if literal.contains(['{', '}']) {
                    return Err(ClipError::SpeechConfig(
                        "command placeholders are {output} and {voice}; text goes on stdin".into(),
                    ));
                }
            }
            if outputs != 1 {
                return Err(ClipError::SpeechConfig(
                    "command must contain exactly one {output} placeholder".into(),
                ));
            }
        }
        Ok(())
    }

    /// Resolve a backend without executing it. Discovery proves executable
    /// availability, not model readiness or successful synthesis.
    pub fn discover(&self, voice: Option<&str>) -> Result<SpeechBackend, ClipError> {
        let path = std::env::var_os("PATH").unwrap_or_default();
        self.discover_with(voice, cfg!(target_os = "macos"), |program| {
            find_executable(program, &path)
        })
    }

    fn discover_with(
        &self,
        voice: Option<&str>,
        macos: bool,
        find: impl Fn(&str) -> Option<PathBuf>,
    ) -> Result<SpeechBackend, ClipError> {
        self.validate()?;
        let voice = voice.or(self.voice.as_deref());
        check_speech("check", voice)?;
        let kind = match self.backend {
            BackendKind::Auto if !self.command.is_empty() => BackendKind::Command,
            BackendKind::Auto if macos => BackendKind::Say,
            BackendKind::Auto
                if self.piper_model.is_some()
                    || voice
                        .is_some_and(|v| Path::new(v).extension() == Some(OsStr::new("onnx"))) =>
            {
                // An explicitly selected model must not silently switch voices.
                BackendKind::Piper
            }
            BackendKind::Auto if find("ftts").or_else(|| find("franken_tts")).is_some() => {
                BackendKind::Frankentts
            }
            BackendKind::Auto => BackendKind::EspeakNg,
            other => other,
        };
        let executable = match kind {
            BackendKind::Frankentts => find("ftts").or_else(|| find("franken_tts")),
            BackendKind::Say => find("/usr/bin/say"),
            BackendKind::EspeakNg => find("espeak-ng"),
            BackendKind::Piper => find("piper"),
            BackendKind::Command => find(&self.command[0]),
            BackendKind::Auto => unreachable!("auto resolved above"),
        }
        .ok_or_else(|| {
            ClipError::SpeechUnavailable(match kind {
                BackendKind::Frankentts =>
                    "FrankenTTS executable not found; install ftts on the daemon host and run ftts pull once".into(),
                BackendKind::Say => "macOS /usr/bin/say is not available".into(),
                BackendKind::Piper => "Piper executable not found; install piper and a local .onnx model, or select frankentts or espeak-ng".into(),
                BackendKind::Command => format!("speech command {:?} is not executable or not on PATH", self.command[0]),
                _ => "no local speech engine found; install FrankenTTS (ftts, then ftts pull) or espeak-ng, or configure speech.toml / FSONOS_TTS_COMMAND".into(),
            })
        })?;
        let piper_model = if kind == BackendKind::Piper {
            let model = voice.map(PathBuf::from).or_else(|| self.piper_model.clone())
                .ok_or_else(|| ClipError::SpeechUnavailable(
                    "Piper needs an installed .onnx model: set piper_model, FSONOS_PIPER_MODEL, or --voice /path/voice.onnx".into()
                ))?;
            let mut config_path = model.as_os_str().to_os_string();
            config_path.push(".json");
            if model.extension() != Some(OsStr::new("onnx"))
                || !model.is_file()
                || !Path::new(&config_path).is_file()
            {
                return Err(ClipError::SpeechUnavailable(
                    "Piper needs an existing .onnx model and its adjacent .onnx.json configuration; no models are downloaded automatically".into()
                ));
            }
            // Preserve a symlink's spelling: Piper looks for .json beside
            // the path we pass, which must be the sidecar checked above.
            Some(std::path::absolute(model)?)
        } else {
            None
        };
        if kind == BackendKind::Command
            && voice.is_some()
            && !self.command[1..].iter().any(|arg| arg.contains("{voice}"))
        {
            return Err(ClipError::SpeechConfig(
                "this command does not accept a voice; add a {voice} argument placeholder".into(),
            ));
        }
        if kind == BackendKind::Command
            && voice.is_none()
            && self.command[1..].iter().any(|arg| arg.contains("{voice}"))
        {
            return Err(ClipError::SpeechConfig(
                "command uses {voice}; set a default voice or supply --voice".into(),
            ));
        }
        Ok(SpeechBackend {
            kind,
            executable,
            voice: voice.map(str::to_owned),
            piper_model,
            command: self.command.clone(),
            timeout: Duration::from_secs(self.timeout_secs),
        })
    }
}

/// One resolved local engine. Its settings come only from owner config.
#[derive(Debug, Clone)]
pub struct SpeechBackend {
    kind: BackendKind,
    executable: PathBuf,
    voice: Option<String>,
    piper_model: Option<PathBuf>,
    command: Vec<String>,
    timeout: Duration,
}

impl SpeechBackend {
    #[must_use]
    pub fn name(&self) -> &'static str {
        match self.kind {
            BackendKind::Frankentts => "frankentts",
            BackendKind::Say => "say",
            BackendKind::EspeakNg => "espeak-ng",
            BackendKind::Piper => "piper",
            BackendKind::Command => "command",
            BackendKind::Auto => unreachable!("auto is never a resolved backend"),
        }
    }

    #[must_use]
    pub fn executable(&self) -> &Path {
        &self.executable
    }

    fn args(&self, out: &Path) -> Vec<OsString> {
        let mut args: Vec<OsString> = match self.kind {
            BackendKind::Frankentts => [
                OsString::from("say"),
                "--file".into(),
                "-".into(),
                "--output".into(),
                out.as_os_str().to_owned(),
                // Keep lifecycle and cancellation under this parent process.
                "--no-resident".into(),
            ]
            .into(),
            BackendKind::Say => vec![
                "-o".into(),
                out.as_os_str().to_owned(),
                "--file-format=WAVE".into(),
                format!("--data-format=LEI16@{SAMPLE_RATE}").into(),
                "-f".into(),
                "-".into(),
            ],
            BackendKind::EspeakNg => {
                vec!["--stdin".into(), "-w".into(), out.as_os_str().to_owned()]
            }
            // -m / -f work with both the original Piper binary and piper-tts.
            BackendKind::Piper => vec![
                "-m".into(),
                self.piper_model
                    .as_ref()
                    .expect("resolved model")
                    .as_os_str()
                    .to_owned(),
                "-f".into(),
                out.as_os_str().to_owned(),
            ],
            BackendKind::Command => self.command[1..]
                .iter()
                .map(|arg| expand_arg(arg, out, self.voice.as_deref().unwrap_or("")))
                .collect(),
            BackendKind::Auto => unreachable!("auto is never a resolved backend"),
        };
        if let Some(voice) = &self.voice {
            match self.kind {
                BackendKind::Frankentts => args.push(format!("--voice={voice}").into()),
                BackendKind::Say | BackendKind::EspeakNg => {
                    args.extend([OsString::from("-v"), OsString::from(voice)]);
                }
                _ => {}
            }
        }
        args
    }

    /// Synthesize into a private temporary directory, then read and validate
    /// the complete WAV. Failed/partial output never has a media clip id.
    pub(super) fn render(&self, text: &str, media_dir: &Path) -> Result<Vec<u8>, ClipError> {
        check_speech(text, self.voice.as_deref())?;
        fs::create_dir_all(media_dir)?;
        let staging = tempfile::Builder::new()
            .prefix(".speech-")
            .tempdir_in(media_dir)?;
        let out = fs::canonicalize(staging.path())?.join("speech.wav");

        // A regular anonymous stdin file cannot block the parent if a broken
        // engine never reads stdin or exits before reading it.
        let mut input = tempfile::tempfile()?;
        input.write_all(text.as_bytes())?;
        input.seek(SeekFrom::Start(0))?;
        let mut stderr = tempfile::tempfile()?;
        let mut command = Command::new(&self.executable);
        command
            .args(self.args(&out))
            .stdin(Stdio::from(input))
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr.try_clone()?));
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let child = command
            .spawn()
            .map_err(|e| ClipError::Speech(format!("could not start {}: {e}", self.name())))?;
        let mut child = ChildGuard {
            child,
            completed: false,
        };
        let deadline = Instant::now() + self.timeout;
        let status = loop {
            if let Some(status) = child.try_wait()? {
                break status;
            }
            if Instant::now() >= deadline {
                return Err(ClipError::SpeechTimeout {
                    backend: self.name().into(),
                    seconds: self.timeout.as_secs(),
                });
            }
            if stderr.metadata()?.len() > MAX_STDERR_BYTES {
                return Err(ClipError::Speech(format!(
                    "{} produced more than 64 KiB of diagnostics",
                    self.name()
                )));
            }
            if fs::metadata(&out).is_ok_and(|m| m.len() > MAX_WAV_BYTES as u64) {
                return Err(ClipError::TooLarge);
            }
            std::thread::sleep(POLL.min(deadline.saturating_duration_since(Instant::now())));
        };
        if !status.success() {
            stderr.seek(SeekFrom::Start(0))?;
            let mut detail = Vec::new();
            stderr.take(MAX_STDERR_BYTES).read_to_end(&mut detail)?;
            let detail = String::from_utf8_lossy(&detail);
            return Err(ClipError::Speech(format!(
                "{} exited with {status}: {}",
                self.name(),
                detail.trim()
            )));
        }
        read_wav(&out).map_err(|e| match e {
            ClipError::Io(e) => ClipError::Speech(format!(
                "{} exited successfully but did not write a readable WAV: {e}",
                self.name()
            )),
            other => other,
        })
    }
}

/// Reap the child on every return path, including timeout or I/O error.
struct ChildGuard {
    child: Child,
    completed: bool,
}

impl ChildGuard {
    fn try_wait(&mut self) -> std::io::Result<Option<ExitStatus>> {
        #[cfg(any(target_os = "linux", target_os = "macos"))]
        {
            use rustix::process::{Pid, WaitId, WaitIdOptions, waitid};
            // Observe completion without reaping. Even a wrapper that exits
            // before its engine must leave no running descendants behind.
            // NOWAIT keeps the PID reserved until group cleanup is complete.
            let status = waitid(
                WaitId::Pid(Pid::from_child(&self.child)),
                WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
            )?;
            if status.is_none() {
                return Ok(None);
            }
            self.kill_group();
            let status = self.child.wait()?;
            self.completed = true;
            Ok(Some(status))
        }
        #[cfg(not(any(target_os = "linux", target_os = "macos")))]
        {
            let status = self.child.try_wait()?;
            self.completed = status.is_some();
            Ok(status)
        }
    }

    fn kill_group(&self) {
        #[cfg(unix)]
        {
            use rustix::process::{Pid, Signal, kill_process_group};
            let _ = kill_process_group(Pid::from_child(&self.child), Signal::KILL);
        }
    }
}

impl Drop for ChildGuard {
    fn drop(&mut self) {
        if !self.completed {
            // Do not reap before signalling: the live/unreaped child keeps
            // its PID reserved, so this cannot target a reused process id.
            self.kill_group();
            let _ = self.child.kill();
        }
        let _ = self.child.wait();
    }
}

/// Expand the template once: a voice containing "{output}" remains literal.
/// Paths retain their OS bytes, including a non-UTF-8 data directory.
fn expand_arg(template: &str, out: &Path, voice: &str) -> OsString {
    let mut arg = OsString::new();
    let mut rest = template;
    while let Some(at) = rest.find('{') {
        arg.push(&rest[..at]);
        rest = &rest[at..];
        if let Some(tail) = rest.strip_prefix("{output}") {
            arg.push(out);
            rest = tail;
        } else if let Some(tail) = rest.strip_prefix("{voice}") {
            arg.push(voice);
            rest = tail;
        } else {
            unreachable!("templates are validated before expansion");
        }
    }
    arg.push(rest);
    arg
}

fn find_executable(program: &str, path: &OsStr) -> Option<PathBuf> {
    let program = Path::new(program);
    if program.components().count() > 1 {
        return is_executable(program).then(|| program.to_path_buf());
    }
    // Ignore empty PATH entries (implicit current directory).
    std::env::split_paths(path)
        .filter(|dir| !dir.as_os_str().is_empty())
        .map(|dir| dir.join(program))
        .find(|candidate| is_executable(candidate))
}

fn is_executable(path: &Path) -> bool {
    let Ok(meta) = fs::metadata(path) else {
        return false;
    };
    if !meta.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        meta.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
mod tests;
