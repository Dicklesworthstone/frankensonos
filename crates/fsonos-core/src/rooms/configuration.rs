//! Reviewed edits of `aliases.toml`, shared by daemon and direct CLI writers.
//!
//! Every accepted change retains unrelated source bytes and comments. The
//! existing parser is authoritative; a valid layout that cannot be edited
//! safely is refused instead of being rewritten as a different document.

use super::{AliasError, Aliases, RESERVED, normalize_room};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod edit;
pub use edit::edit;

/// Bound configuration reads and writes without truncating an accepted file.
pub const MAX_CONFIG_BYTES: usize = 256 * 1024;
/// Alias names and room targets follow the room resolver's input bound.
pub const MAX_NAME_CHARS: usize = 128;
/// A client is an exact policy principal, including a long email or tailnet id.
pub const MAX_CLIENT_BYTES: usize = 512;
/// One room set cannot grow without a bound.
pub const MAX_ALIAS_ROOMS: usize = 512;

static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);

/// One complete edit. Aliases name real rooms, which may currently be offline.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AliasEdit {
    SetAlias {
        name: String,
        rooms: Vec<String>,
    },
    RemoveAlias {
        name: String,
    },
    SetDefault {
        client: Option<String>,
        room: String,
    },
    ClearDefault {
        client: Option<String>,
    },
}

impl AliasEdit {
    /// Validate authored names and counts before discovery or file I/O.
    /// An offline or renamed room is a discovery warning, not a parse error.
    pub fn validate(&self) -> Result<(), ConfigurationError> {
        match self {
            Self::SetAlias { name, rooms } => {
                name_ok("alias name", name)?;
                if rooms.is_empty() || rooms.len() > MAX_ALIAS_ROOMS {
                    return Err(invalid(format!(
                        "an alias must name between 1 and {MAX_ALIAS_ROOMS} rooms"
                    )));
                }
                for room in rooms {
                    name_ok("room target", room)?;
                }
            }
            Self::RemoveAlias { name } => name_ok("alias name", name)?,
            Self::SetDefault { client, room } => {
                client_ok(client.as_deref())?;
                name_ok("default room", room)?;
            }
            Self::ClearDefault { client } => client_ok(client.as_deref())?,
        }
        Ok(())
    }
}

fn name_ok(what: &str, value: &str) -> Result<(), ConfigurationError> {
    if value.trim().is_empty()
        || value.chars().count() > MAX_NAME_CHARS
        || value.chars().any(char::is_control)
    {
        return Err(invalid(format!(
            "{what} must contain 1 to {MAX_NAME_CHARS} characters without controls"
        )));
    }
    let bare = value.rsplit_once('@').map_or(value, |(name, _)| name);
    if RESERVED.contains(&normalize_room(bare).as_str()) {
        return Err(invalid(format!(
            "{what} cannot be a reserved target (all, everywhere or here)"
        )));
    }
    Ok(())
}

