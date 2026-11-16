//! Serialized preference edits and atomic publication, shared by direct CLI
//! processes and the daemon. The stable lock survives replacement of the TOML
//! inode; unique staging files cannot overwrite another writer's unfinished work.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use super::{Moods, Preferences, SpotifyError};

static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);

/// Keep a failed edit distinct from a published file whose directory durability
/// could not be confirmed. Callers must observe an uncertain result before retrying.
#[derive(Debug, thiserror::Error)]
pub enum PreferenceWriteError {
    #[error(transparent)]
    Spotify(#[from] SpotifyError),
    #[error("could not lock preferences {}: {source}", path.display())]
    Lock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not stage preferences {}: {source}", path.display())]
    Stage {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not confirm preference publication {}: {source}", path.display())]
    Publish {
        path: PathBuf,
        #[source]
        source: io::Error,
        /// Rename succeeded; only directory durability could not be confirmed.
        published: bool,
    },
}

pub(super) fn save(preferences: &Preferences, path: &Path) -> Result<(), PreferenceWriteError> {
    let text = preferences.to_toml()?;
    let _lock = lock(path)?;
    publish(path, text.as_bytes())
}

pub(super) fn edit<R>(
    path: &Path,
    moods: &Moods,
    change: impl FnOnce(&mut Preferences) -> Result<R, String>,
) -> Result<(Preferences, R), PreferenceWriteError> {
    let _lock = lock(path)?;
    let mut preferences = Preferences::load(path, moods)?;
    let before = preferences.clone();
    let result = change(&mut preferences).map_err(SpotifyError::Config)?;
    preferences.validate(moods).map_err(SpotifyError::Config)?;
    if preferences != before {
        publish(path, preferences.to_toml()?.as_bytes())?;
    }
    Ok((preferences, result))
}

fn parent(path: &Path) -> &Path {
    path.parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .unwrap_or_else(|| Path::new("."))
}

fn lock(path: &Path) -> Result<File, PreferenceWriteError> {
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
    acquire().map_err(|source| PreferenceWriteError::Lock {
        path: lock_path,
        source,
    })
}

fn publish(path: &Path, bytes: &[u8]) -> Result<(), PreferenceWriteError> {
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
                return Err(PreferenceWriteError::Stage {
                    path: temporary,
                    source,
                });
            }
        };
        let written = file.write_all(bytes).and_then(|()| file.sync_all());
        drop(file);
        if let Err(source) = written {
            let _ = fs::remove_file(&temporary);
            return Err(PreferenceWriteError::Stage {
                path: temporary,
                source,
            });
        }
        if let Err(source) = fs::rename(&temporary, path) {
            let _ = fs::remove_file(&temporary);
            return Err(PreferenceWriteError::Publish {
                path: path.to_owned(),
                source,
                published: false,
            });
        }
        #[cfg(unix)]
        File::open(parent(path))
            .and_then(|directory| directory.sync_all())
            .map_err(|source| PreferenceWriteError::Publish {
                path: path.to_owned(),
                source,
                published: true,
            })?;
        return Ok(());
    }
    Err(PreferenceWriteError::Stage {
        path: path.to_owned(),
        source: io::Error::new(
            io::ErrorKind::AlreadyExists,
            "could not allocate a preference staging file",
        ),
    })
}

#[cfg(test)]
mod tests;
