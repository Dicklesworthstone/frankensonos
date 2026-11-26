//! Reviewed, owner-authorized edits of policy.toml without speaker I/O.
//! The exact observed revision and current on-disk permissions are checked
//! under one stable lock. Invalid files are preserved, never replaced by defaults.

use super::{Client, Decision, McpProfile, Policy, PolicyError, ToolCall};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read as _, Write as _};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

mod edit;
pub use edit::edit;

pub const MAX_CONFIG_BYTES: usize = 256 * 1024;
pub const MAX_NAME_CHARS: usize = 128;
pub const MAX_CLIENT_BYTES: usize = 512;
pub const MAX_RULES: usize = 256;
static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);

/// One complete policy section or one exact room/client rule. Absent optional
/// fields restore inheritance; an empty allow list deliberately denies all tools.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyEdit {
    SetDefaults {
        max_volume: u8,
        max_step: u8,
        fade_secs: u32,
    },
    SetQuietHours {
        start: String,
        end: String,
        max_volume: u8,
    },
    ClearQuietHours,
    SetRoom {
        room: String,
        max_volume: Option<u8>,
        max_step: Option<u8>,
    },
    RemoveRoom {
        room: String,
    },
    SetClient {
        client: String,
        allow: Option<Vec<String>>,
        deny: Vec<String>,
        grant: Vec<String>,
        capped: Option<bool>,
        manage_others: bool,
        mcp_profile: Option<McpProfile>,
    },
    RemoveClient {
        client: String,
    },
}

impl PolicyEdit {
    pub fn validate(&self) -> Result<(), ConfigurationError> {
        let volume = |value: u8| {
            if value > 100 {
                Err(invalid("volume limits must be between 0 and 100"))
            } else {
                Ok(())
            }
        };
        match self {
            Self::SetDefaults {
                max_volume,
                max_step,
                ..
            } => {
                volume(*max_volume)?;
                volume(*max_step)?;
            }
            Self::SetQuietHours {
                start,
                end,
                max_volume,
            } => {
                let time = |value: &str| {
                    value.len() == 5
                        && value.as_bytes()[2] == b':'
                        && chrono::NaiveTime::parse_from_str(value, "%H:%M").is_ok()
                };
                if !time(start) || !time(end) || start == end {
                    return Err(invalid(
                        "quiet hours need different start/end times in HH:MM form; clear quiet hours to disable them",
                    ));
                }
                volume(*max_volume)?;
            }
            Self::ClearQuietHours => {}
            Self::SetRoom {
                room,
                max_volume,
                max_step,
            } => {
                name(room, MAX_NAME_CHARS, "room name")?;
                for value in max_volume.iter().chain(max_step.iter()) {
                    volume(*value)?;
                }
            }
            Self::RemoveRoom { room } => name(room, MAX_NAME_CHARS, "room name")?,
            Self::SetClient {
                client,
                allow,
                deny,
                grant,
                ..
            } => {
                client_name(client)?;
                for rules in allow.iter().chain([deny, grant]) {
                    if rules.len() > MAX_RULES {
                        return Err(invalid("a policy rule list exceeds 256 entries"));
                    }
                    for rule in rules {
                        name(rule, MAX_NAME_CHARS, "tool or class")?;
                        if !rule
                            .bytes()
                            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
                        {
                            return Err(invalid(
                                "tools and classes must use letters, digits, underscores or hyphens",
                            ));
                        }
                    }
                }
            }
            Self::RemoveClient { client } => client_name(client)?,
        }
        Ok(())
    }
}

fn name(value: &str, limit: usize, what: &str) -> Result<(), ConfigurationError> {
    if value.trim().is_empty()
        || value.chars().count() > limit
        || value.chars().any(char::is_control)
    {
        return Err(invalid(format!(
            "{what} must contain 1 to {limit} characters without controls"
        )));
    }
    Ok(())
}

