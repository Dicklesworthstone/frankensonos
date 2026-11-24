//! The Spotify app the DJ reads the owner's library with. Its client id
//! identifies the app (PKCE has no client secret), so it is configuration,
//! not a credential: `--spotify-client-id` / `FSONOS_SPOTIFY_CLIENT_ID`
//! name it, else `spotify.toml` in the data directory, which `fsonos setup`
//! writes. Kept there, a daemon restarted by hand, by launchd or after a
//! reboot still has it (a variable set in one shell is easily lost).

use std::fs::{self, OpenOptions};
use std::io::{self, Write as _};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

use fsonos_api::{ErrorCode, Failure};
use fsonos_spotify::client::{SpotifyConfig, TokenCache};

/// The file in the data directory.
pub const FILE_NAME: &str = "spotify.toml";

/// Where an owner creates the app.
pub const DASHBOARD: &str = "https://developer.spotify.com/dashboard";

/// The client id `flag` names (the option or variable), else the one saved
/// in `data_dir`.
#[must_use]
pub fn client_id(flag: Option<&str>, data_dir: &Path) -> Option<String> {
    flag.map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
        .or_else(|| saved(data_dir))
}

/// The client id saved in `data_dir`, when there is a plausible one.
#[must_use]
pub fn saved(data_dir: &Path) -> Option<String> {
    let text = std::fs::read_to_string(data_dir.join(FILE_NAME)).ok()?;
    let table: toml::Table = text.parse().ok()?;
    let id = table.get("client_id")?.as_str()?.trim();
    plausible(id).then(|| id.to_owned())
}

/// Startup overrides retain precedence; otherwise new operations pick up the
/// app saved after the daemon started.
#[derive(Debug, Clone)]
pub struct Settings {
    pub client_id: Option<String>,
    pub redirect_uri: String,
}

impl From<SpotifyConfig> for Settings {
    fn from(config: SpotifyConfig) -> Self {
        Self {
            client_id: Some(config.client_id),
            redirect_uri: config.redirect_uri,
        }
    }
}

impl Settings {
    pub fn resolve(&self, data_dir: &Path) -> Result<SpotifyConfig, Failure> {
        let client_id = client_id(self.client_id.as_deref(), data_dir).ok_or_else(|| {
            Failure::new(
                ErrorCode::SpotifyAuthRequired,
                "no Spotify app is configured yet",
            )
            .with_hint("Configure the app with fsonos setup or the daemon's Spotify sign-in panel.")
        })?;
        let config = SpotifyConfig {
            client_id,
            redirect_uri: self.redirect_uri.clone(),
        };
        config
            .validate()
            .map_err(|error| Failure::invalid(error.to_string()))?;
        Ok(config)
    }

    /// Initialize the app, never switch an existing grant to a different app.
    /// Repeating the same configuration is harmless, including after a lost reply.
    pub fn configure(&self, data_dir: &Path, requested: &str) -> Result<(), Failure> {
        let requested = requested.trim();
        if !plausible(requested) {
            return Err(Failure::invalid(
                "the Spotify app client id must contain exactly 32 hexadecimal digits",
            ));
        }
        SpotifyConfig {
            client_id: requested.to_owned(),
            redirect_uri: self.redirect_uri.clone(),
        }
        .validate()
        .map_err(|error| Failure::invalid(error.to_string()))?;
        if let Some(active) = self
            .client_id
            .as_deref()
            .map(str::trim)
            .filter(|id| !id.is_empty())
        {
            return same_app(active, requested);
        }
        save(data_dir, requested)
    }
}

fn same_app(active: &str, requested: &str) -> Result<(), Failure> {
    if active == requested {
        Ok(())
    } else {
        Err(Failure::invalid(
            "a different Spotify app is already configured; initial setup cannot replace it",
        )
        .with_hint("Keep the existing app. Changing an app requires deliberate host-side configuration and a new sign-in."))
    }
}

static NEXT_WRITE: AtomicU64 = AtomicU64::new(0);

