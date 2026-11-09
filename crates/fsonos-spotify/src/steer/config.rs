//! Editable owner-wide mood overrides and automatic time-of-day programs.
//!
//! The document retains only the owner's definitions. An absent program list
//! follows the built-ins; an empty list disables automatic moods. The existing
//! parser remains the authority for filter and program semantics.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{DjConstraints, Moods, MoodsFile, Program};
use crate::SpotifyError;
use crate::client::sha256;

mod references;
pub use references::check_references;

/// Maximum encoded size of a configuration written through the editor.
pub const MAX_CONFIG_BYTES: usize = 256 * 1024;
/// Maximum number of custom moods and built-in overrides in an edit.
pub const MAX_CUSTOM_MOODS: usize = 128;
/// Maximum number of ordered automatic programs in an edit.
pub const MAX_PROGRAMS: usize = 256;

const DAYS: [&str; 7] = ["mon", "tue", "wed", "thu", "fri", "sat", "sun"];
static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);

/// The editable part of `moods.toml`, shared by every DJ zone in the daemon.
/// Removing an override restores a built-in of that name or removes a custom
/// mood. Programs name the effective moods after overrides have been applied.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct MoodsConfig {
    pub moods: BTreeMap<String, DjConstraints>,
    /// `None` follows the built-in program, `Some([])` disables automatic
    /// moods, and a nonempty list replaces the entire program in order.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub programs: Option<Vec<ProgramConfig>>,
}

/// An ordered local-time program. The first matching program wins. Overnight
/// windows belong to their starting day; equal endpoints cover that whole day.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProgramConfig {
    /// Weekday names; reads normalize them to `mon` through `sun`.
    #[serde(default = "every_day")]
    pub days: Vec<String>,
    pub from: String,
    pub to: String,
    pub mood: String,
}

fn every_day() -> Vec<String> {
    DAYS.iter().map(|day| (*day).to_owned()).collect()
}

impl From<&Program> for ProgramConfig {
    fn from(program: &Program) -> Self {
        let (from, to) = program.window();
        let hhmm = |minute: u16| format!("{:02}:{:02}", minute / 60, minute % 60);
        Self {
            days: DAYS
                .iter()
                .zip(program.days())
                .filter(|(_, active)| *active)
                .map(|(day, _)| (*day).to_owned())
                .collect(),
            from: hhmm(from),
            to: hhmm(to),
            mood: program.mood.clone(),
        }
    }
}

/// A complete editor read. The revision is a SHA-256 digest of the exact file
/// bytes, with an absent file distinguished from an existing empty one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoodsSnapshot {
    pub config: MoodsConfig,
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MoodsSaved {
    pub changed: bool,
    pub snapshot: MoodsSnapshot,
}