fn client_ok(client: Option<&str>) -> Result<(), ConfigurationError> {
    if client.is_some_and(|client| {
        client.trim().is_empty()
            || client.len() > MAX_CLIENT_BYTES
            || client.chars().any(char::is_control)
    }) {
        return Err(invalid(format!(
            "client identity must be nonblank, at most {MAX_CLIENT_BYTES} bytes and contain no controls"
        )));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasesSnapshot {
    pub aliases: Aliases,
    /// Exact-byte equality token, including whether the file exists. This is
    /// not a credential; callers must independently authorize configuration edits.
    pub revision: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AliasesSaved {
    pub changed: bool,
    pub snapshot: AliasesSnapshot,
}

/// Distinguish a refused or failed write from a rename that became visible
/// before directory durability could be confirmed.
#[derive(Debug, thiserror::Error)]
pub enum ConfigurationError {
    #[error("aliases.toml changed since it was read; reload before saving")]
    Conflict { current_revision: String },
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Parse(#[from] AliasError),
    #[error("could not read room configuration {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not lock room configuration {}: {source}", path.display())]
    Lock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not stage room configuration {}: {source}", path.display())]
    Stage {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not confirm room configuration publication {}: {source}", path.display())]
    Publish {
        path: PathBuf,
        #[source]
        source: io::Error,
        /// Rename succeeded; read the selected configuration before deciding
        /// on another write. Retrying this edit automatically is unsafe.
        published: bool,
    },
}

fn invalid(message: impl Into<String>) -> ConfigurationError {
    ConfigurationError::Invalid(message.into())
}

fn check_size(bytes: &[u8]) -> Result<(), ConfigurationError> {
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(invalid(format!(
            "room configuration supports at most {MAX_CONFIG_BYTES} encoded bytes"
        )));
    }
    Ok(())
}

/// Read all aliases and explicit defaults with an exact observed revision.
/// A missing file has no overrides and a different revision from an empty file.
pub fn read(path: &Path) -> Result<AliasesSnapshot, ConfigurationError> {
    snapshot(read_bytes(path)?.as_deref())
}

/// Compare and edit under a stable sibling lock, including separate processes.
/// The returned snapshot is parsed before publication, so a successful rename
/// does not depend on a later configuration read to produce its result.
///
/// All application writers must use this boundary. External text editors need
/// not honor the lock: edits visible before comparison invalidate the revision,
/// but a filesystem cannot compare-and-rename against an uncooperative writer.
pub fn apply(
    path: &Path,
    expected_revision: &str,
    change: &AliasEdit,
) -> Result<AliasesSaved, ConfigurationError> {
    change.validate()?;
    if expected_revision.trim().is_empty()
        || expected_revision.len() > 4096
        || expected_revision.chars().any(char::is_control)
    {
        return Err(invalid("room configuration requires its observed revision"));
    }
    let _lock = lock(path)?;
    let bytes = read_bytes(path)?;
    let current_revision = revision(bytes.as_deref());
    if current_revision != expected_revision {
        return Err(ConfigurationError::Conflict { current_revision });
    }
    let current = snapshot(bytes.as_deref())?;
    let before = text(bytes.as_deref().unwrap_or_default())?;
    let after = edit(before, change)?;
    if before == after {
        return Ok(AliasesSaved {
            changed: false,
            snapshot: current,
        });
    }
    let accepted = snapshot(Some(after.as_bytes()))?;
    publish(path, after.as_bytes())?;
    Ok(AliasesSaved {
        changed: true,
        snapshot: accepted,
    })
}

fn text(bytes: &[u8]) -> Result<&str, ConfigurationError> {
    std::str::from_utf8(bytes)
        .map_err(|error| invalid(format!("aliases.toml is not UTF-8: {error}")))
}

fn snapshot(bytes: Option<&[u8]>) -> Result<AliasesSnapshot, ConfigurationError> {
    let content = bytes.unwrap_or_default();
    check_size(content)?;
    Ok(AliasesSnapshot {
        aliases: Aliases::parse(text(content)?)?,
        revision: revision(bytes),
    })
}

fn read_bytes(path: &Path) -> Result<Option<Vec<u8>>, ConfigurationError> {
    let file = match File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(source) => {
            return Err(ConfigurationError::Read {
                path: path.to_owned(),
                source,
            });
        }
    };
    let mut bytes = Vec::new();
    file.take(MAX_CONFIG_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|source| ConfigurationError::Read {
            path: path.to_owned(),
            source,
        })?;
    check_size(&bytes)?;
    Ok(Some(bytes))
}

fn revision(bytes: Option<&[u8]>) -> String {
    let Some(bytes) = bytes else {
        return "aliases-v1-missing".into();
    };
    let digest = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    });
    format!("aliases-v1-{:x}-{digest:016x}", bytes.len())
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn lock(path: &Path) -> Result<File, ConfigurationError> {
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
    acquire().map_err(|source| ConfigurationError::Lock {
        path: lock_path,
        source,
    })
}

fn publish(path: &Path, bytes: &[u8]) -> Result<(), ConfigurationError> {
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
                return Err(ConfigurationError::Stage {
                    path: temporary,
                    source,
                });
            }
        };
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(source) = written {
            let _ = fs::remove_file(&temporary);
            return Err(ConfigurationError::Stage {
                path: temporary,
                source,
            });
        }
        if let Err(source) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(ConfigurationError::Publish {
                path: path.to_owned(),
                source,
                published: false,
            });
        }
        #[cfg(unix)]
        File::open(parent(path))
            .and_then(|directory| directory.sync_all())
            .map_err(|source| ConfigurationError::Publish {
                path: path.to_owned(),
                source,
                published: true,
            })?;
        return Ok(());
    }
    Err(ConfigurationError::Stage {
        path: path.to_owned(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a room-configuration staging file",
        ),
    })
}

#[cfg(test)]
mod tests;
