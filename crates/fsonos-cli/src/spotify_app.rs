//! The Spotify app the DJ reads the owner's library with. Its client id
//! identifies the app (PKCE has no client secret), so it is configuration,
//! not a credential: `--spotify-client-id` / `FSONOS_SPOTIFY_CLIENT_ID`
//! name it, else `spotify.toml` in the data directory, which `fsonos setup`
//! writes. Kept there, a daemon restarted by hand, by launchd or after a
//! reboot still has it (a variable set in one shell is easily lost).

use std::path::Path;

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

/// Save `client_id` as the data directory's Spotify app.
pub fn save(data_dir: &Path, client_id: &str) -> std::io::Result<()> {
    std::fs::create_dir_all(data_dir)?;
    let id = toml::Value::String(client_id.trim().to_owned());
    std::fs::write(
        data_dir.join(FILE_NAME),
        format!(
            "# The Spotify app fsonos reads your library with (written by `fsonos setup`).\n\
             # A client id names the app; it is not a secret.\nclient_id = {id}\n"
        ),
    )
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
}