fn client_name(value: &str) -> Result<(), ConfigurationError> {
    name(value, MAX_CLIENT_BYTES, "client key")?;
    if value.len() > MAX_CLIENT_BYTES {
        return Err(invalid("client keys support at most 512 encoded bytes"));
    }
    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySnapshot {
    pub revision: String,
    pub policy: Policy,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PolicySaved {
    pub changed: bool,
    pub snapshot: PolicySnapshot,
}

#[derive(Debug, thiserror::Error)]
pub enum ConfigurationError {
    #[error("policy.toml changed since it was read; reload before saving")]
    Conflict { current_revision: String },
    #[error("{0}")]
    Invalid(String),
    #[error(transparent)]
    Parse(#[from] PolicyError),
    #[error("policy edit denied: {0}")]
    Denied(String),
    #[error("could not read house policy {}: {source}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not lock house policy {}: {source}", path.display())]
    Lock {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not stage house policy {}: {source}", path.display())]
    Stage {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("could not confirm house policy publication {}: {source}", path.display())]
    Publish {
        path: PathBuf,
        #[source]
        source: io::Error,
        published: bool,
    },
}

fn invalid(message: impl Into<String>) -> ConfigurationError {
    ConfigurationError::Invalid(message.into())
}

fn check_size(bytes: &[u8]) -> Result<(), ConfigurationError> {
    if bytes.len() > MAX_CONFIG_BYTES {
        return Err(invalid("policy.toml supports at most 256 KiB"));
    }
    Ok(())
}

/// Authorize independently of the content the caller proposes to install.
/// A grant to use configure_policy does not itself confer owner administration.
pub fn authorize(policy: &Policy, client: &Client) -> Result<(), String> {
    for decision in [
        policy.authorize(
            ToolCall {
                name: "configure_policy",
                read_only: false,
            },
            client,
        ),
        policy.may_change(client, None),
    ] {
        if let Decision::Deny { reason } = decision {
            return Err(reason);
        }
    }
    Ok(())
}

pub fn read(path: &Path) -> Result<PolicySnapshot, ConfigurationError> {
    snapshot(read_bytes(path)?.as_deref())
}

/// All application writers share this lock. The trusted surface supplies the
/// owner identity; it is never read from a proposed policy or request body.
/// Uncooperative external editors remain outside this filesystem transaction.
pub fn apply(
    path: &Path,
    expected_revision: &str,
    change: &PolicyEdit,
    authorize: impl FnOnce(&Policy) -> Result<(), String>,
) -> Result<PolicySaved, ConfigurationError> {
    change.validate()?;
    if expected_revision.trim().is_empty()
        || expected_revision.len() > 4096
        || expected_revision.chars().any(char::is_control)
    {
        return Err(invalid("house policy requires its observed revision"));
    }
    let _lock = lock(path)?;
    let bytes = read_bytes(path)?;
    let current = snapshot(bytes.as_deref())?;
    authorize(&current.policy).map_err(ConfigurationError::Denied)?;
    if current.revision != expected_revision {
        return Err(ConfigurationError::Conflict {
            current_revision: current.revision,
        });
    }
    let before = text(bytes.as_deref().unwrap_or_default())?;
    let after = edit(before, change)?;
    if before == after {
        return Ok(PolicySaved {
            changed: false,
            snapshot: current,
        });
    }
    let accepted = snapshot(Some(after.as_bytes()))?;
    publish(path, after.as_bytes())?;
    Ok(PolicySaved {
        changed: true,
        snapshot: accepted,
    })
}

fn text(bytes: &[u8]) -> Result<&str, ConfigurationError> {
    std::str::from_utf8(bytes)
        .map_err(|error| invalid(format!("policy.toml is not UTF-8: {error}")))
}

fn snapshot(bytes: Option<&[u8]>) -> Result<PolicySnapshot, ConfigurationError> {
    let content = bytes.unwrap_or_default();
    check_size(content)?;
    Ok(PolicySnapshot {
        revision: revision(bytes),
        policy: Policy::from_toml(text(content)?)?,
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
        return "policy-v1-missing".into();
    };
    let digest = bytes.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x100_0000_01b3)
    });
    format!("policy-v1-{:x}-{digest:016x}", bytes.len())
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
            "could not allocate a policy-configuration staging file",
        ),
    })
}

#[cfg(test)]
mod tests;