/// Configuration edits never bypass a stale revision. I/O failures identify
/// the publication stage so callers can distinguish a failed write from a
/// successfully renamed file whose directory sync could not be confirmed.
#[derive(Debug, Error)]
pub enum ConfigWriteError {
    #[error("moods.toml changed since it was read; reload before saving")]
    Conflict { current_revision: String },
    #[error(transparent)]
    Spotify(#[from] SpotifyError),
    #[error("could not lock mood configuration {}: {source}", path.display())]
    Lock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not stage mood configuration {}: {source}", path.display())]
    Stage {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not confirm publication of mood configuration {}: {source}", path.display())]
    Publish {
        path: PathBuf,
        #[source]
        source: io::Error,
        /// True means rename succeeded and only directory durability failed.
        /// The new document may already be visible; callers should read back.
        published: bool,
    },
}

impl MoodsConfig {
    /// Read the existing TOML format, including named day groups such as
    /// `weekdays`. Preserve all constraint fields and normalize only the
    /// case-insensitive names, weekday lists, and time spelling.
    ///
    /// Local files retain the existing parser's acceptance. Editor size limits
    /// apply to writes, so a remote read never silently drops local settings.
    pub fn parse(text: &str, source: &str) -> Result<Self, SpotifyError> {
        let effective = Moods::parse(text, source)?;
        let file: MoodsFile =
            toml::from_str(text).map_err(|e| SpotifyError::Config(format!("{source}: {e}")))?;
        Ok(Self {
            moods: file
                .moods
                .into_iter()
                .map(|(name, constraints)| (name.to_lowercase(), constraints.into_inner()))
                .collect(),
            programs: file.programs.map(|_| {
                effective
                    .programs()
                    .iter()
                    .map(ProgramConfig::from)
                    .collect()
            }),
        })
    }

    /// The same effective mood catalogue and programs that normal playback and
    /// rehearsal consume, without changing the current DJ or touching speakers.
    pub fn resolve(&self) -> Result<Moods, SpotifyError> {
        Moods::parse(&self.encode()?, "moods.toml")
    }

    /// A validated, canonical TOML document suitable for an editor write.
    pub fn to_toml(&self) -> Result<String, SpotifyError> {
        self.check_counts()?;
        let text = self.encode()?;
        check_bytes(&text)?;
        let canonical = Self::parse(&text, "moods.toml")?.encode()?;
        check_bytes(&canonical)?;
        Ok(canonical)
    }

    /// Load an editable document and exact-byte revision. A missing file has
    /// no overrides and follows all of the current built-in defaults.
    pub fn load(path: &Path) -> Result<MoodsSnapshot, SpotifyError> {
        snapshot(read(path)?.as_deref(), path)
    }

    /// Compare and replace under a stable sibling file lock. All daemon/CLI
    /// writers using this method serialize, including separate processes.
    /// An unchanged document preserves the original bytes and comments.
    ///
    /// External text editors need not honor the lock; their edits before the
    /// comparison invalidate the revision, but the filesystem offers no atomic
    /// compare-and-rename against an uncooperative writer.
    pub fn save(
        &self,
        path: &Path,
        expected_revision: &str,
    ) -> Result<MoodsSaved, ConfigWriteError> {
        let text = self.to_toml()?;
        let config = Self::parse(&text, "moods.toml")?;
        let _lock = lock(path)?;
        let bytes = read(path)?;
        let current_revision = revision(bytes.as_deref());
        if current_revision != expected_revision {
            return Err(ConfigWriteError::Conflict { current_revision });
        }
        let current = snapshot(bytes.as_deref(), path)?;
        if current.config == config {
            return Ok(MoodsSaved {
                changed: false,
                snapshot: current,
            });
        }
        publish(path, text.as_bytes())?;
        Ok(MoodsSaved {
            changed: true,
            snapshot: MoodsSnapshot {
                config,
                revision: revision(Some(text.as_bytes())),
            },
        })
    }

    fn encode(&self) -> Result<String, SpotifyError> {
        toml::to_string(self).map_err(|e| SpotifyError::Config(e.to_string()))
    }

    fn check_counts(&self) -> Result<(), SpotifyError> {
        if self.moods.len() > MAX_CUSTOM_MOODS {
            return Err(SpotifyError::Config(format!(
                "mood edits support at most {MAX_CUSTOM_MOODS} custom moods and overrides"
            )));
        }
        if self
            .programs
            .as_ref()
            .is_some_and(|programs| programs.len() > MAX_PROGRAMS)
        {
            return Err(SpotifyError::Config(format!(
                "mood edits support at most {MAX_PROGRAMS} automatic programs"
            )));
        }
        Ok(())
    }
}

fn check_bytes(text: &str) -> Result<(), SpotifyError> {
    if text.len() > MAX_CONFIG_BYTES {
        return Err(SpotifyError::Config(format!(
            "mood edits support at most {MAX_CONFIG_BYTES} encoded bytes"
        )));
    }
    Ok(())
}

fn read(path: &Path) -> Result<Option<Vec<u8>>, SpotifyError> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(bytes)),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn snapshot(bytes: Option<&[u8]>, path: &Path) -> Result<MoodsSnapshot, SpotifyError> {
    let config = match bytes {
        None => MoodsConfig::default(),
        Some(bytes) => {
            let text = std::str::from_utf8(bytes)
                .map_err(|error| SpotifyError::Config(format!("{}: {error}", path.display())))?;
            MoodsConfig::parse(text, &path.display().to_string())?
        }
    };
    Ok(MoodsSnapshot {
        config,
        revision: revision(bytes),
    })
}

fn revision(bytes: Option<&[u8]>) -> String {
    let mut tagged = Vec::with_capacity(bytes.map_or(1, |bytes| bytes.len() + 1));
    tagged.push(u8::from(bytes.is_some()));
    if let Some(bytes) = bytes {
        tagged.extend_from_slice(bytes);
    }
    let mut revision = String::with_capacity(64);
    for byte in sha256(&tagged) {
        let _ = write!(revision, "{byte:02x}");
    }
    revision
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn lock(path: &Path) -> Result<File, ConfigWriteError> {
    let lock_path = path.with_extension("toml.lock");
    let acquire = || -> io::Result<File> {
        fs::create_dir_all(parent(path))?;
        let mut options = OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let file = options.open(&lock_path)?;
        file.lock()?;
        Ok(file)
    };
    acquire().map_err(|source| ConfigWriteError::Lock {
        path: lock_path,
        source,
    })
}

fn publish(path: &Path, bytes: &[u8]) -> Result<(), ConfigWriteError> {
    for _ in 0..128 {
        let temporary = path.with_extension(format!(
            "toml.{}.{}.tmp",
            std::process::id(),
            NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
        ));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = match options.open(&temporary) {
            Ok(file) => file,
            Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
            Err(source) => {
                return Err(ConfigWriteError::Stage {
                    path: temporary,
                    source,
                });
            }
        };
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(source) = written {
            let _ = fs::remove_file(&temporary);
            return Err(ConfigWriteError::Stage {
                path: temporary,
                source,
            });
        }
        if let Err(source) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(ConfigWriteError::Publish {
                path: path.to_owned(),
                source,
                published: false,
            });
        }
        #[cfg(unix)]
        File::open(parent(path))
            .and_then(|directory| directory.sync_all())
            .map_err(|source| ConfigWriteError::Publish {
                path: path.to_owned(),
                source,
                published: true,
            })?;
        return Ok(());
    }
    Err(ConfigWriteError::Stage {
        path: path.to_owned(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a mood-configuration staging file",
        ),
    })
}

#[cfg(test)]
mod tests;