/// Save an initial app under one stable lock shared by setup and the daemon.
/// A same-id retry preserves the file, including comments and unrelated settings.
pub fn save(data_dir: &Path, client_id: &str) -> Result<(), Failure> {
    let client_id = client_id.trim();
    if !plausible(client_id) {
        return Err(Failure::invalid(
            "the Spotify app client id must contain exactly 32 hexadecimal digits",
        ));
    }
    let io_failure = |error: io::Error| {
        Failure::new(
            ErrorCode::Internal,
            format!("save the Spotify app: {error}"),
        )
    };
    fs::create_dir_all(data_dir).map_err(io_failure)?;
    let lock = OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(data_dir.join("spotify.toml.lock"))
        .map_err(io_failure)?;
    lock.lock().map_err(io_failure)?;
    let path = data_dir.join(FILE_NAME);
    let previous = match fs::read_to_string(&path) {
        Ok(text) => {
            let table: toml::Table = text.parse().map_err(|_| {
                Failure::invalid("spotify.toml is malformed; fix it before configuring an app")
            })?;
            if let Some(active) = table.get("client_id") {
                let active = active.as_str().map(str::trim).filter(|id| plausible(id)).ok_or_else(|| {
                    Failure::invalid("spotify.toml contains an invalid client_id; fix it before configuring an app")
                })?;
                return same_app(active, client_id);
            }
            text
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => String::new(),
        Err(error) => return Err(io_failure(error)),
    };
    // Old token files do not record which app issued them. Do not silently bind
    // an orphaned grant to a newly entered app.
    if TokenCache::in_data_dir(data_dir)
        .load()
        .map_err(|error| {
            Failure::new(
                ErrorCode::Internal,
                format!("read the Spotify sign-in: {error}"),
            )
        })?
        .is_some()
    {
        return Err(
            Failure::invalid("a cached Spotify sign-in has no configured app").with_hint(
                "Restore the original spotify.toml on the daemon host before using this sign-in.",
            ),
        );
    }
    let value = toml::Value::String(client_id.to_owned());
    let text = format!(
        "# The Spotify app used for read-only library access.\nclient_id = {value}\n{previous}"
    );
    let staged = data_dir.join(format!(
        "spotify.toml.{}.{}.tmp",
        std::process::id(),
        NEXT_WRITE.fetch_add(1, Ordering::Relaxed)
    ));
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&staged)
        .map_err(io_failure)?;
    file.write_all(text.as_bytes()).map_err(io_failure)?;
    file.sync_all().map_err(io_failure)?;
    drop(file);
    fs::rename(staged, path).map_err(io_failure)?;
    #[cfg(unix)]
    fs::File::open(data_dir)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            Failure::new(
                ErrorCode::Internal,
                format!("the Spotify app was saved but directory durability was not confirmed: {error}"),
            )
            .with_hint("Read Spotify sign-in status before retrying; the same app can be configured again safely.")
        })?;
    Ok(())
}

/// Whether `id` looks like a Spotify client id: 32 hexadecimal digits.
#[must_use]
pub fn plausible(id: &str) -> bool {
    id.len() == 32 && id.bytes().all(|b| b.is_ascii_hexdigit())
}

#[cfg(test)]
mod tests {
    use super::*;

    const APP: &str = "0123456789abcdef0123456789ABCDEF";

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir =
            std::env::temp_dir().join(format!("fsonos-spotify-app-{name}-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn a_saved_app_is_read_back_and_the_option_still_wins() {
        let dir = scratch("saved");
        assert_eq!(client_id(None, &dir), None);
        save(&dir, &format!("  {APP}\n")).unwrap();
        assert_eq!(saved(&dir).as_deref(), Some(APP));
        assert_eq!(client_id(None, &dir).as_deref(), Some(APP));
        assert_eq!(client_id(Some(""), &dir).as_deref(), Some(APP));
        let other = "fedcba9876543210fedcba9876543210";
        assert_eq!(client_id(Some(other), &dir).as_deref(), Some(other));
    }

    #[test]
    fn a_file_without_a_plausible_id_names_no_app() {
        let dir = scratch("implausible");
        for text in [
            "client_id = \"not-an-id\"\n",
            "client_id = 12\n",
            "clientid = \"0123456789abcdef0123456789abcdef\"\n",
            "this is not toml = = \n",
        ] {
            std::fs::write(dir.join(FILE_NAME), text).unwrap();
            assert_eq!(saved(&dir), None, "{text}");
        }
        assert!(plausible(APP));
        assert!(!plausible(&APP[1..]));
    }

    #[test]
    fn existing_settings_pick_up_first_configuration_without_reconstruction() {
        let dir = tempfile::tempdir().unwrap();
        let settings = Settings {
            client_id: None,
            redirect_uri: crate::config::SPOTIFY_REDIRECT_URI.into(),
        };
        assert!(settings.resolve(dir.path()).is_err());
        settings.configure(dir.path(), APP).unwrap();
        assert_eq!(settings.resolve(dir.path()).unwrap().client_id, APP);
        let before = fs::read(dir.path().join(FILE_NAME)).unwrap();
        settings.configure(dir.path(), APP).unwrap();
        assert_eq!(fs::read(dir.path().join(FILE_NAME)).unwrap(), before);
        assert!(
            settings
                .configure(dir.path(), "fedcba9876543210fedcba9876543210")
                .is_err()
        );
        assert_eq!(saved(dir.path()).as_deref(), Some(APP));

        let override_id = "fedcba9876543210fedcba9876543210";
        let explicit = Settings {
            client_id: Some(override_id.into()),
            redirect_uri: settings.redirect_uri,
        };
        assert_eq!(explicit.resolve(dir.path()).unwrap().client_id, override_id);
        explicit.configure(dir.path(), override_id).unwrap();
        assert!(explicit.configure(dir.path(), APP).is_err());
        assert_eq!(fs::read(dir.path().join(FILE_NAME)).unwrap(), before);
    }

    #[test]
    fn simultaneous_initial_configurations_never_replace_each_other() {
        let dir = tempfile::tempdir().unwrap();
        let barrier = std::sync::Barrier::new(2);
        let ids = [APP, "fedcba9876543210fedcba9876543210"];
        let accepted = std::thread::scope(|scope| {
            let workers: Vec<_> = ids
                .iter()
                .map(|id| {
                    let barrier = &barrier;
                    let path = dir.path();
                    scope.spawn(move || {
                        barrier.wait();
                        save(path, id).is_ok()
                    })
                })
                .collect();
            workers
                .into_iter()
                .map(|worker| worker.join().unwrap())
                .filter(|accepted| *accepted)
                .count()
        });
        assert_eq!(accepted, 1);
        assert!(ids.contains(&saved(dir.path()).unwrap().as_str()));
    }

    #[test]
    fn initial_setup_preserves_malformed_files_and_unbound_grants() {
        let dir = tempfile::tempdir().unwrap();
        let malformed = "client_id = [\n";
        fs::write(dir.path().join(FILE_NAME), malformed).unwrap();
        assert!(save(dir.path(), APP).is_err());
        assert_eq!(
            fs::read_to_string(dir.path().join(FILE_NAME)).unwrap(),
            malformed
        );

        let unbound = tempfile::tempdir().unwrap();
        TokenCache::in_data_dir(unbound.path())
            .store(&fsonos_spotify::client::CachedToken {
                access_token: "fixture-access".into(),
                refresh_token: "fixture-refresh".into(),
                expires_at: i64::MAX,
                scope: fsonos_spotify::client::SCOPE.into(),
            })
            .unwrap();
        let error = save(unbound.path(), APP).unwrap_err();
        assert!(error.detail.contains("cached Spotify sign-in"));
        assert!(!unbound.path().join(FILE_NAME).exists());
    }

    #[test]
    fn an_idempotent_configuration_preserves_owner_comments_and_settings() {
        let dir = tempfile::tempdir().unwrap();
        let text =
            format!("# Keep this comment\nclient_id = \"{APP}\"\n[owner]\nlabel = \"music\"\n");
        fs::write(dir.path().join(FILE_NAME), &text).unwrap();
        save(dir.path(), APP).unwrap();
        assert_eq!(
            fs::read_to_string(dir.path().join(FILE_NAME)).unwrap(),
            text
        );
    }
}
